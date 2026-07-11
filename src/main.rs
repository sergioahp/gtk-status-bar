// Entry point: bring up tracing, the tokio runtime, the GTK application, and
// wire each subsystem's GUI fan-out to its widget. Each setup_*_updates owns
// (a) creating the channel, (b) installing the sender into the global bus, and
// (c) spawning the producer task (which lives in the corresponding subsystem
// module). Producer crashes are logged but currently not retried — see TODO in
// each subsystem's setup_*_event_listener.

mod bus;
mod dbus;
mod hypr;
mod pw;
mod tray;
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

    widgets::update_time_widget(time_widget);
    widgets::setup_workspace_updates(workspace_widget, title_widget.clone())?;
    widgets::setup_title_updates(title_widget)?;
    widgets::setup_battery_updates(battery_widget)?;
    widgets::setup_bluetooth_updates(bt_widget)?;
    widgets::setup_volume_updates(volume_widget)?;

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
        if let Err(e) = activate(app) {
            error!("Application activation failed: {}", e);
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
