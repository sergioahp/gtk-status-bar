// Entry point: bring up tracing, the tokio runtime, the GTK application, and
// wire each subsystem's GUI fan-out to its widget. activate() creates one Bus,
// hands each receiver to its widget drain (setup_*_updates), and only then
// spawns the supervised producer tasks with Bus clones — so every consumer is
// wired before the first producer can send. Producers that crash are restarted
// with exponential backoff by their run_*_supervised wrappers.

mod bus;
mod dbus;
mod hypr;
mod pw;
mod widgets;

use anyhow::{Context, Result};

use gio::prelude::*;
use gtk4::prelude::*;
use tracing::{error, info};

fn setup_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

fn activate(application: &gtk4::Application) -> Result<()> {
    info!("Activating GTK application");

    let window = gtk4::ApplicationWindow::new(application);
    window.add_css_class("layer-bar");

    widgets::load_css_styles(&window);
    widgets::configure_layer_shell(&window);

    let (bar, bt_widget, volume_widget, battery_widget, time_widget, workspace_widget, title_widget) =
        widgets::create_experimental_bar();
    window.set_child(Some(&bar));
    window.show();

    let (bus, receivers) = bus::Bus::new();

    widgets::update_time_widget(time_widget);
    widgets::setup_workspace_updates(receivers.workspace, workspace_widget, title_widget.clone());
    widgets::setup_title_updates(receivers.title, title_widget);
    widgets::setup_battery_updates(receivers.battery, battery_widget);
    widgets::setup_bluetooth_updates(receivers.bluetooth, bt_widget);
    widgets::setup_volume_updates(volume_widget)?;

    // Every consumer above is wired before any producer below spawns; the
    // D-Bus monitor serves both the battery and bluetooth channels, so this
    // ordering is what makes its first sends race-free.
    tokio::spawn(hypr::run_workspace_listener_supervised(bus.clone()));
    tokio::spawn(hypr::run_title_listener_supervised(bus.clone()));
    tokio::spawn(dbus::run_dbus_monitor_supervised(bus));

    info!("Application activated successfully");
    Ok(())
}

fn create_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Runtime::new()
        .context("Failed to create Tokio runtime")
}

fn main() -> Result<()> {
    setup_logging();
    info!("Starting GTK status bar application");

    let rt = create_tokio_runtime()?;
    let _guard = rt.enter();

    let application = gtk4::Application::new(Some("sh.wmww.gtk-layer-example"), Default::default());

    application.connect_activate(|app| {
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
        if let Err(e) = activate(app) {
            error!("Application activation failed: {:#}", e);
            std::process::exit(1);
        }
    });

    info!("Running GTK application");
    application.run();

    // Maybe set up error recovery: exponentially backup retries, currently a failed task will not
    // execute again during the duration of the program
    // Monitor battery status

    Ok(())
}
