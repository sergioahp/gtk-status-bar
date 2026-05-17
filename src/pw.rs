// PipeWire subsystem: track audio sink volumes and report changes for the
// default sink. PipeWire's C-style callback model needs `Rc<RefCell<…>>` for
// shared state inside the dedicated thread; that's why this module looks very
// different from the tokio-driven hyprland/dbus subsystems. ThreadLoop owns
// the event loop; we hand it a registry listener and let it dispatch.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use pipewire as pw;
use pw::spa::pod::{Pod, Value as PodValue, ValueArray, deserialize::PodDeserializer};
use pw::spa::param::ParamType;
use pw::{
    device::Device,
    node::Node,
    proxy::{Listener, ProxyT},
    thread_loop::ThreadLoop,
    types::ObjectType,
    metadata::Metadata,
};

use crate::bus::VolumeUpdate;

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
    let media_class = props.and_then(|p| p.get("media.class"));
    debug!("🔍 Checking node - media.class: {:?}", media_class);

    let result = media_class
         // monitor only sinks for now
         .map(|c| c.contains("Audio") && c.contains("Sink"))
         .unwrap_or(false);

    debug!("🔍 Node filter result: {} for media.class: {:?}", result, media_class);
    result
}

fn is_audio_device(props: &Option<&pw::spa::utils::dict::DictRef>) -> bool {
    props.and_then(|p| p.get("device.api"))
         .map(|api| api == "alsa" || api == "bluez5")
         .unwrap_or(false)
}

fn parse_volume_from_pod(param: &Pod) -> Option<(Option<u8>, Option<u8>, Option<bool>)> {
    let obj = param.as_object().ok()?;
    let mut volume: Option<f32> = None;
    let mut mute: Option<bool> = None;
    let mut channel_volumes: Vec<f32> = Vec::new();

    for prop in obj.props() {
        let key = prop.key().0;
        let value_pod = prop.value();

        match key {
            pw::spa::sys::SPA_PROP_volume => {
                if let Ok(vol) = value_pod.get_float() {
                    volume = Some(vol);
                }
            },
            pw::spa::sys::SPA_PROP_mute => {
                if let Ok(m) = value_pod.get_bool() {
                    mute = Some(m);
                }
            },
            pw::spa::sys::SPA_PROP_channelVolumes => {
                if let Ok((_, PodValue::ValueArray(ValueArray::Float(volumes)))) =
                    PodDeserializer::deserialize_any_from(value_pod.as_bytes()) {
                    channel_volumes = volumes;
                }
            },
            _ => {}
        }
    }

    // Convert to structured data with cube root transformation (like wpctl)
    let volume_percent = volume.map(|v| (v.powf(1.0/3.0) * 100.0).round() as u8);
    let channel_percent = channel_volumes.first().map(|&v| (v.powf(1.0/3.0) * 100.0).round() as u8);

    // Return None only if we have no volume data at all
    if volume_percent.is_none() && channel_percent.is_none() && mute.is_none() {
        return None;
    }

    Some((volume_percent, channel_percent, mute))
}

// Start PipeWire monitoring on dedicated ThreadLoop thread
pub fn start_pipewire_thread(sender: mpsc::UnboundedSender<VolumeUpdate>) -> Result<()> {
    std::thread::spawn(move || {
        debug!("🔧 Initializing PipeWire on dedicated thread...");

        // Track the default sink name (not ID, since metadata uses names)
        let default_sink_name = Rc::new(RefCell::new(None::<String>));

        // Create HashMap to track device_id -> (node_name, description, volume_percent, channel_percent, is_muted)
        let device_map = Rc::new(RefCell::new(HashMap::<u32, (String, String, Option<u8>, Option<u8>, Option<bool>)>::new()));
        debug!("📋 Created device tracking HashMap for (node_name, description, volume, channel, mute)");

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

        debug!("🎵 PipeWire ThreadLoop started - monitoring volume changes with default sink filtering");

        // Set up metadata listener for default sink detection
        let registry_weak_metadata = Rc::downgrade(&registry);
        let default_sink_name_for_metadata = Rc::clone(&default_sink_name);
        let device_map_for_metadata = Rc::clone(&device_map);
        let sender_for_metadata = sender.clone();

        // Metadata listener for default sink tracking
        let _metadata_registry_listener = registry
            .add_listener_local()
            .global(move |obj| {
                // GPT-5 says:
                // Only the "default" metadata carries default.audio.sink/source
                let meta_name = obj.props
                    .and_then(|p| p.get("metadata.name"))
                    .unwrap_or("");
                if meta_name != "default" {
                    debug!("🚫 Skipping metadata '{}' (props: {:?})", meta_name, obj.props);
                    return;
                }
                debug!("✅ Processing metadata.name == default");

                if let Some(reg) = registry_weak_metadata.upgrade() {
                    if obj.type_ == ObjectType::Metadata {
                        debug!("📋 Found metadata object: {:?}", obj.props);

                        let metadata: Metadata = reg.bind(obj).unwrap();
                        let meta_id = metadata.upcast_ref().id();
                        debug!("📋 Bound 'default' Metadata (id={})", meta_id);

                        let default_sink_weak = Rc::downgrade(&default_sink_name_for_metadata);
                        let device_map_weak_metadata = Rc::downgrade(&device_map_for_metadata);
                        let sender_clone_metadata = sender_for_metadata.clone();

                        // Listen for property changes
                        let meta_listener = metadata
                            .add_listener_local()
                            .property(move |subject, key, _type_, value| {
                                debug!("📝 metadata.property subject={:?} key={:?} value={:?}", subject, key, value);

                                // Handle empty/None cases explicitly
                                match (key, value) {
                                    (None, _) => debug!("🚫 Skipping metadata property: key is None"),
                                    (_, None) => debug!("🚫 Skipping metadata property: value is None for key {:?}", key),
                                    (Some(k), Some(_v)) if k.is_empty() => debug!("🚫 Skipping metadata property: empty key"),
                                    (Some(k), Some(v)) if v.is_empty() => debug!("🚫 Skipping metadata property: empty value for key '{}'", k),
                                    (Some(k), Some(v)) => {
                                        debug!("🔍 Processing metadata property: {}={}", k, v);
                                        if k == "default.audio.sink" {
                                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(v) {
                                                if let Some(name) = json.get("name").and_then(|n| n.as_str()) {
                                                    if let Some(default_sink) = default_sink_weak.upgrade() {
                                                        let previous_sink = default_sink.borrow().clone();
                                                        *default_sink.borrow_mut() = Some(name.to_string());
                                                        info!("🔄 Default sink -> {}", name);
                                                        debug!("🎯 SINK CHANGE: {:?} -> {} (should trigger volume fetch)", previous_sink, name);

                                                        // Find the device ID that matches this node name and update GUI
                                                        if let Some(device_map) = device_map_weak_metadata.upgrade() {
                                                            debug!("🗂️ Searching device map for default sink '{}'", name);

                                                            // Log all devices we have tracked
                                                            if let Ok(map) = device_map.try_borrow() {
                                                                debug!("🗂️ Current device map contents: {:?}", *map);

                                                                // Match by node.name (first element of tuple)
                                                                let mut found_device = false;
                                                                for (device_id, (node_name, device_description, cached_vol, cached_ch, cached_mute)) in map.iter() {
                                                                    debug!("🔍 Checking device {}: node_name='{}', description='{}' against default sink '{}'",
                                                                           device_id, node_name, device_description, name);

                                                                    if node_name == name {
                                                                        debug!("🎯 MATCH! Found device {} with node_name '{}' matching default sink", device_id, node_name);
                                                                        debug!("🎨 Updating GUI label to: '{}' with cached volume data", device_description);
                                                                        debug!("💾 Cached volume data: Vol: {:?}%, Ch: {:?}%, Mute: {:?}", cached_vol, cached_ch, cached_mute);

                                                                        // Use cached volume data if available, otherwise use reasonable defaults
                                                                        let volume_percent = *cached_vol;
                                                                        let channel_percent = *cached_ch;
                                                                        let is_muted = *cached_mute;

                                                                        // Send GUI update with real cached volume data
                                                                        let update = VolumeUpdate {
                                                                            id: *device_id,
                                                                            name: device_description.clone(),
                                                                            volume_percent,
                                                                            channel_percent,
                                                                            is_muted,
                                                                        };
                                                                        if let Err(e) = sender_clone_metadata.send(update) {
                                                                            error!("❌ Failed to send device name update to GUI: {}", e);
                                                                        } else {
                                                                            debug!("✅ Sent REAL volume data to GUI: '{}' Vol: {:?}%, Ch: {:?}%, Mute: {:?}",
                                                                                   device_description, volume_percent, channel_percent, is_muted);
                                                                        }
                                                                        found_device = true;
                                                                        break; // Found the match, stop searching
                                                                    }
                                                                }

                                                                if !found_device {
                                                                    warn!("⚠️ Default sink '{}' not found in device map! Map has {} entries", name, map.len());
                                                                    debug!("🗂️ Available node names: {:?}",
                                                                           map.values().map(|(node_name, _, _, _, _)| node_name).collect::<Vec<_>>());
                                                                }
                                                            } else {
                                                                error!("❌ Failed to borrow device_map when default sink changed to '{}'", name);
                                                            }
                                                        } else {
                                                            error!("❌ device_map_weak upgrade failed when default sink changed to '{}'", name);
                                                        }
                                                    }
                                                }
                                            } else {
                                                warn!("❌ default.audio.sink value is not JSON: {}", v);
                                            }
                                        } else {
                                            debug!("🔧 Other metadata property: {} (ignored)", k);
                                        }
                                    }
                                }
                                0
                            })
                            .register();

                        // Keep both proxy and listener alive
                        let proxy: Box<dyn ProxyT> = Box::new(metadata);
                        let keep_weak = Rc::downgrade(&keep_alive);
                        let removed = proxy.upcast_ref()
                            .add_listener_local()
                            .removed(move || {
                                if let Some(k) = keep_weak.upgrade() {
                                    k.borrow_mut().remove(meta_id);
                                }
                            })
                            .register();

                        keep_alive.borrow_mut().add_proxy(proxy, Box::new(meta_listener));
                        keep_alive.borrow_mut().add_listener(meta_id, Box::new(removed));
                    }
                }
            })
            .register();

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

                            // Get node.name for default sink matching
                            let node_name = obj.props
                                .and_then(|p| p.get("node.name"))
                                .unwrap_or("")
                                .to_string();

                            debug!("📱 Monitoring audio node: {} ({}) [node.name: {}]", name, id, node_name);
                            debug!("🔗 ADDING NODE LISTENER for node.name: {}", node_name);

                            // Add device to tracking HashMap with node.name, description, and initial empty volume data
                            if let Some(mut device_map) = device_map.clone().try_borrow_mut().ok() {
                                device_map.insert(id, (node_name.clone(), name.clone(), None, None, None));
                                debug!("📝 Added device to HashMap: {} -> ({}, {}, no volume yet)", id, node_name, name);
                                debug!("🗂️ Current device map size: {}", device_map.len());
                            } else {
                                error!("❌ Failed to borrow device_map for insertion of device {} ({})", id, name);
                            }

                            node.subscribe_params(&[
                                ParamType::Props,
                                ParamType::Route,
                            ]);

                            let name_clone = name.clone();
                            let node_name_clone = node_name.clone();
                            let sender_clone = sender.clone();
                            let default_sink_weak = Rc::downgrade(&default_sink_name);
                            let device_map_weak = Rc::downgrade(&device_map);
                            let node_listener = node
                                .add_listener_local()
                                .param(move |_seq, param_type, _idx, _next, param| {
                                    if param_type == ParamType::Props {
                                        debug!("🎛️  NODE PARAM CALLBACK: {} ({}) received Props param", name_clone, id);
                                        if let Some(pod) = param {
                                            if let Some((volume_percent, channel_percent, is_muted)) = parse_volume_from_pod(pod) {
                                                debug!("🔊 Node {}: {} - Vol: {:?}% | Ch: {:?}% | Mute: {:?} [CACHING]",
                                                       id, name_clone, volume_percent, channel_percent, is_muted);

                                                // Update device volume in HashMap for ALL devices
                                                if let Some(device_map) = device_map_weak.upgrade() {
                                                    if let Ok(mut map) = device_map.try_borrow_mut() {
                                                        if let Some((_node_name, description, old_vol, old_ch, old_mute)) = map.get_mut(&id) {
                                                            *old_vol = volume_percent;
                                                            *old_ch = channel_percent;
                                                            *old_mute = is_muted;
                                                            debug!("📝 Updated volume cache for device {}: {} -> Vol: {:?}%, Ch: {:?}%, Mute: {:?}",
                                                                   id, description, volume_percent, channel_percent, is_muted);
                                                        } else {
                                                            debug!("⚠️ Device {} not found in HashMap during volume update", id);
                                                        }
                                                    } else {
                                                        error!("❌ Failed to borrow device_map for volume update of device {}", id);
                                                    }
                                                } else {
                                                    error!("❌ device_map_weak upgrade failed during volume update for device {}", id);
                                                }

                                                // Check if this is the default sink for GUI updates
                                                let is_default = if let Some(default_sink) = default_sink_weak.upgrade() {
                                                    let current_default = default_sink.borrow();
                                                    let result = current_default.as_ref().map_or(false, |default| {
                                                        node_name_clone == *default
                                                    });
                                                    debug!("🎯 Checking if device {} is default: current_default={:?}, node_name={}, is_default={}",
                                                           id, current_default, node_name_clone, result);
                                                    result
                                                } else {
                                                    debug!("⚠️ Cannot check default status: default_sink_weak upgrade failed for device {}", id);
                                                    false
                                                };

                                                if is_default {
                                                    debug!("📤 SENDING VOLUME UPDATE to GUI for default sink");

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
                                                } else {
                                                    debug!("📊 Cached volume for non-default device {} ({})", id, name_clone);
                                                }
                                            }
                                        }
                                    }
                                })
                                .register();

                            let proxy: Box<dyn ProxyT> = Box::new(node);
                            let proxy_id = proxy.upcast_ref().id();
                            let keep_weak = Rc::downgrade(&keep);
                            let device_map_weak_remove = Rc::downgrade(&device_map);
                            let removed_listener = proxy.upcast_ref()
                                .add_listener_local()
                                .removed(move || {
                                    debug!("🗑️ Node {} removed, cleaning up", proxy_id);
                                    if let Some(device_map) = device_map_weak_remove.upgrade() {
                                        if let Ok(mut map) = device_map.try_borrow_mut() {
                                            if let Some((removed_node_name, removed_description, _, _, _)) = map.remove(&proxy_id) {
                                                debug!("✅ Removed device from HashMap: {} -> ({}, {})", proxy_id, removed_node_name, removed_description);
                                                debug!("🗂️ Device map size after removal: {}", map.len());
                                            } else {
                                                debug!("⚠️ Device {} was not in HashMap when removed", proxy_id);
                                            }
                                        } else {
                                            error!("❌ Failed to borrow device_map for removal of device {}", proxy_id);
                                        }
                                    } else {
                                        error!("❌ device_map_weak upgrade failed when removing device {}", proxy_id);
                                    }

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
                                ParamType::Props,
                                ParamType::Route,
                            ]);

                            let name_clone = name.clone();
                            let sender_clone = sender.clone();
                            let device_listener = device
                                .add_listener_local()
                                .param(move |_seq, param_type, _idx, _next, param| {
                                    if param_type == ParamType::Props {
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
        let (_stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        // Block this OS thread until shutdown is requested (no wasteful sleep loop!)
        // ThreadLoop::start() already manages its own internal event thread
        stop_rx.recv().ok();

        debug!("🛑 Shutdown requested, stopping ThreadLoop...");
        thread_loop.stop();
        debug!("✅ ThreadLoop stopped gracefully");
    });

    Ok(())
}
