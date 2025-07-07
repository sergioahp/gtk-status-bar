mod error;

use gio::prelude::*;
use gtk::prelude::*;
use gtk::glib;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use chrono::Local;
use tokio::sync::mpsc;
use hyprland::shared::{HyprDataActive, HyprDataActiveOptional};
use hyprland::event_listener::AsyncEventListener;
use hyprland::async_closure;
use std::sync::OnceLock;
use tracing::{info, warn, error, debug};
use error::{AppError, Result};
use zbus::Connection;
use zbus::fdo;
use zbus_names::InterfaceName;
use zbus::message::Type as MessageType;
use zbus::MatchRule;
use futures::StreamExt;

#[derive(Debug, Clone)]
struct WorkspaceUpdate {
    name: String,
    id: hyprland::shared::WorkspaceId,
}

static WORKSPACE_SENDER: OnceLock<mpsc::UnboundedSender<WorkspaceUpdate>> = OnceLock::new();
static TITLE_SENDER: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();
static BATTERY_SENDER: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();

fn setup_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

fn create_workspace_widget() -> Result<gtk::Label> {
    debug!("Creating workspace widget");
    let label = gtk::Label::new(Some("Workspace ?"));
    label.add_css_class("workspace-widget");
    label.set_halign(gtk::Align::Center);
    Ok(label)
}

fn format_workspace_name_from_string(name: &str, id: hyprland::shared::WorkspaceId) -> String {
    if name.is_empty() {
        return format!("Workspace {}", id);
    }
    format!("Workspace {}", name)
}

fn format_workspace_name_from_type(name: &hyprland::shared::WorkspaceType, id: hyprland::shared::WorkspaceId) -> String {
    match name {
        hyprland::shared::WorkspaceType::Regular(name) => {
            format_workspace_name_from_string(name, id)
        }
        hyprland::shared::WorkspaceType::Special(name_opt) => {
            match name_opt {
                Some(name) if !name.is_empty() => format!("Special: {}", name),
                _ => format!("Special {}", id),
            }
        }
    }
}

fn format_title_string(title: String, max_length: usize) -> String {
    if title.chars().count() <= max_length {
        title
    } else {
        // reserve 1 for the …
        let chars_left = (max_length / 2) - 1;
        let chars_right = max_length - chars_left;
        let crop_from_idx = title.char_indices()
            .nth(chars_left)
            .map(|(idx, _)| idx)
            .unwrap_or(chars_left);
        let crop_to_idx = title.char_indices()
            .nth(title.chars().count() - chars_right)
            .map(|(idx, _)| idx)
            .unwrap_or(title.len());
        format!(
            "{}…{}",
            &title[..crop_from_idx],
            &title[crop_to_idx..]
        )
    }
}

async fn get_initial_title_state() -> Result<String> {
    // We do want to know when the operation is successfull but the title string is not there,
    // which would be because there is no active client
    debug!("Fetching initial title state");
    
    let client = hyprland::data::Client::get_active_async().await?;
    let display_name = match client {
        Some(client) => format_title_string(client.title, 64),
        None => String::new()
    };
    
    info!("Initial title: {:?}", display_name);
    Ok(display_name)
}

async fn send_workspace_update(update: WorkspaceUpdate) -> Result<()> {
    let sender = WORKSPACE_SENDER.get()
        .ok_or_else(|| AppError::WorkspaceChannel("Global sender not initialized".to_string()))?;
    
    sender.send(update)
        .map_err(|e| AppError::WorkspaceChannel(format!("Failed to send update: {}", e)))?;
    
    Ok(())
}

async fn send_title_update(update: Option<String>) -> Result<()> {
    let sender = TITLE_SENDER.get()
        .ok_or_else(|| AppError::TitleChannel("Global sender not initialized".to_string()))?;
    
    // TODO: maybe handle None variant as: remove the widget? maybe pass as optional and handle
    // that None case elsewere
    sender.send(update.unwrap_or_default())
        .map_err(|e| AppError::TitleChannel(format!("Failed to send update: {}", e)))?;
    
    Ok(())
}

async fn send_battery_update(update: String) -> Result<()> {
    let sender = BATTERY_SENDER.get()
        .ok_or_else(|| AppError::BatteryChannel("Global sender not initialized".to_string()))?;
    
    sender.send(update)
        .map_err(|e| AppError::BatteryChannel(format!("Failed to send update: {}", e)))?;
    
    Ok(())
}

async fn handle_workspace_change(workspace_data: hyprland::event_listener::WorkspaceEventData) -> Result<()> {
    debug!("Handling workspace change event");
    
    let display_name = format_workspace_name_from_type(&workspace_data.name, workspace_data.id);
    info!("Workspace changed to: {}", display_name);
    
    // Send combined workspace update with both name and ID
    let update = WorkspaceUpdate {
        name: display_name,
        id: workspace_data.id,
    };
    send_workspace_update(update).await
}

fn update_title_widget_workspace_color(title_widget: &gtk::Label, workspace_id: hyprland::shared::WorkspaceId) {
    // Get workspace color based on ID
    let color = get_workspace_color(workspace_id);
    
    // Apply color directly via CSS provider for immediate update
    let css_provider = gtk::CssProvider::new();
    let css = format!(
        ".title-widget {{ background-color: {}; }}",
        color
    );
    
    css_provider.load_from_data(&css);
    
    let style_context = title_widget.style_context();
    style_context.add_provider(&css_provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
    
    debug!("Updated title widget color to: {} for workspace: {}", color, workspace_id);
}

fn get_workspace_color(workspace_id: hyprland::shared::WorkspaceId) -> &'static str {
    match workspace_id {
        1 => "rgba(122, 162, 247, 0.5)",
        2 => "rgba(125, 207, 255, 0.5)",
        3 => "rgba(158, 206, 106, 0.5)",
        4 => "rgba(187, 154, 247, 0.5)",
        5 => "rgba(247, 118, 142, 0.5)",
        6 => "rgba(255, 158, 102, 0.5)",
        7 => "rgba(157, 124, 216, 0.5)",
        8 => "rgba(224, 175, 104, 0.5)",
        9 => "rgba(42, 195, 222, 0.5)",
        10 => "rgba(13, 185, 215, 0.5)",
        _ => "rgba(67, 233, 123, 0.5)", // Default color
    }
}

async fn handle_title_change(title_data: hyprland::event_listener::WindowTitleEventData) -> Result<()> {
    debug!("Handling title change event");
    
    // If not active client skip event except if there is no active client, use title_data.address
    let active_client = hyprland::data::Client::get_active_async().await?
    // log + early return, not as debug it is normal sometimes for it to not be an active client,
    // use combinators
    .filter(|client| client.address == title_data.address);

    if let Some(client) = active_client {
        let formatted_title = format_title_string(client.title, 64);
        info!("Title changed to: {}", formatted_title);
        send_title_update(Some(formatted_title)).await
    } else {
        info!("No active client matches the title change event");
        Ok(())
    }
}

async fn handle_active_window_change(window_data: Option<hyprland::event_listener::WindowEventData>) -> Result<()> {
    debug!("Handling active window change event");
    
    let formatted_title = match &window_data {
        Some(data) => {
            debug!("Window data - class: '{}', title: '{}', address: '{}'", data.class, data.title, data.address);
            format_title_string(data.title.clone(), 64)
        }
        None => {
            debug!("No active window (window_data is None)");
            String::new()
        }
    };
    
    info!("Active window changed, title: '{}'", formatted_title);
    debug!("Sending title update: '{}'", formatted_title);
    send_title_update(Some(formatted_title)).await
}


async fn setup_title_event_listener() -> Result<()> {
    debug!("Setting up title event listener");
    
    let initial_state = get_initial_title_state().await
        .unwrap_or_else(|e| {
            warn!("Failed to get initial title state: {}", e);
            "".to_string()
        });
    
    if let Err(e) = send_title_update(Some(initial_state)).await {
        error!("Failed to send initial title update: {}", e);
    }
    
    let mut event_listener = AsyncEventListener::new();
    
    event_listener.add_window_title_changed_handler(async_closure! {
        |title_data| {
            if let Err(e) = handle_title_change(title_data).await {
                error!("Failed to handle title change: {}", e);
            }
        }
    });
    
    event_listener.add_active_window_changed_handler(async_closure! {
        |window_data| {
            if let Err(e) = handle_active_window_change(window_data).await {
                error!("Failed to handle active window change: {}", e);
            }
        }
    });
    
    info!("Starting title event listener");
    event_listener.start_listener_async().await?;
    
    Ok(())
}

async fn setup_workspace_event_listener() -> Result<()> {
    debug!("Setting up workspace event listener");
    
    let workspace_result = hyprland::data::Workspace::get_active_async().await;
    
    match workspace_result {
        Ok(workspace) => {
            let initial_state = format_workspace_name_from_string(&workspace.name, workspace.id);
            let update = WorkspaceUpdate {
                name: initial_state,
                id: workspace.id,
            };
            if let Err(e) = send_workspace_update(update).await {
                error!("Failed to send initial workspace update: {}", e);
            }
        }
        Err(e) => {
            warn!("Failed to get initial workspace state: {}", e);
            let fallback_update = WorkspaceUpdate {
                name: "Workspace ?".to_string(),
                id: 1, // WorkspaceId is just an i32
            };
            if let Err(e) = send_workspace_update(fallback_update).await {
                error!("Failed to send fallback workspace update: {}", e);
            }
        }
    }
    
    let mut event_listener = AsyncEventListener::new();
    
    event_listener.add_workspace_changed_handler(async_closure! {
        |workspace_data| {
            if let Err(e) = handle_workspace_change(workspace_data).await {
                error!("Failed to handle workspace change: {}", e);
            }
        }
    });
    
    info!("Starting workspace event listener");
    event_listener.start_listener_async().await?;
    
    Ok(())
}

fn setup_workspace_updates(label: gtk::Label, title_widget: gtk::Label) -> Result<()> {
    debug!("Setting up workspace updates");
    
    // Set up combined workspace updates
    let (tx, mut rx) = mpsc::unbounded_channel();
    if WORKSPACE_SENDER.set(tx).is_err() {
        return Err(AppError::WorkspaceChannel("Failed to set global workspace sender".to_string()));
    }
    
    tokio::spawn(async move {
        if let Err(e) = setup_workspace_event_listener().await {
            error!("Workspace event listener failed: {}", e);
        }
    });
    
    // Handle combined workspace updates (name + ID) in single frame
    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating workspace - label: '{}', color for workspace: {}", update.name, update.id);
            // Update both workspace text and title color atomically
            label.set_text(&update.name);
            update_title_widget_workspace_color(&title_widget, update.id);
        }
    });
    
    Ok(())
}

fn setup_title_updates(label: gtk::Label) -> Result<()> {
    debug!("Setting up title updates");
    
    let (tx, mut rx) = mpsc::unbounded_channel();
    
    if TITLE_SENDER.set(tx).is_err() {
        return Err(AppError::TitleChannel("Failed to set global title sender".to_string()));
    }
    
    tokio::spawn(async move {
        if let Err(e) = setup_title_event_listener().await {
            error!("Title event listener failed: {}", e);
        }
    });
    
    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating title label: {}", update);
            label.set_text(&update);
        }
    });
    
    Ok(())
}

fn setup_battery_updates(label: gtk::Label) -> Result<()> {
    debug!("Setting up battery updates");
    
    let (tx, mut rx) = mpsc::unbounded_channel();
    
    if BATTERY_SENDER.set(tx).is_err() {
        return Err(AppError::BatteryChannel("Failed to set global battery sender".to_string()));
    }
    
    tokio::spawn(async move {
        if let Err(e) = monitor_battery().await {
            error!("Battery monitoring failed: {}", e);
        }
    });
    
    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating battery label: {}", update);
            label.set_text(&update);
        }
    });
    
    Ok(())
}

fn create_title_widget() -> Result<gtk::Label> {
    debug!("Creating title widget");
    let label = gtk::Label::new(Some("Application Title"));
    label.add_css_class("title-widget");
    label.set_halign(gtk::Align::End);
    Ok(label)
}

fn create_time_widget() -> Result<gtk::Label> {
    debug!("Creating time widget");
    let time_str = get_current_time()?;
    let label = gtk::Label::new(Some(&time_str));
    label.add_css_class("time-widget");
    label.set_halign(gtk::Align::End);
    Ok(label)
}

fn get_current_time() -> Result<String> {
    Ok(Local::now().format("%H:%M").to_string())
}

fn update_time_widget(label: gtk::Label) -> Result<()> {
    debug!("Setting up time widget updates");
    
    let label_weak = label.downgrade();
    glib::timeout_add_seconds_local(1, move || {
        let Some(label) = label_weak.upgrade() else {
            debug!("Time widget label dropped, stopping updates");
            return glib::ControlFlow::Break;
        };
        
        let time_str = match get_current_time() {
            Ok(time) => time,
            Err(e) => {
                warn!("Failed to get current time: {}", e);
                "??:??".to_string()
            }
        };
        
        label.set_text(&time_str);
        glib::ControlFlow::Continue
    });
    
    Ok(())
}

fn create_bt_widget() -> Result<gtk::Label> {
    debug!("Creating bluetooth widget");
    let label = gtk::Label::new(Some("No BT"));
    label.add_css_class("bt-widget");
    label.set_halign(gtk::Align::End);
    Ok(label)
}

fn create_battery_widget() -> Result<gtk::Label> {
    debug!("Creating battery widget");
    let label = gtk::Label::new(Some("🔋 ??%"));
    label.add_css_class("battery-widget");
    label.set_halign(gtk::Align::End);
    Ok(label)
}

fn create_left_group() -> Result<(gtk::Box, gtk::Label)> {
    debug!("Creating left group");
    
    let left_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    left_group.add_css_class("left-group");
    left_group.set_valign(gtk::Align::Start);
    left_group.set_hexpand(false);
    
    let workspace_widget = create_workspace_widget()?;
    left_group.append(&workspace_widget);
    
    Ok((left_group, workspace_widget))
}

fn create_center_group() -> Result<(gtk::Box, gtk::Label, gtk::Box)> {
    debug!("Creating center group");
    
    let center_spacer_start = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    center_spacer_start.set_hexpand(true);

    let center_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    center_group.add_css_class("center-group");
    center_group.set_valign(gtk::Align::Center);
    center_group.set_hexpand(false);
    
    let title_widget = create_title_widget()?;
    center_group.append(&title_widget);

    let center_spacer_end = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    center_spacer_end.set_hexpand(true);
    
    Ok((center_spacer_start, title_widget, center_spacer_end))
}

fn create_right_group() -> Result<(gtk::Box, gtk::Label, gtk::Label)> {
    debug!("Creating right group");
    
    let right_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    right_group.add_css_class("right-group");
    right_group.set_hexpand(false);
    right_group.set_valign(gtk::Align::End);
    right_group.append(&create_bt_widget()?);
    
    let battery_widget = create_battery_widget()?;
    right_group.append(&battery_widget);
    
    let time_widget = create_time_widget()?;
    right_group.append(&time_widget);
    
    Ok((right_group, battery_widget, time_widget))
}

fn create_experimental_bar() -> Result<(gtk::Box, gtk::Label, gtk::Label, gtk::Label, gtk::Label)> {
    debug!("Creating experimental bar");
    
    let main_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    main_box.set_hexpand(true);
    main_box.set_valign(gtk::Align::Center);

    let (left_group, workspace_widget) = create_left_group()?;
    let (center_spacer_start, title_widget, center_spacer_end) = create_center_group()?;
    let (right_group, battery_widget, time_widget) = create_right_group()?;

    main_box.append(&left_group);
    main_box.append(&center_spacer_start);
    main_box.append(&title_widget);
    main_box.append(&center_spacer_end);
    main_box.append(&right_group);

    Ok((main_box, battery_widget, time_widget, workspace_widget, title_widget))
}

fn load_css_styles(window: &gtk::ApplicationWindow) -> Result<()> {
    debug!("Loading CSS styles");
    
    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_path("style.css");
    
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(window),
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );
    
    info!("CSS styles loaded successfully");
    Ok(())
}

fn configure_layer_shell(window: &gtk::ApplicationWindow) -> Result<()> {
    debug!("Configuring layer shell");
    
    window.init_layer_shell();
    window.set_layer(Layer::Bottom);
    window.auto_exclusive_zone_enable();

    let anchors = [
        (Edge::Left, true),
        (Edge::Right, true),
        (Edge::Top, true),
        (Edge::Bottom, false),
    ];

    for (anchor, state) in anchors {
        window.set_anchor(anchor, state);
    }

    window.set_default_height(30);
    
    info!("Layer shell configured successfully");
    Ok(())
}

async fn monitor_battery() -> Result<()> {
    info!("Starting battery monitoring task");
    let connection = Connection::system().await?;
    // Get initial status
    // TODO: what if there is no battery (for example, in a desktop?)
    // Probably should monitor if a battery comes into existance so
    // you should not return

    let obj_proxy = fdo::PropertiesProxy::builder(&connection)
        .destination("org.freedesktop.UPower")?
        .path("/org/freedesktop/UPower/devices/battery_BAT0")?
        .build()
        .await?;
    info!("Object proxy created");

    let interface_name = InterfaceName::try_from("org.freedesktop.UPower.Device")?;
    match obj_proxy
        .get(interface_name, "Percentage")
        .await {
        Ok(value) => {
            let percentage = f64::try_from(value)?;
            info!("Battery is at {:.1}%", percentage);
            let battery_text = format!("🔋 {:.0}%", percentage);
            send_battery_update(battery_text).await?;
        }
        Err(e) => {
            info!("No battery detected initially (likely desktop system): {}", e);
            // Invisible label, maybe thing removing the widget altogether or make it invisible or
            // something: we don't want to polute the bar: if there is no battery then this info
            // is not so relevant
            send_battery_update("".to_string()).await?;
        }
    };
    info!("Initial battery state processed"); 
    
    // Subscribe to UPower property changes before creating MessageStream.
    // Without this subscription, the MessageStream receives no messages because
    // D-Bus requires explicit signal subscriptions via match rules.
    // Note: ObjectManagerProxy only reports interface additions/removals, not property changes.
    // As per https://openrr.github.io/openrr/zbus/fdo/struct.ObjectManagerProxy.html:
    // "Changes to properties on existing interfaces are not reported using this interface"
    // Therefore we must subscribe to org.freedesktop.DBus.Properties.PropertiesChanged.
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.UPower")?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path("/org/freedesktop/UPower/devices/battery_BAT0")?
        .build();
    
    let dbus_proxy = fdo::DBusProxy::new(&connection).await?;
    dbus_proxy.add_match_rule(rule).await?;
    info!("Battery monitor: Subscribed to UPower property changes");

    let mut stream: zbus::MessageStream = connection.into();
    info!("Battery monitor: Starting to listen for D-Bus messages");

    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else {
            error!("Error receiving DBus message in battery monitor loop");
            continue;
        };

        debug!("Got an event in event stream: {:?}", msg);

        let header = msg.header();
        debug!("Battery monitor: Received D-Bus message from path: {:?}, interface: {:?}, member: {:?}", 
               header.path(), header.interface(), header.member());

        let Some(member) = header.member() else {
            debug!("Battery monitor: Message has no member field");
            continue;
        };

        if member.as_str() != "PropertiesChanged" {
            debug!("Battery monitor: Ignoring message with member: {}", member.as_str());
            continue;
        }

        info!("Battery monitor: Received PropertiesChanged signal");
        
        let body = msg.body();
        let Ok(args) = body.deserialize::<(String, std::collections::HashMap<String, zbus::zvariant::Value>, Vec<String>)>() else {
            warn!("Battery monitor: Failed to deserialize PropertiesChanged message");
            continue;
        };

        debug!("Battery monitor: PropertiesChanged interface: {}", args.0);
        debug!("Battery monitor: PropertiesChanged properties: {:?}", args.1.keys().collect::<Vec<_>>());
        
        if args.0 != "org.freedesktop.UPower.Device" {
            debug!("Battery monitor: PropertiesChanged for different interface: {}", args.0);
            continue;
        }

        info!("Battery monitor: UPower.Device properties changed");
        
        let Some(percent_value) = args.1.get("Percentage") else {
            debug!("Battery monitor: No Percentage property in UPower.Device change");
            continue;
        };

        let Ok(percentage) = f64::try_from(percent_value) else {
            warn!("Battery monitor: Failed to convert percentage value");
            continue;
        };

        info!("Battery monitor: Battery percentage updated to {:.1}%", percentage);
        let battery_text = format!("🔋 {:.0}%", percentage);
        if let Err(e) = send_battery_update(battery_text).await {
            error!("Battery monitor: Failed to send battery update: {}", e);
        }
    }

    warn!("Battery monitor: Message stream ended unexpectedly");

    Ok(())
}

fn activate(application: &gtk::Application) -> Result<()> {
    info!("Activating GTK application");
    
    let window = gtk::ApplicationWindow::new(application);
    window.add_css_class("layer-bar");

    load_css_styles(&window)?;
    configure_layer_shell(&window)?;

    let (bar, battery_widget, time_widget, workspace_widget, title_widget) = create_experimental_bar()?;
    window.set_child(Some(&bar));
    window.show();

    update_time_widget(time_widget)?;
    setup_workspace_updates(workspace_widget, title_widget.clone())?;
    setup_title_updates(title_widget)?;
    setup_battery_updates(battery_widget)?;

    info!("Application activated successfully");
    Ok(())
}

fn create_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    debug!("Creating tokio runtime");
    
    tokio::runtime::Runtime::new()
        .map_err(|e| AppError::TokioRuntime(format!("Failed to create runtime: {}", e)))
}

fn main() -> Result<()> {
    setup_logging();
    info!("Starting GTK status bar application");

    let rt = create_tokio_runtime()?;
    let _guard = rt.enter();
    
    let application = gtk::Application::new(Some("sh.wmww.gtk-layer-example"), Default::default());

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
    tokio::spawn(async move {
        monitor_battery().await;
    });
    
    Ok(())
}
