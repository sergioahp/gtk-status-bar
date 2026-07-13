// Entry point: bring up tracing, the tokio runtime, the GTK application, and
// wire each subsystem's GUI fan-out to its widget. activate() creates one Bus
// plus the bidirectional tray endpoints, hands every UI half to its widget
// drain, and only then spawns the supervised producers. Every consumer is
// therefore wired before the first producer can send. Producers that crash are
// restarted with exponential backoff by their run_*_supervised wrappers.

mod bus;
mod dbus;
mod hypr;
mod pw;
mod tray;
mod widgets;

use std::env;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use gio::prelude::*;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tray_ipc::IpcUiRequest;

const USAGE: &str = "Usage: gtk-status-bar [--monitor CONNECTOR]\n\n\
CONNECTOR is the GDK output connector name, such as DVI-I-1 or DP-1. Without\n\
--monitor, the compositor chooses the output.";

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    monitor: Option<String>,
}

enum CliAction {
    Run(CliOptions),
    Help,
}

fn parse_cli(arguments: &[String]) -> Result<CliAction> {
    match arguments {
        [] => Ok(CliAction::Run(CliOptions { monitor: None })),
        [flag] if flag == "--help" || flag == "-h" => Ok(CliAction::Help),
        [flag, connector] if flag == "--monitor" && !connector.is_empty() => {
            Ok(CliAction::Run(CliOptions {
                monitor: Some(connector.clone()),
            }))
        }
        [flag] if flag == "--monitor" => bail!("--monitor requires a CONNECTOR\n\n{USAGE}"),
        _ => bail!("unknown or malformed arguments\n\n{USAGE}"),
    }
}

fn setup_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

async fn run_tray_ipc_supervised(ui_tx: mpsc::UnboundedSender<IpcUiRequest>) {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("Starting tray IPC server");
        if let Err(error) = tray_ipc::run_server(ui_tx.clone()).await {
            warn!(%error, "Tray IPC server stopped");
        }

        if started.elapsed() >= reset_threshold {
            debug!(
                elapsed = ?started.elapsed(),
                "Tray IPC server was stable; resetting restart backoff"
            );
            delay = Duration::from_secs(1);
        }

        warn!(restart_delay = ?delay, "Restarting tray IPC server");
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

fn activate(application: &gtk4::Application, monitor: Option<&str>) -> Result<()> {
    info!("Activating GTK application");

    let window = gtk4::ApplicationWindow::new(application);
    window.add_css_class("layer-bar");

    widgets::load_css_styles(&window);
    widgets::configure_layer_shell(&window, monitor)?;

    let (
        bar,
        tray_widget,
        bt_widget,
        volume_widget,
        battery_widget,
        time_widget,
        workspace_widget,
        title_widget,
    ) = widgets::create_experimental_bar();
    window.set_child(Some(&bar));
    window.show();

    let (bus, receivers) = bus::Bus::new();
    let (tray_backend, tray_ui) = tray::channels();
    let (tray_ipc_tx, tray_ipc_rx) = mpsc::unbounded_channel();

    widgets::update_time_widget(time_widget);
    widgets::setup_tray_updates(tray_ui, tray_ipc_rx, tray_widget, &window);
    widgets::setup_workspace_updates(receivers.workspace, workspace_widget, title_widget.clone());
    widgets::setup_title_updates(receivers.title, title_widget);
    widgets::setup_battery_updates(receivers.battery, battery_widget);
    widgets::setup_bluetooth_updates(receivers.bluetooth, bt_widget);
    widgets::setup_volume_updates(volume_widget)?;

    // Every consumer above is wired before any producer below spawns. The
    // D-Bus monitor serves both battery and bluetooth, while the tray also has
    // a UI-to-backend command channel; both still obey the same ordering.
    tokio::spawn(hypr::run_workspace_listener_supervised(bus.clone()));
    tokio::spawn(hypr::run_title_listener_supervised(bus.clone()));
    tokio::spawn(dbus::run_dbus_monitor_supervised(bus));
    tokio::spawn(tray::run_tray_supervised(tray_backend));
    tokio::spawn(run_tray_ipc_supervised(tray_ipc_tx));

    info!("Application activated successfully");
    Ok(())
}

fn create_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Runtime::new().context("Failed to create Tokio runtime")
}

fn main() -> Result<()> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let options = match parse_cli(&arguments)? {
        CliAction::Run(options) => options,
        CliAction::Help => {
            println!("{USAGE}");
            return Ok(());
        }
    };

    setup_logging();
    info!("Starting GTK status bar application");

    let rt = create_tokio_runtime()?;
    let _guard = rt.enter();

    let application = gtk4::Application::new(Some("sh.wmww.gtk-layer-example"), Default::default());

    application.connect_activate(move |app| {
        // GApplication re-fires activate on the primary instance when a second
        // copy of the binary launches under the same application id. Rebuilding
        // the bar would double-spawn every producer, so present the existing
        // window instead. Previously this path failed the OnceLock sender init
        // and exit(1)'d the healthy bar.
        if let Some(window) = app.active_window() {
            info!("Already activated; presenting existing window");
            window.present();
            return;
        }
        if let Err(e) = activate(app, options.monitor.as_deref()) {
            error!("Application activation failed: {:#}", e);
            std::process::exit(1);
        }
    });

    info!("Running GTK application");
    application.run_with_args(&["gtk-status-bar"]);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn monitor_is_optional() {
        let CliAction::Run(options) = parse_cli(&[]).expect("empty arguments should parse") else {
            panic!("empty arguments unexpectedly requested help");
        };
        assert_eq!(options, CliOptions { monitor: None });
    }

    #[test]
    fn parses_monitor_connector() {
        let CliAction::Run(options) =
            parse_cli(&arguments(&["--monitor", "DVI-I-1"])).expect("monitor should parse")
        else {
            panic!("monitor arguments unexpectedly requested help");
        };
        assert_eq!(
            options,
            CliOptions {
                monitor: Some("DVI-I-1".to_string())
            }
        );
    }

    #[test]
    fn rejects_monitor_without_connector() {
        let error = parse_cli(&arguments(&["--monitor"]))
            .err()
            .expect("missing connector should fail");
        assert!(error.to_string().contains("requires a CONNECTOR"));
    }
}
