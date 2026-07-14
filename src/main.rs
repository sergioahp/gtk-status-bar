// Entry point: bring up tracing, the tokio runtime, the GTK application, and
// wire each subsystem's GUI fan-out to its widget. activate() creates one Bus
// plus the bidirectional tray endpoints, hands every UI half to its widget
// drain, and only then spawns the supervised producers. Every consumer is
// therefore wired before the first producer can send. Producers that crash are
// restarted with exponential backoff by their run_*_supervised wrappers.

mod bus;
mod clock;
mod dbus;
mod hypr;
mod network;
mod pw;
mod tray;
mod widgets;

use std::env;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use gio::prelude::*;
use gtk4::prelude::*;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tray_ipc::IpcUiRequest;

const USAGE: &str = "Usage: gtk-status-bar [OPTIONS]\n\n\
Options:\n\
  --monitor CONNECTOR\n\
  --network-ping-target ADDRESS       Repeat to replace the Cloudflare defaults\n\
  --network-stable-mean-seconds N     Default: 60\n\
  --network-unstable-mean-seconds N   Default: 1\n\
  --network-down-after-seconds N      Default: 15\n\
  --network-recent-window-seconds N   Default: 60\n\
  --network-ping-timeout-seconds N    Default: 2\n\
  -h, --help\n\n\
CONNECTOR is the GDK output connector name, such as DVI-I-1 or DP-1. Ping\n\
targets must be IPv4 or IPv6 addresses.";

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    monitor: Option<String>,
    network: network::NetworkConfig,
}

enum CliAction {
    Run(CliOptions),
    Help,
}

fn parse_cli(arguments: &[String]) -> Result<CliAction> {
    let mut options = CliOptions {
        monitor: None,
        network: network::NetworkConfig::default(),
    };
    let mut custom_targets = Vec::new();
    let mut index = 0;

    while index < arguments.len() {
        let flag = arguments[index].as_str();
        if flag == "--help" || flag == "-h" {
            return Ok(CliAction::Help);
        }
        let Some(value) = arguments.get(index + 1) else {
            if flag == "--monitor" {
                bail!("--monitor requires a CONNECTOR\n\n{USAGE}");
            }
            bail!("{flag} requires a value\n\n{USAGE}");
        };
        match flag {
            "--monitor" if !value.is_empty() => options.monitor = Some(value.clone()),
            "--network-ping-target" => {
                custom_targets.push(value.parse::<IpAddr>().with_context(|| {
                    format!("--network-ping-target requires an IPv4 or IPv6 address: {value}")
                })?);
            }
            "--network-stable-mean-seconds" => {
                options.network.stable_mean = parse_seconds(flag, value)?;
            }
            "--network-unstable-mean-seconds" => {
                options.network.unstable_mean = parse_seconds(flag, value)?;
            }
            "--network-down-after-seconds" => {
                options.network.outage_confirmation = parse_seconds(flag, value)?;
            }
            "--network-recent-window-seconds" => {
                options.network.recent_instability = parse_seconds(flag, value)?;
            }
            "--network-ping-timeout-seconds" => {
                options.network.ping_timeout = parse_seconds(flag, value)?;
            }
            _ => bail!("unknown argument: {flag}\n\n{USAGE}"),
        }
        index += 2;
    }

    if !custom_targets.is_empty() {
        options.network.ping_targets = custom_targets;
    }
    Ok(CliAction::Run(options))
}

fn parse_seconds(flag: &str, value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("{flag} requires a positive integer number of seconds"))?;
    if seconds == 0 {
        bail!("{flag} must be greater than zero");
    }
    Ok(Duration::from_secs(seconds))
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

fn activate(application: &gtk4::Application, options: &CliOptions) -> Result<()> {
    info!("Activating GTK application");

    let window = gtk4::ApplicationWindow::new(application);
    window.add_css_class("layer-bar");

    widgets::load_css_styles(&window);
    widgets::configure_layer_shell(&window, options.monitor.as_deref())?;

    let (
        bar,
        tray_widget,
        bt_widget,
        volume_widget,
        network_widget,
        battery_widget,
        time_widget,
        workspace_widget,
        client_strip,
    ) = widgets::create_experimental_bar();
    window.set_child(Some(&bar));
    window.show();

    let (bus, receivers) = bus::Bus::new();
    let (tray_backend, tray_ui) = tray::channels();
    let (tray_ipc_tx, tray_ipc_rx) = mpsc::unbounded_channel();

    widgets::update_time_widget(time_widget);
    widgets::setup_tray_updates(tray_ui, tray_ipc_rx, tray_widget, &window);
    widgets::setup_workspace_updates(receivers.workspace, workspace_widget);
    widgets::setup_client_updates(receivers.clients, client_strip);
    widgets::setup_battery_updates(receivers.battery, battery_widget);
    widgets::setup_bluetooth_updates(receivers.bluetooth, bt_widget);
    widgets::setup_network_updates(receivers.network, network_widget);
    widgets::setup_volume_updates(volume_widget)?;

    // Every consumer above is wired before any producer below spawns. The
    // D-Bus monitor serves both battery and bluetooth, while the tray also has
    // a UI-to-backend command channel; both still obey the same ordering.
    tokio::spawn(hypr::run_workspace_listener_supervised(bus.clone()));
    tokio::spawn(hypr::run_client_listener_supervised(bus.clone()));
    tokio::spawn(dbus::run_dbus_monitor_supervised(bus.clone()));
    tokio::spawn(network::run_network_monitor_supervised(
        bus,
        options.network.clone(),
    ));
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
        if let Err(e) = activate(app, &options) {
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
        assert_eq!(
            options,
            CliOptions {
                monitor: None,
                network: network::NetworkConfig::default(),
            }
        );
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
                monitor: Some("DVI-I-1".to_string()),
                network: network::NetworkConfig::default(),
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

    #[test]
    fn repeated_ping_targets_replace_defaults_and_timings_parse() {
        let CliAction::Run(options) = parse_cli(&arguments(&[
            "--network-ping-target",
            "192.0.2.1",
            "--network-ping-target",
            "2001:db8::1",
            "--network-stable-mean-seconds",
            "90",
            "--network-down-after-seconds",
            "12",
        ]))
        .expect("network arguments should parse") else {
            panic!("network arguments unexpectedly requested help");
        };
        assert_eq!(
            options.network.ping_targets,
            vec![
                "192.0.2.1".parse::<IpAddr>().unwrap(),
                "2001:db8::1".parse::<IpAddr>().unwrap()
            ]
        );
        assert_eq!(options.network.stable_mean, Duration::from_secs(90));
        assert_eq!(options.network.outage_confirmation, Duration::from_secs(12));
    }

    #[test]
    fn invalid_network_arguments_are_rejected() {
        assert!(parse_cli(&arguments(&["--network-ping-target", "cloudflare"])).is_err());
        assert!(parse_cli(&arguments(&["--network-stable-mean-seconds", "0"])).is_err());
    }
}
