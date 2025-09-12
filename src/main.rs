mod error;

use anyhow::{Context, Result};

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
// use error::{AppError, Result};
use zbus::Connection;
use zbus::fdo;
use zbus_names::InterfaceName;
use zbus::message::Type as MessageType;
use zbus::MatchRule;
use std::collections::HashMap;
use zbus::zvariant;
use zbus::zvariant::Value;
use futures::StreamExt;

// PipeWire dependencies
use pipewire as pw;
use pw::spa::pod::{Pod, Value as PodValue, ValueArray, deserialize::PodDeserializer};
use std::rc::Rc;
use std::{cell::RefCell};
use pw::{
    device::Device,
    node::Node,
    proxy::{Listener, ProxyT},
    thread_loop::ThreadLoop,
    types::ObjectType,
};

#[derive(Debug, Clone)]
struct WorkspaceUpdate {
    name: String,
    id: hyprland::shared::WorkspaceId,
}

#[derive(Debug, Clone)]
struct VolumeUpdate {
    id: u32,
    name: String,
    volume_percent: Option<u8>,  // Main volume 0-100%
    channel_percent: Option<u8>, // First channel volume 0-100% (most accurate for user changes)
    is_muted: Option<bool>,
}

static WORKSPACE_SENDER: OnceLock<mpsc::UnboundedSender<WorkspaceUpdate>> = OnceLock::new();
static TITLE_SENDER:     OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();
static BATTERY_SENDER:   OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();
static BLUETOOTH_SENDER: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();

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

fn create_volume_widget() -> Result<gtk::Label> {
    debug!("Creating volume widget");
    let label = gtk::Label::new(Some("Volume ?"));
    label.add_css_class("volume-widget");
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

// Safe wrapper for ThreadLoop constructor to encapsulate unsafe code
fn new_thread_loop() -> Result<ThreadLoop, pw::Error> {
    // Safety: ThreadLoop is created on the PW thread, used only there, and stopped before drop.
    unsafe { ThreadLoop::new(None, None) }
}

// Manage PipeWire objects and listeners on the PipeWire thread
struct PWKeepAlive {
    proxies: HashMap<u32, Box<dyn ProxyT>>,
    listeners: HashMap<u32, Vec<Box<dyn Listener>>>,
}

impl PWKeepAlive {
    fn new() -> Self {
        Self {
            proxies: HashMap::new(),
            listeners: HashMap::new(),
        }
    }

    fn add_proxy(&mut self, proxy: Box<dyn ProxyT>, listener: Box<dyn Listener>) {
        let id = proxy.upcast_ref().id();
        self.proxies.insert(id, proxy);
        self.listeners.entry(id).or_default().push(listener);
    }

    fn add_listener(&mut self, id: u32, listener: Box<dyn Listener>) {
        self.listeners.entry(id).or_default().push(listener);
    }

    fn remove(&mut self, id: u32) {
        self.proxies.remove(&id);
        self.listeners.remove(&id);
    }
}

// Helper functions to identify audio objects
fn is_audio_node(props: &Option<&pw::spa::utils::dict::DictRef>) -> bool {
    props.and_then(|p| p.get("media.class"))
         .map(|c| c.contains("Audio") && (c.contains("Sink") || c.contains("Source")))
         .unwrap_or(false)
}

fn is_audio_device(props: &Option<&pw::spa::utils::dict::DictRef>) -> bool {
    props.and_then(|p| p.get("device.api"))
         .map(|api| api == "alsa" || api == "bluez5")
         .unwrap_or(false)
}

// SPA property constants for volume control
const SPA_PROP_VOLUME: u32 = 65539;
const SPA_PROP_MUTE: u32 = 65540;
const SPA_PROP_CHANNEL_VOLUMES: u32 = 65544;

fn parse_volume_from_pod(param: &Pod) -> Option<(Option<u8>, Option<u8>, Option<bool>)> {
    let obj = param.as_object().ok()?;
    let mut volume: Option<f32> = None;
    let mut mute: Option<bool> = None;
    let mut channel_volumes: Vec<f32> = Vec::new();

    for prop in obj.props() {
        let key = prop.key().0;
        let value_pod = prop.value();

        match key {
            SPA_PROP_VOLUME => {
                if let Ok(vol) = value_pod.get_float() {
                    volume = Some(vol);
                }
            },
            SPA_PROP_MUTE => {
                if let Ok(m) = value_pod.get_bool() {
                    mute = Some(m);
                }
            },
            SPA_PROP_CHANNEL_VOLUMES => {
                if let Ok((_, PodValue::ValueArray(ValueArray::Float(volumes)))) = 
                    PodDeserializer::deserialize_any_from(value_pod.as_bytes()) {
                    channel_volumes = volumes;
                }
            },
            _ => {}
        }
    }

    // Convert to structured data (no string formatting!)
    let volume_percent = volume.map(|v| (v * 100.0).round() as u8);
    let channel_percent = channel_volumes.first().map(|&v| (v * 100.0).round() as u8);
    
    // Return None only if we have no volume data at all
    if volume_percent.is_none() && channel_percent.is_none() && mute.is_none() {
        return None;
    }

    Some((volume_percent, channel_percent, mute))
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

    debug!("Initial title: {:?}", display_name);
    Ok(display_name)
}

async fn send_workspace_update(update: WorkspaceUpdate) -> Result<()> {
    let sender = WORKSPACE_SENDER.get()
        .context("Global workspace sender not initialized")?;

    sender.send(update)
        .context("Failed to send workspace update")?;

    Ok(())
}

async fn send_title_update(update: Option<String>) -> Result<()> {
    let sender = TITLE_SENDER.get()
        .context("Global title sender not initialized")?;

    // TODO: maybe handle None variant as: remove the widget? maybe pass as optional and handle
    // that None case elsewere
    sender.send(update.unwrap_or_default())
        .context("Failed to send title update")?;

    Ok(())
}

async fn send_battery_update(update: String) -> Result<()> {
    let sender = BATTERY_SENDER.get()
        .context("Global battery sender not initialized")?;

    sender.send(update)
        .context("Failed to send battery update")?;

    Ok(())
}

fn compute_bluetooth_display_string(bluetooth_devices: &HashMap<String, BluetoothDevice>) -> String {
    let device_strings: Vec<String> = bluetooth_devices
        .values()
        .filter_map(|device| {
            // Only include devices with battery percentage
            let percentage = device.battery_percentage?;
            
            // Get first character of device name, fallback to 'D' for device
            let first_char = device.device_name
                .as_ref()
                .and_then(|name| name.chars().next())
                .unwrap_or('D');
            
            Some(format!("{}{}", first_char, percentage))
        })
        .collect();
    
    if device_strings.is_empty() {
        "No BT".to_string()
    } else {
        device_strings.join(" ")
    }
}

async fn handle_workspace_change(workspace_data: hyprland::event_listener::WorkspaceEventData) -> Result<()> {
    debug!("Handling workspace change event");

    let display_name = format_workspace_name_from_type(&workspace_data.name, workspace_data.id);
    debug!("Workspace changed to: {}", display_name);

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
        debug!("Title changed to: {}", formatted_title);
        send_title_update(Some(formatted_title)).await
    } else {
        debug!("No active client matches the title change event");
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

    debug!("Active window changed, title: '{}'", formatted_title);
    debug!("Sending title update: '{}'", formatted_title);
    send_title_update(Some(formatted_title)).await
}


async fn setup_title_event_listener() -> Result<()> {
    debug!("Setting up title event listener");

    let initial_state = get_initial_title_state().await
        .unwrap_or_else(|e| {
            error!("Failed to get initial title state: {}", e);
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
            error!("Failed to get initial workspace state: {}", e);
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
        return Err(anyhow::anyhow!("Failed to set global workspace sender"));
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
        return Err(anyhow::anyhow!("Failed to set global title sender"));
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
        return Err(anyhow::anyhow!("Failed to set global battery sender"));
    }

    tokio::spawn(async move {
        if let Err(e) = monitor_dbus().await {
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

fn setup_bluetooth_updates(label: gtk::Label) -> Result<()> {
    debug!("Setting up Bluetooth battery updates");

    let (tx, mut rx) = mpsc::unbounded_channel();

    if BLUETOOTH_SENDER.set(tx).is_err() {
        return Err(anyhow::anyhow!("Failed to set global Bluetooth sender"));
    }

    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating Bluetooth battery label: {}", update);
            label.set_text(&update);
        }
    });

    Ok(())
}

fn setup_volume_updates(label: gtk::Label) -> Result<()> {
    debug!("Setting up volume updates with tokio async channels");

    let (sender, mut receiver) = mpsc::unbounded_channel::<VolumeUpdate>();

    // Start PipeWire monitoring on dedicated thread
    start_pipewire_thread(sender)?;

    // Spawn async task on GTK main thread to handle volume updates
    glib::spawn_future_local(async move {
        debug!("🚀 Starting async volume update loop...");
        
        while let Some(update) = receiver.recv().await {
            // Use channel volume first (more accurate), fallback to main volume
            if let Some(volume_percent) = update.channel_percent.or(update.volume_percent) {
                let display_text = format!("🔊 {}: {}%{}", 
                    update.name.split_whitespace().next().unwrap_or("Audio"),
                    volume_percent,
                    if update.is_muted == Some(true) { " 🔇" } else { "" }
                );
                label.set_text(&display_text);
                debug!("📺 GTK UI updated via ASYNC: {}", display_text);
            } else {
                debug!("📺 Skipping GUI update - no volume data available");
            }
        }
        
        debug!("⚠️ Volume update loop ended");
    });

    Ok(())
}


// Start PipeWire monitoring on dedicated ThreadLoop thread
fn start_pipewire_thread(sender: mpsc::UnboundedSender<VolumeUpdate>) -> Result<()> {
    std::thread::spawn(move || {
        debug!("🔧 Initializing PipeWire on dedicated thread...");
        
        // Initialize PipeWire on this thread
        pw::init();
        debug!("✅ PipeWire initialized");

        // Create ThreadLoop - manages PipeWire loop on this thread
        let thread_loop = match new_thread_loop() {
            Ok(tl) => {
                debug!("✅ ThreadLoop created");
                tl
            }
            Err(e) => {
                error!("❌ Failed to create ThreadLoop: {}", e);
                return;
            }
        };

        let context = match pw::context::Context::new(&thread_loop) {
            Ok(ctx) => {
                debug!("✅ Context created");
                ctx
            }
            Err(e) => {
                error!("❌ Failed to create context: {}", e);
                return;
            }
        };

        let core = match context.connect(None) {
            Ok(c) => {
                debug!("✅ Core connected");
                c
            }
            Err(e) => {
                error!("❌ Failed to connect core: {}", e);
                return;
            }
        };

        let _core_listener = core
            .add_listener_local()
            .info(|info| {
                debug!("📡 PipeWire connected: {}", info.name());
            })
            .error(|id, seq, res, message| {
                error!("❌ PipeWire error id:{} seq:{} res:{}: {}", id, seq, res, message);
            })
            .register();

        let registry = match core.get_registry() {
            Ok(reg) => {
                debug!("✅ Registry obtained");
                Rc::new(reg)
            }
            Err(e) => {
                error!("❌ Failed to get registry: {}", e);
                return;
            }
        };
        let registry_weak = Rc::downgrade(&registry);
        let keep_alive = Rc::new(RefCell::new(PWKeepAlive::new()));
        let keep_alive_weak = Rc::downgrade(&keep_alive);

        debug!("🎵 PipeWire ThreadLoop started - monitoring volume changes with async channels");

        // Registry listener for discovering audio objects
        let _registry_listener = registry
            .add_listener_local()
            .global(move |obj| {
                if let (Some(reg), Some(keep)) = (registry_weak.upgrade(), keep_alive_weak.upgrade()) {
                    match obj.type_ {
                        ObjectType::Node if is_audio_node(&obj.props) => {
                            let node: Node = reg.bind(obj).unwrap();
                            let id = node.upcast_ref().id();
                            let name = obj.props
                                .and_then(|p| p.get("node.description").or_else(|| p.get("node.name")))
                                .unwrap_or("Unknown Node").to_string();

                            debug!("📱 Monitoring audio node: {} ({})", name, id);

                            node.subscribe_params(&[
                                pw::spa::param::ParamType::Props,
                                pw::spa::param::ParamType::Route,
                            ]);

                            let name_clone = name.clone();
                            let sender_clone = sender.clone();
                            let node_listener = node
                                .add_listener_local()
                                .param(move |_seq, param_type, _idx, _next, param| {
                                    if param_type == pw::spa::param::ParamType::Props {
                                        if let Some(pod) = param {
                                            if let Some((volume_percent, channel_percent, is_muted)) = parse_volume_from_pod(pod) {
                                                debug!("🔊 Node {}: {} - Vol: {:?}% | Ch: {:?}% | Mute: {:?} [ASYNC DELIVERY]", 
                                                       id, name_clone, volume_percent, channel_percent, is_muted);
                                                
                                                let update = VolumeUpdate {
                                                    id,
                                                    name: name_clone.clone(),
                                                    volume_percent,
                                                    channel_percent,
                                                    is_muted,
                                                };
                                                // Send via async channel - immediate delivery!
                                                if let Err(e) = sender_clone.send(update) {
                                                    error!("Failed to send volume update: {}", e);
                                                }
                                            }
                                        }
                                    }
                                })
                                .register();

                            let proxy: Box<dyn ProxyT> = Box::new(node);
                            let proxy_id = proxy.upcast_ref().id();
                            let keep_weak = Rc::downgrade(&keep);
                            let removed_listener = proxy.upcast_ref()
                                .add_listener_local()
                                .removed(move || {
                                    if let Some(k) = keep_weak.upgrade() {
                                        k.borrow_mut().remove(proxy_id);
                                    }
                                })
                                .register();

                            keep.borrow_mut().add_proxy(proxy, Box::new(node_listener));
                            keep.borrow_mut().add_listener(id, Box::new(removed_listener));
                        }
                        ObjectType::Device if is_audio_device(&obj.props) => {
                            let device: Device = reg.bind(obj).unwrap();
                            let id = device.upcast_ref().id();
                            let name = obj.props
                                .and_then(|p| p.get("device.description").or_else(|| p.get("device.name")))
                                .unwrap_or("Unknown Device").to_string();

                            debug!("🔌 Monitoring audio device: {} ({})", name, id);

                            device.subscribe_params(&[
                                pw::spa::param::ParamType::Props,
                                pw::spa::param::ParamType::Route,
                            ]);

                            let name_clone = name.clone();
                            let sender_clone = sender.clone();
                            let device_listener = device
                                .add_listener_local()
                                .param(move |_seq, param_type, _idx, _next, param| {
                                    if param_type == pw::spa::param::ParamType::Props {
                                        if let Some(pod) = param {
                                            if let Some((volume_percent, channel_percent, is_muted)) = parse_volume_from_pod(pod) {
                                                debug!("🔊 Device {}: {} - Vol: {:?}% | Ch: {:?}% | Mute: {:?} [ASYNC DELIVERY]", 
                                                       id, name_clone, volume_percent, channel_percent, is_muted);
                                                
                                                let update = VolumeUpdate {
                                                    id,
                                                    name: name_clone.clone(),
                                                    volume_percent,
                                                    channel_percent,
                                                    is_muted,
                                                };
                                                if let Err(e) = sender_clone.send(update) {
                                                    error!("Failed to send volume update: {}", e);
                                                }
                                            }
                                        }
                                    }
                                })
                                .register();

                            let proxy: Box<dyn ProxyT> = Box::new(device);
                            let proxy_id = proxy.upcast_ref().id();
                            let keep_weak = Rc::downgrade(&keep);
                            let removed_listener = proxy.upcast_ref()
                                .add_listener_local()
                                .removed(move || {
                                    if let Some(k) = keep_weak.upgrade() {
                                        k.borrow_mut().remove(proxy_id);
                                    }
                                })
                                .register();

                            keep.borrow_mut().add_proxy(proxy, Box::new(device_listener));
                            keep.borrow_mut().add_listener(id, Box::new(removed_listener));
                        }
                        _ => {}
                    }
                }
            })
            .register();

        // Start the ThreadLoop
        thread_loop.start();
        debug!("✅ ThreadLoop started successfully");

        debug!("🔄 PipeWire thread running - async event delivery active...");

        // Set up graceful shutdown channel  
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        
        // Block this OS thread until shutdown is requested (no wasteful sleep loop!)
        // ThreadLoop::start() already manages its own internal event thread
        stop_rx.recv().ok();
        
        debug!("🛑 Shutdown requested, stopping ThreadLoop...");
        thread_loop.stop();
        debug!("✅ ThreadLoop stopped gracefully");
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
                error!("Failed to get current time: {}", e);
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

fn create_right_group() -> Result<(gtk::Box, gtk::Label, gtk::Label, gtk::Label, gtk::Label)> {
    debug!("Creating right group");

    let right_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    right_group.add_css_class("right-group");
    right_group.set_hexpand(false);
    right_group.set_valign(gtk::Align::End);
    let bt_widget = create_bt_widget()?;
    right_group.append(&bt_widget);

    let volume_widget = create_volume_widget()?;
    right_group.append(&volume_widget);

    let battery_widget = create_battery_widget()?;
    right_group.append(&battery_widget);

    let time_widget = create_time_widget()?;
    right_group.append(&time_widget);

    Ok((right_group, bt_widget, volume_widget, battery_widget, time_widget))
}

fn create_experimental_bar() -> Result<(gtk::Box, gtk::Label, gtk::Label, gtk::Label, gtk::Label, gtk::Label, gtk::Label)> {
    debug!("Creating experimental bar");

    let main_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    main_box.set_hexpand(true);
    main_box.set_valign(gtk::Align::Center);

    let (left_group, workspace_widget) = create_left_group()?;
    let (center_spacer_start, title_widget, center_spacer_end) = create_center_group()?;
    let (right_group, bt_widget, volume_widget, battery_widget, time_widget) = create_right_group()?;

    main_box.append(&left_group);
    main_box.append(&center_spacer_start);
    main_box.append(&title_widget);
    main_box.append(&center_spacer_end);
    main_box.append(&right_group);

    Ok((main_box, bt_widget, volume_widget, battery_widget, time_widget, workspace_widget, title_widget))
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

fn process_bluetooth_battery_percentage(value: Value<'_>) -> Option<u8> {
    u8::try_from(value)
        .inspect_err(|e| {
            error!("Failed to convert Bluetooth battery percentage to u8: {}", e);
        })
        .ok()
        .inspect(|percentage| {
            info!("Bluetooth device battery at {}%", percentage);
        })
}

async fn process_battery_percentage(value: Value<'_>) {
    if let Some(percentage) = f64::try_from(value)
        .inspect_err(|e| {
            error!("Failed to convert battery percentage to f64: {}", e);
        })
        .ok() 
    {
        info!("Battery percentage changed to {:.1}%", percentage);
        let battery_text = format!("🔋 {:.0}%", percentage);
        if let Err(e) = send_battery_update(battery_text).await {
            error!("Failed to send battery update: {}", e);
        }
    }
}

async fn process_battery_state(value: Value<'_>) {
    if let Some(state) = u32::try_from(value)
        .inspect_err(|e| {
            error!("Failed to convert battery state to u32: {}", e);
        })
        .ok() 
    {
        match state {
            1 => info!("Battery is charging (state: {})", state),
            2 => info!("Battery is discharging (state: {})", state),
            3 => info!("Battery is empty (state: {})", state),
            4 => info!("Battery is fully charged (state: {})", state),
            5 => info!("Battery charge is pending (state: {})", state),
            6 => info!("Battery discharge is pending (state: {})", state),
            _ => info!("Battery state unknown: {}", state),
        }
        // TODO: Future UI update for battery state
    }
}

fn process_bluetooth_battery_interface(battery_interface_value: &Value<'_>) -> Option<u8> {
    match battery_interface_value {
        Value::Dict(battery_info) => {
            match battery_info.get::<_, zvariant::Value>(&zvariant::Str::from("Percentage")) {
                Err(e) => {
                    error!("Dbus monitor: Failed to get percentage from a bluetooth device's battery: {}", e);
                    None
                },
                Ok(None) => {
                    debug!("Bluetooth battery interface found but no Percentage property");
                    None
                },
                Ok(Some(percentage_value)) => {
                    process_bluetooth_battery_percentage(percentage_value.clone())
                }
            }
        },
        other => {
            error!("Dbus monitor: Failed to parse battery_info as Dict: {:?}", other);
            None
        }
    }
}

fn process_battery_device_properties(properties_dict: &zvariant::Dict) {
    // Check State property (charging/discharging/fully charged)
    match properties_dict.get::<_, zvariant::Value>(&zvariant::Str::from("State")) {
        Err(e) => {
            debug!("Dbus monitor: Failed to get State property from battery device: {}", e);
        },
        Ok(None) => {
            debug!("Battery device properties found but no State property");
        },
        Ok(Some(Value::U32(state))) => {
            match state {
                0 => info!("Dbus monitor: Battery state: Unknown"),
                1 => info!("Dbus monitor: Battery state: Charging (plugged in)"),
                2 => info!("Dbus monitor: Battery state: Discharging (unplugged)"),
                3 => info!("Dbus monitor: Battery state: Empty"),
                4 => info!("Dbus monitor: Battery state: Fully charged (plugged in)"),
                5 => info!("Dbus monitor: Battery state: Pending charge"),
                6 => info!("Dbus monitor: Battery state: Pending discharge"),
                other => info!("Dbus monitor: Battery state: Unknown value {}", other),
            }
        },
        Ok(Some(other)) => {
            debug!("Battery State property has unexpected type: {:?}", other);
        },
    }

    // Check Percentage property (existing functionality)
    match properties_dict.get::<_, zvariant::Value>(&zvariant::Str::from("Percentage")) {
        Err(e) => {
            debug!("Dbus monitor: Failed to get Percentage property from battery device: {}", e);
        },
        Ok(None) => {
            debug!("Battery device properties found but no Percentage property");
        },
        Ok(Some(Value::F64(percentage))) => {
            info!("Dbus monitor: Battery percentage: {:.1}%", percentage);
        },
        Ok(Some(other)) => {
            debug!("Battery Percentage property has unexpected type: {:?}", other);
        },
    }
}

// UNSAFE assumtion for now: assume Battery1 and MediaTransport1 are on the same object when they
// exist, but a device could have just one of them or non
#[derive(Debug, Clone)]
struct BluetoothDevice {
    device_path: String,
    has_battery: bool,
    has_media: bool,
    battery_percentage: Option<u8>,
    device_name: Option<String>,
}

async fn monitor_dbus() -> Result<()> {
    info!("Starting D-Bus monitoring task");
    let connection = Connection::system().await
        .map_err(|e| {
            error!("Failed to connect to system D-Bus: {}", e);
            e
        })?;
    // Get initial status
    // TODO: what if there is no battery (for example, in a desktop?)
    // Probably should monitor if a battery comes into existance so
    // you should not return


    // will .ok() later
    let properties_proxy = zbus::fdo::PropertiesProxy::new(
        &connection,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower/devices/battery_BAT0",
    ).await
    .inspect_err(|e| error!("different style to construction battery_BAT0 proxy failed"))
    .ok();

    if let Some(proxy) = properties_proxy {
        let battery_interface_name = InterfaceName::try_from("org.freedesktop.UPower.Device")
        .inspect_err(|e| error!("Failed to create interface name: {}", e))
        .ok();
        if let Some(battery_interface_name) = battery_interface_name {
            let battery_percentage = proxy.get(battery_interface_name.clone(), "Percentage").await
            .inspect_err(|e| 
                info!("No battery detected initially (likely desktop system): {}", e)
            )
            .ok()
            .and_then(|battery| 
                f64::try_from(battery)
                .inspect_err(|e| {
                    error!("Failed to convert battery percentage to f64: {}", e);
                })
                .ok());
        
            let battery_text = battery_percentage
                .map(|percentage| {
                    info!("Battery is at {:.1}%", percentage);
                    format!("🔋 {:.0}%", percentage)
                })
                .unwrap_or_else(|| {
                    debug!("Using empty battery text");
                    String::new()
                });

            send_battery_update(battery_text).await
                .inspect_err(|e| error!("Failed to send battery update: {}", e))
                .ok();

            if let Some(state_value) = proxy.get(battery_interface_name.clone(), "State").await
                .inspect_err(|e|
                    info!("No battery state detected initially (likely desktop system): {}", e)
                )
                .ok()
            {
                process_battery_state(state_value.into()).await;
            }
        }
    };

    // Initial Bluetooth battery query - check for connected devices with battery info
    let bluez_proxy = zbus::fdo::PropertiesProxy::new(
        &connection,
        "org.bluez",
        "/", // ObjectManager path
    ).await
    .inspect_err(|e| error!("Failed to create Bluez ObjectManager proxy: {}", e))
    .ok();

    // create hashmap of bt devices:
    // TODO: Consider adding has_device1 field to BluetoothDevice struct for full symmetry
    // with has_battery and has_media fields. Current approach uses device_name presence
    // as proxy for Device1 interface availability.
    let mut bluetooth_devices: HashMap<String, BluetoothDevice> = HashMap::new();

    if let Some(bluez_proxy) = bluez_proxy {
        // Use ObjectManager to get all managed objects
        let object_manager = zbus::fdo::ObjectManagerProxy::new(&connection, "org.bluez", "/").await
            .inspect_err(|e| error!("Failed to create Bluez ObjectManager: {}", e))
            .ok();

        if let Some(object_manager) = object_manager {
            match object_manager.get_managed_objects().await {
                Ok(objects) => {
                    info!("Found {} Bluetooth objects", objects.len());

                    // Look for Bluetooth devices and populate HashMap
                    for (object_path, interfaces) in objects {
                        // Track all BT devices, some might gain battery/media interfaces later
                        let mut has_battery        = false;
                        let mut battery_percentage: Option<u8> = None;
                        let mut device_name: Option<String> = None;
                        let mut has_media         = false;

                        // TODO: transform to a match and add logs
                        // Check for Device1 interface (basic device info)
                        if let Some(device_interface) = interfaces.get("org.bluez.Device1") {
                            // Extract device name/alias
                            if let Some(name_value) = device_interface.get("Alias")
                                .or_else(|| device_interface.get("Name")) {
                                if let Ok(name) = String::try_from(name_value.clone()) {
                                    device_name = Some(name);
                                }
                            }
                        }

                        // Check for Battery1 interface
                        if let Some(battery_interface) = interfaces.get("org.bluez.Battery1") {
                            info!("Found Bluetooth device with battery at: {}", object_path);
                            has_battery = true;

                            // Get the battery percentage if available
                            if let Some(percentage_value) = battery_interface.get("Percentage") {
                                battery_percentage = process_bluetooth_battery_percentage(percentage_value.clone().into());
                            } else {
                                debug!("Bluetooth battery device at {} has no Percentage property", object_path);
                            }
                        }

                        // Check for MediaControl1 interface (changed from MediaTransport1)
                        // TODO: Problem: on the top level bt device of my earbuds
                        // we see MediaControl1 but not MediaTransport1
                        // this breaks the assumption that we wouldn't need to corelate
                        // multiple paths to a single physical device
                        // OR we could use MediaControl1
                        // we also assume the toplevel one is the one with
                        // Device1
                        // 
                        // In case you need to corelate devices, check the
                        // .Device property on the multiple devices, it seems
                        // to point to the appropiate top level device
                        if interfaces.contains_key("org.bluez.MediaControl1") {
                            has_media = true;
                            debug!("Found Bluetooth device with media control at: {}", object_path);
                        }

                        // Only add Bluetooth devices that have battery or media interfaces or have
                        // Device1 interface and thus should in theory have a name and alias
                        // NOTE: even if the docs say so, in practice we have found multiple
                        // Device1 interfaces with no name
                        if has_battery || has_media || device_name.is_some() {
                            bluetooth_devices.insert(object_path.to_string(), BluetoothDevice {
                                device_path: object_path.to_string(),
                                has_battery,
                                has_media,
                                battery_percentage,
                                device_name,
                            });
                            debug!("Added device {} to HashMap (has_battery: {}, has_media: {})", object_path, has_battery, has_media);
                        }
                    }
                    debug!("Initial bluetooth devices: {:?}", bluetooth_devices);
                    
                    // Send initial GUI update for discovered devices
                    let display_string = compute_bluetooth_display_string(&bluetooth_devices);
                    if let Some(sender) = BLUETOOTH_SENDER.get() {
                        if let Err(e) = sender.send(display_string.clone()) {
                            error!("Failed to send initial Bluetooth display update to GUI: {}", e);
                        } else {
                            info!("Sent initial Bluetooth display: {}", display_string);
                        }
                    } else {
                        warn!("Bluetooth sender not initialized, cannot send initial GUI update");
                    }
                }
                Err(e) => {
                    info!("No Bluetooth devices found or failed to query: {}", e);
                    
                    // Send "No BT" update even when no devices found
                    let display_string = compute_bluetooth_display_string(&bluetooth_devices);
                    if let Some(sender) = BLUETOOTH_SENDER.get() {
                        if let Err(e) = sender.send(display_string) {
                            error!("Failed to send 'No BT' display update to GUI: {}", e);
                        }
                    }
                }
            }
        }
    }

 

    // Subscribe to UPower property changes before creating MessageStream.
    // Without this subscription, the MessageStream receives no messages because
    // D-Bus requires explicit signal subscriptions via match rules.
    // Note: ObjectManagerProxy only reports interface additions/removals, not property changes.
    // As per https://openrr.github.io/openrr/zbus/fdo/struct.ObjectManagerProxy.html:
    // "Changes to properties on existing interfaces are not reported using this interface"
    // Therefore we must subscribe to org.freedesktop.DBus.Properties.PropertiesChanged.
    let dbus_proxy = fdo::DBusProxy::new(&connection).await?;

    // from the connection, we get the dbus_proxy, we add the rules to the proxy
    // which makes it so that when we do connection.into() to get a stream
    // we can think of the rules being *inside* that connection

    // Probably we have tried to go from dbus_proxy to stream, should have
    // documented the attempt but we didn't

    // Some code online seems to use select!, which merges multiple async sources into one
    // We should think if select! + multiple streams is better. The current approach is
    // One stream, multiple match rules, branch out depending on the type of event

    // None to signal a fail: (which ok, there is more monitoring to do)
    let battery_rule: Option<MatchRule> = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.UPower")
        .map_err(|e| error!("Failed to set sender in match rule: {}", e))
        .ok()
        .and_then(|builder|
            builder.interface("org.freedesktop.DBus.Properties")
            .map_err(|e| error!("Failed to set interface in match rule: {}", e))
            .ok())
        .and_then(|builder|
            builder.member("PropertiesChanged")
            .map_err(|e|
            error!("Failed to set member in match rule: {}", e))
            .ok())
        .and_then(|builder|
            builder.path("/org/freedesktop/UPower/devices/battery_BAT0")
            .map_err(|e| error!("Failed to set path in match rule: {}", e)
            ).ok())
        .and_then(|builder| Some(builder.build()));


    if let Some(x) = battery_rule {
        dbus_proxy.add_match_rule(x)
            .await
            .map_err(|e| {
                error!("Failed to add D-Bus match rule for battery property changes: {}", e);
            })
            .ok();
    }

    let bt_interface_added_rule: Option<MatchRule> = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.bluez")
        .map_err(|e|
            error!("Failed to set sender in Bluetooth match rule: {}", e))
        .ok()
        .and_then(|builder|
            builder.interface("org.freedesktop.DBus.ObjectManager")
            .map_err(|e|
            error!("Failed to set interface in Bluetooth match rule: {}", e))
            .ok())
        .and_then(|builder|
            builder.member("InterfacesAdded")
            .map_err(|e|
            error!("Failed to set member in Bluetooth match rule: {}", e))
            .ok())
        .and_then(|builder| Some(builder.build()));

    if let Some(x) = bt_interface_added_rule {
        dbus_proxy.add_match_rule(x)
            .await
            .map_err(|e| {
                error!("Failed to add Bluetooth InterfacesAdded match rule: {}", e);
            })
            .ok();
    }

    // Match rule for Bluetooth device disconnections
    let bt_interface_removed_rule: Option<MatchRule> = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.bluez")
        .map_err(|e| error!("Failed to set sender in Bluetooth InterfacesRemoved match rule: {}", e))
        .ok()
        .and_then(|builder|
            builder.interface("org.freedesktop.DBus.ObjectManager")
            .map_err(|e| 
            error!("Failed to set interface in Bluetooth InterfacesRemoved match rule: {}", e))
            .ok())
        .and_then(|builder|
            builder.member("InterfacesRemoved")
            .map_err(|e|
            error!("Failed to set member in Bluetooth InterfacesRemoved match rule: {}", e))
            .ok())
        .and_then(|builder| Some(builder.build()));

    if let Some(x) = bt_interface_removed_rule {
        dbus_proxy.add_match_rule(x)
            .await
            .map_err(|e| {
                error!("Failed to add Bluetooth InterfacesRemoved match rule: {}", e);
            })
            .ok();
    }

    let mut stream: zbus::MessageStream = connection.into();
    info!("Dbus monitor: Starting to listen for D-Bus messages");

    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else {
            error!(
                "Error receiving DBus message in the dbus monitor loop: {:?}",
                msg.err()
            );
            continue;
        };

        debug!("Got an event in event stream: {:?}", msg);

        let header = msg.header();

        // We are listening to only signals, which by the spec they should have path, interface and
        // member present, so we skip the loop if not
        debug!("Dbus monitor: Received D-Bus message from path: {:?}, interface: {:?}, member: {:?}",
               header.path(), header.interface(), header.member());

        let path = match header.path() {
            Some(path) => path.as_str(),
            None => {
                error!("Dbus monitor: Received message with no path, ignoring");
                continue;
            }
        };

        let member = match header.member() {
            Some(m) => m.as_str(),
            None => {
                debug!("Dbus monitor: Message has no member field: {:?}", header.member());
                continue;
            }
        };

        let interface = match header.interface() {
            Some(interface) => interface.as_str(),
            None => {
          debug!("Dbus monitor: Message has no interface field: {:?}", header.interface());
                continue;
            }
        };

        info!("Dbus monitor: Received signal");

        match (path, interface, member) {
            (_, "org.freedesktop.DBus.ObjectManager", "InterfacesAdded") => {
                info!("Dbus monitor: Received InterfacesAdded signal from ObjectManager");
                let body = msg.body();
                let Ok(body_deserialized) = body.deserialize::<zvariant::Structure>() else {
                    error!("Dbus monitor: Failed to deserialize InterfacesAdded message body as Structure");
                    continue;
                };

                let fields = body_deserialized.fields();

                // Destructure into two separate Values first
                let (object_path_value, interfaces_dict_value) = match fields {
                    [a, b] => (a, b),
                    other => {
                        error!("Dbus monitor: Expected exactly 2 fields, got: {}", other.len());
                        continue;
                    }
                };

                // TODO: Add nested function here to extract and validate object path
                // fn extract_object_path(value: &Value) -> Result<&str, String> {
                //     match value {
                //         Value::ObjectPath(path) => Ok(path.as_str()),
                //         other => Err(format!("Expected ObjectPath, got: {:?}", other))
                //     }
                // }
                // This will allow other InterfacesAdded handling code to reuse path extraction

                let interfaces_and_properties = match interfaces_dict_value {
                    Value::Dict(dict) => dict,
                    other => {
                        error!("Dbus monitor: Expected Dict as second field, got: {:?}", other);
                        continue;
                    }
                };

                // Create longer-lived Str bindings
                let bluetooth_interface_key = zvariant::Str::from("org.bluez.Device1");
                let upower_interface_key = zvariant::Str::from("org.freedesktop.UPower.Device");

                // Debug: print all available interfaces in the dict
                debug!("Available interfaces in InterfacesAdded: {:?}", 
                       interfaces_and_properties.iter().map(|(k, _v)| k).collect::<Vec<_>>());

                let mut device_name: Option<String> = None;
                match interfaces_and_properties.get::<_, Value>(&bluetooth_interface_key) {
                    Ok(Some(Value::Dict(device1))) => {
                        debug!("Found Device1 interface properties: {:?}", device1);
                        // TODO: use alias, if alias fails use name and log that that is
                        // not supposed to happend by the bluez device api
                        // also alias is not supposed to be empty
                        match device1.get(&zvariant::Str::from("Name")) {
                            Ok(Some(Value::Str(name))) => {
                                debug!("Found Bluetooth device name: {}", name);
                                device_name = Some(name.to_string());
                            },
                            Ok(Some(other)) => {
                                error!("Device Name property has unexpected type: {:?}", other);
                            },
                            Ok(None) => {
                                error!("Device1 interface found but no Name property");
                            },
                            Err(e) => {
                                error!("Failed to get Name property from Device1 interface: {}", e);
                            },
                        }
                        // Update existing device or create new one in HashMap
                        if let Value::ObjectPath(object_path) = object_path_value {
                            if let Some(device) = bluetooth_devices.get_mut(object_path.as_str()) {
                                // Update existing device with name
                                // maybe allow yourself to update even if none?
                                device.device_name = device_name.clone();
                                info!("Updated existing device {} with name: {:?}", object_path, device_name);
                            } else {
                                // Create new device entry
                                bluetooth_devices.insert(object_path.to_string(), BluetoothDevice {
                                    device_path: object_path.to_string(),
                                    has_battery: false,
                                    has_media: false,
                                    battery_percentage: None,
                                    device_name: device_name.clone(),
                                });
                                info!("Created new device {} with name: {:?}", object_path, device_name);
                            }
                        } else {
                            error!("Expected ObjectPath for device path, got: {:?}", object_path_value);
                        }
                    },
                    Ok(Some(other)) => {
                        error!("Device1 interface found but has unexpected type: {:?}", other);
                    },
                    Ok(None) => {
                        debug!("Device1 interface not found in interfaces");
                    },
                    Err(e) => {
                        error!("Failed to get Device1 interface: {}", e);
                    }
                }
                // Check for Bluetooth MediaControl1 interface (indicates media device connection)
                let media_control_key = zvariant::Str::from("org.bluez.MediaControl1");
                // TODO: split Ok and Some for better logging
                // TODO: incorporate if let Stuff() instead of two branched match statements
                if let Ok(Some(_)) = interfaces_and_properties.get::<_, Value>(&media_control_key) {
                    info!("Dbus monitor: Bluetooth media device connected");
                    // Update HashMap with media capability
                    if let Value::ObjectPath(object_path) = object_path_value {
                        if let Some(device) = bluetooth_devices.get_mut(object_path.as_str()) {
                            device.has_media = true;
                            info!("Updated device {} with media capability", object_path);
                        } else {
                            debug!("Creating new device in hashmap for media: {}", object_path);
                            bluetooth_devices.insert(object_path.to_string(), BluetoothDevice {
                                device_path: object_path.to_string(),
                                has_battery: false,
                                has_media: true,
                                battery_percentage: None,
                                device_name: None,
                            });
                            info!("Created new device {} with media capability via InterfacesAdded", object_path);
                        }
                    } else {
                        error!("Expected ObjectPath for media device path field, got: {:?}. Skipping update to bluetooth_devices", object_path_value);
                    }
                };

                match interfaces_and_properties.get::<_, Value>(&zvariant::Str::from("org.bluez.Battery1")) {
                    Err(e) => {
                        error!("Failed to get bluetooth battery interface: {}", e);
                    },
                    Ok(None) => {
                        debug!("Not a device with org.bluez.Battery1 interface");
                    },
                    Ok(Some(battery_interface_value)) => {
                        let percentage = process_bluetooth_battery_interface(&battery_interface_value);
                        // Update HashMap with new battery percentage
                        if let Value::ObjectPath(object_path) = object_path_value {
                            if let Some(device) = bluetooth_devices.get_mut(object_path.as_str()) {
                                device.has_battery = true;
                                device.battery_percentage = percentage;
                                info!("Updated device {} battery: {:?}%", object_path, percentage);
                            } else {
                                debug!("Creating new device in hashmap: {}", object_path);
                                bluetooth_devices.insert(object_path.to_string(), BluetoothDevice {
                                    device_path: object_path.to_string(),
                                    has_battery: true,
                                    has_media: false,
                                    battery_percentage: percentage,
                                    device_name: None,
                                });
                                info!("Created new device {} with battery: {:?}% via InterfacesAdded", object_path, percentage);
                            }
                            
                            // Send GUI update for all Bluetooth devices
                            let display_string = compute_bluetooth_display_string(&bluetooth_devices);
                            if let Some(sender) = BLUETOOTH_SENDER.get() {
                                if let Err(e) = sender.send(display_string) {
                                    error!("Failed to send Bluetooth battery update to GUI: {}", e);
                                }
                            } else {
                                error!("Bluetooth sender not initialized, cannot send GUI update");
                            }
                        } else {
                            error!("Expected ObjectPath for object path field, got: {:?}. Skiping update to bluetooth_devices", object_path_value);
                        }
                    }
                };



                // Check for UPower Device interface
                if let Some(Value::Dict(_battery_props)) = interfaces_and_properties
                    .get::<_, Value>(&upower_interface_key)
                    .ok()
                    .flatten() {
                    info!("Dbus monitor: Battery device added");
                    // Possibly refresh battery information or re-subscribe if needed
                }

            }
            (_, "org.freedesktop.DBus.Properties", "PropertiesChanged") => {
                info!("Dbus monitor: Received PropertiesChanged signal");
                let body = msg.body();
                let Ok(body_deserialized) = body.deserialize::<zvariant::Structure>() else {
                    error!("Dbus monitor: Failed to deserialize PropertiesChanged message body as Structure");
                    continue;
                };
                let fields = body_deserialized.fields();
                let (interface_name_val, changed_properties_val, invalidated_properties) = match fields {
                    [a, b, c] => (a, b, c),
                    other => {
                        error!("Dbus monitor: Expected exactly 3 fields, got: {}", other.len());
                        continue;
                    }
                };
                // Convert name, match if it is battery
                let interface_names = match interface_name_val {
                    Value::Str(val) => val,
                    other => {
                        error!("Dbus monitor: Expected interface name to be a string, got: {:?}", other);
                        continue;
                    }
                };

                match interface_names.as_str() {
                    "org.freedesktop.UPower.Device" => {
                        let changed_properties = match changed_properties_val {
                            Value::Dict(dict) => dict,
                            other => {
                                error!("Dbus monitor: Expected Dict for changed_properties, got: {:?}", other);
                                continue;
                            }
                        };

                        // Use the new battery properties processing function
                        process_battery_device_properties(changed_properties);

                        // Use dedicated function for percentage changes
                        let percentage_key = Value::Str("Percentage".into());
                        if let Ok(Some(percentage_value)) = changed_properties.get::<_, Value>(&percentage_key) {
                            process_battery_percentage(percentage_value).await;
                        }

                    }
                    "org.bluez.Battery1" => {
                        let Value::Dict(_) = changed_properties_val else {
                            error!("Dbus monitor: Expected Dict for changed_properties, got: {:?}", changed_properties_val);
                            continue;
                        };

                        // Use the existing function by passing changed properties as Value::Dict
                        let percentage = process_bluetooth_battery_interface(changed_properties_val);
                        // Update HashMap with new battery percentage
                        if let Some(device) = bluetooth_devices.get_mut(path) {
                            device.battery_percentage = percentage;
                            info!("Updated device {} battery via PropertiesChanged: {:?}%", path, percentage);
                        } else {
                            error!("Device Battery1 property change that wasn't previously on the hashmap");
                            info!("Creating new device in hashmap for battery via PropertiesChanged: {}", path);
                            bluetooth_devices.insert(path.to_string(), BluetoothDevice {
                                device_path: path.to_string(),
                                has_battery: true,
                                has_media: false,
                                battery_percentage: percentage,
                                device_name: None, // TODO: Extract device name if available
                            });
                            info!("Created new device {} with battery capability via PropertiesChanged", path);
                        }
                        
                        // Send GUI update for all Bluetooth devices
                        let display_string = compute_bluetooth_display_string(&bluetooth_devices);
                        if let Some(sender) = BLUETOOTH_SENDER.get() {
                            if let Err(e) = sender.send(display_string) {
                                error!("Failed to send Bluetooth battery update to GUI: {}", e);
                            }
                        } else {
                            error!("Bluetooth sender not initialized, cannot send GUI update");
                        }
                    }
                    "org.bluez.MediaControl1" => {
                        info!("Dbus monitor: MediaControl1 properties changed for {}", path);
                        // Update HashMap with media capability if device exists
                        if let Some(device) = bluetooth_devices.get_mut(path) {
                            device.has_media = true;
                            info!("Updated device {} with media capability via PropertiesChanged", path);
                        } else {
                            error!("Device MediaControl1 property change that wasn't previously on the hashmap");
                            info!("Creating new device in hashmap for media via PropertiesChanged: {}", path);
                            bluetooth_devices.insert(path.to_string(), BluetoothDevice {
                                device_path: path.to_string(),
                                has_battery: false,
                                has_media: true,
                                battery_percentage: None,
                                device_name: None,
                            });
                            info!("Created new device {} with media capability via PropertiesChanged", path);
                        }
                        // TODO: Process specific MediaControl1 properties if needed
                    }
                    other => {
                        debug!("Dbus monitor: Ignored PropertiesChanged for interface: {:?}", other);
                        continue;
                    }
                };

            }
            (_, "org.freedesktop.DBus.ObjectManager", "InterfacesRemoved") => {
                info!("Dbus monitor: Received InterfacesRemoved signal from ObjectManager");
                let body = msg.body();
                let Ok(body_deserialized) = body.deserialize::<zvariant::Structure>() else {
                    error!("Dbus monitor: Failed to deserialize InterfacesRemoved message body as Structure");
                    continue;
                };
                let fields = body_deserialized.fields();
                let (object_path_value, interfaces_array_value) = match fields {
                    [a, b] => (a, b),
                    other => {
                        error!("Dbus monitor: Expected exactly 2 fields in InterfacesRemoved, got: {}", other.len());
                        continue;
                    }
                };

                let object_path = match object_path_value {
                    Value::ObjectPath(object_path) => object_path,
                    other => {
                        error!("Dbus monitor: Expected ObjectPath as first element, got {:?}", other);
                        continue;
                    }
                };

                let interfaces = match interfaces_array_value {
                    Value::Array(arr) => arr,
                    other => {
                        error!("Dbus monitor: Expected Array as second element, got {:?}", other);
                        continue;
                    }
                };

                debug!("Dbus monitor: Interfaces removed from {}: {:?}", object_path, interfaces);

                // Check for bt battery or media interfaces and handle them
                for iface in interfaces.iter() {
                    if let Value::Str(interface_name) = iface {
                        match interface_name.as_str() {
                            "org.bluez.Battery1" => {
                                info!("Dbus monitor: Bluetooth battery interface removed from {}", object_path);
                                let object_path_str = object_path.as_str();
                                if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
                                    device.has_battery = false;
                                    device.battery_percentage = None;
                                    info!("Updated device {} to remove battery capability", object_path);

                                    // Remove device entirely if it has no useful interfaces or name left
                                    if !device.has_media && !device.has_battery && device.device_name.is_none() {
                                        bluetooth_devices.remove(object_path_str);
                                        info!("Removed device {} from HashMap (no battery, media, or name)", object_path);
                                    }
                                } else {
                                    debug!("Battery interface removed from device not in HashMap: {}", object_path);
                                }
                            }
                            "org.bluez.MediaControl1" => {
                                info!("Dbus monitor: Bluetooth media interface removed from {}", object_path);
                                let object_path_str = object_path.as_str();
                                if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
                                    device.has_media = false;
                                    info!("Updated device {} to remove media capability", object_path);

                                    // Remove device entirely if it has no useful interfaces or name left
                                    if !device.has_media && !device.has_battery && device.device_name.is_none() {
                                        bluetooth_devices.remove(object_path_str);
                                        info!("Removed device {} from HashMap (no battery, media, or name)", object_path);
                                    }
                                } else {
                                    debug!("Media interface removed from device not in HashMap: {}", object_path);
                                }
                            }
                            "org.bluez.Device1" => {
                                info!("Dbus monitor: Bluetooth Device1 interface removed from {}", object_path);
                                let object_path_str = object_path.as_str();
                                if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
                                    device.device_name = None;
                                    info!("Cleared device name for {}", object_path);

                                    // Remove device entirely if it has no useful interfaces or name left
                                    if !device.has_media && !device.has_battery && device.device_name.is_none() {
                                        bluetooth_devices.remove(object_path_str);
                                        info!("Removed device {} from HashMap (no battery, media, or name)", object_path);
                                    }
                                } else {
                                    debug!("Device1 interface removed from device not in HashMap: {}", object_path);
                                }
                            }
                            "org.freedesktop.UPower.Device" => {
                                info!("Dbus monitor: UPower battery interface removed from {}", object_path);
                                // TODO: Handle cleanup or UI update for removed battery device
                            }
                            _ => {}
                        }
                    }
                }

                // Send GUI update after any Bluetooth device removal
                let display_string = compute_bluetooth_display_string(&bluetooth_devices);
                if let Some(sender) = BLUETOOTH_SENDER.get() {
                    if let Err(e) = sender.send(display_string) {
                        error!("Failed to send Bluetooth battery update to GUI after device removal: {}", e);
                    }
                } else {
                    error!("Bluetooth sender not initialized, cannot send GUI update after device removal");
                }

            }
            _ => {
                warn!("Dbus monitor: Unhandled signal: path: {}, interface: {}, member: {}", path, interface, member);
            }
        }

    }

    error!("Dbus monitor: Message stream ended unexpectedly");

    Ok(())
}

fn activate(application: &gtk::Application) -> Result<()> {
    info!("Activating GTK application");

    let window = gtk::ApplicationWindow::new(application);
    window.add_css_class("layer-bar");

    load_css_styles(&window)?;
    configure_layer_shell(&window)?;

    let (bar, bt_widget, volume_widget, battery_widget, time_widget, workspace_widget, title_widget) = create_experimental_bar()?;
    window.set_child(Some(&bar));
    window.show();

    update_time_widget(time_widget)?;
    setup_workspace_updates(workspace_widget, title_widget.clone())?;
    setup_title_updates(title_widget)?;
    setup_battery_updates(battery_widget)?;
    setup_bluetooth_updates(bt_widget)?;
    setup_volume_updates(volume_widget)?;

    info!("Application activated successfully");
    Ok(())
}

fn create_tokio_runtime() -> Result<tokio::runtime::Runtime> {
    debug!("Creating tokio runtime");

    tokio::runtime::Runtime::new()
        .context("Failed to create Tokio runtime")
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

    Ok(())
}
