// D-Bus subsystem: UPower battery + BlueZ bluetooth device tracking.
//
// monitor_dbus() opens one system-bus connection, does an initial query of the
// battery and the bluetooth ObjectManager to seed the local HashMap, then
// subscribes to three signal types via MatchRules (PropertiesChanged,
// InterfacesAdded, InterfacesRemoved) and dispatches each incoming signal in a
// big match over (path, interface, member). Local HashMap<path, BluetoothDevice>
// is the source of truth for the bluetooth display string; battery state is
// pushed through BATTERY_SENDER directly.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use tracing::{debug, error, info, warn};
use zbus::Connection;
use zbus::MatchRule;
use zbus::fdo;
use zbus::message::Type as MessageType;
use zbus::zvariant;
use zbus::zvariant::Value;
use zbus_names::InterfaceName;

use crate::bus;

// UNSAFE assumtion for now: assume Battery1 and MediaTransport1 are on the same object when they
// exist, but a device could have just one of them or non
#[derive(Debug, Clone)]
pub struct BluetoothDevice {
    pub device_path: String,
    pub has_battery: bool,
    pub has_media: bool,
    pub battery_percentage: Option<u8>,
    pub device_name: Option<String>,
}

pub fn compute_bluetooth_display_string(bluetooth_devices: &HashMap<String, BluetoothDevice>) -> String {
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
        "".to_string()  // Return empty string instead of "No BT" so widget gets hidden
    } else {
        device_strings.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(path: &str, name: Option<&str>, percentage: Option<u8>) -> (String, BluetoothDevice) {
        (
            path.to_string(),
            BluetoothDevice {
                device_path: path.to_string(),
                has_battery: percentage.is_some(),
                has_media: false,
                battery_percentage: percentage,
                device_name: name.map(str::to_string),
            },
        )
    }

    // Empty map => empty string (NOT "No BT"); the widget layer uses this as
    // the hide signal via set_visible(false).
    #[test]
    fn bt_display_empty_map_is_empty_string() {
        let map: HashMap<String, BluetoothDevice> = HashMap::new();
        assert_eq!(compute_bluetooth_display_string(&map), "");
    }

    // Devices without a battery percentage are filtered out entirely. If the
    // map only contains such devices, the result is the same as empty.
    #[test]
    fn bt_display_devices_without_battery_filtered() {
        let map: HashMap<String, BluetoothDevice> =
            [device("/d1", Some("Pixel"), None), device("/d2", None, None)]
                .into_iter()
                .collect();
        assert_eq!(compute_bluetooth_display_string(&map), "");
    }

    // One named device with battery: first char of name + integer percentage.
    #[test]
    fn bt_display_single_named_device() {
        let map: HashMap<String, BluetoothDevice> =
            [device("/d1", Some("Pixel Buds"), Some(80))].into_iter().collect();
        assert_eq!(compute_bluetooth_display_string(&map), "P80");
    }

    // Device with battery but no name falls back to 'D' (for "device").
    #[test]
    fn bt_display_device_no_name_uses_d_prefix() {
        let map: HashMap<String, BluetoothDevice> =
            [device("/d1", None, Some(42))].into_iter().collect();
        assert_eq!(compute_bluetooth_display_string(&map), "D42");
    }

    // First *character* (not byte) of the device name — verifies multi-byte
    // UTF-8 doesn't slice mid-byte.
    #[test]
    fn bt_display_multibyte_name_uses_first_char() {
        let map: HashMap<String, BluetoothDevice> =
            [device("/d1", Some("🎧 Sony"), Some(55))].into_iter().collect();
        assert_eq!(compute_bluetooth_display_string(&map), "🎧55");
    }

    // Two devices: assert via set comparison since HashMap iteration order is
    // not guaranteed. We split on space to avoid order-dependent equality.
    #[test]
    fn bt_display_two_devices_joined_by_space() {
        let map: HashMap<String, BluetoothDevice> = [
            device("/d1", Some("Pixel"), Some(80)),
            device("/d2", Some("Sony"), Some(60)),
        ]
        .into_iter()
        .collect();
        let out = compute_bluetooth_display_string(&map);
        let mut parts: Vec<&str> = out.split(' ').collect();
        parts.sort();
        assert_eq!(parts, vec!["P80", "S60"]);
    }
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

fn process_battery_percentage(value: Value<'_>) {
    if let Some(percentage) = f64::try_from(value)
        .inspect_err(|e| {
            error!("Failed to convert battery percentage to f64: {}", e);
        })
        .ok()
    {
        info!("Battery percentage changed to {:.1}%", percentage);
        let battery_text = format!("🔋 {:.0}%", percentage);
        if let Err(e) = bus::send_battery_update(battery_text) {
            error!("Failed to send battery update: {}", e);
        }
    }
}

fn process_battery_state(value: Value<'_>) {
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
            debug!("Battery device properties contain no State property");
        },
        Ok(Some(state_value)) => {
            process_battery_state(state_value.clone());
        }
    }
}

// MatchRule builders. Each .sender/.interface/.member/.path returns
// Result<MatchRuleBuilder, _>, so we use `?` with anyhow::Context to get a
// flat layered trace and let the caller match() on the final Result. Replaces
// the previous .map_err(...).ok().and_then(|builder| ...)-style chains that
// silently swallowed each failure and made the match rule end up as `None`
// with no aggregate trace.
fn build_battery_match_rule() -> Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.UPower")
            .context("battery rule: set sender")?
        .interface("org.freedesktop.DBus.Properties")
            .context("battery rule: set interface")?
        .member("PropertiesChanged")
            .context("battery rule: set member")?
        .path("/org/freedesktop/UPower/devices/battery_BAT0")
            .context("battery rule: set path")?
        .build())
}

fn build_bluez_object_manager_match_rule(member: &'static str) -> Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.bluez")
            .with_context(|| format!("bluez rule ({}): set sender", member))?
        .interface("org.freedesktop.DBus.ObjectManager")
            .with_context(|| format!("bluez rule ({}): set interface", member))?
        .member(member)
            .with_context(|| format!("bluez rule ({}): set member", member))?
        .build())
}

// Drop a bluetooth device from the map if it has lost every interface that
// would justify displaying it. We track devices via three booleans (battery,
// media, has-name) and any signal that flips one to false has to check whether
// the device is now empty.
fn remove_if_idle(devices: &mut HashMap<String, BluetoothDevice>, path: &str) {
    let Some(d) = devices.get(path) else { return };
    if !d.has_media && !d.has_battery && d.device_name.is_none() {
        devices.remove(path);
        info!("Removed device {} from HashMap (no battery, media, or name)", path);
    }
}

// ObjectManager.InterfacesAdded signal handler. Each signal carries one object
// path plus a dict of {interface_name => {property => value}}. We learn about
// Device1 (name), Battery1 (percentage), and MediaControl1 (presence) from
// here, and seed the local HashMap so subsequent PropertiesChanged signals
// have something to update. Early-returns replace `continue` in the parent
// loop; logging stays at the same call sites it was before extraction.
fn handle_interfaces_added(msg: &zbus::Message, bluetooth_devices: &mut HashMap<String, BluetoothDevice>) {
    info!("Dbus monitor: Received InterfacesAdded signal from ObjectManager");
    let body = msg.body();
    let Ok(body_deserialized) = body.deserialize::<zvariant::Structure>() else {
        error!("Dbus monitor: Failed to deserialize InterfacesAdded message body as Structure");
        return;
    };

    let fields = body_deserialized.fields();

    // Destructure into two separate Values first
    let (object_path_value, interfaces_dict_value) = match fields {
        [a, b] => (a, b),
        other => {
            error!("Dbus monitor: Expected exactly 2 fields, got: {}", other.len());
            return;
        }
    };

    // Both the object path and the interface dict get used three times below
    // (once per interface we recognize). Extract once up front via let-else;
    // bailing on a malformed body keeps the rest of the function un-nested.
    let Value::ObjectPath(object_path) = object_path_value else {
        error!("Dbus monitor: Expected ObjectPath as first field, got: {:?}", object_path_value);
        return;
    };
    let Value::Dict(interfaces_and_properties) = interfaces_dict_value else {
        error!("Dbus monitor: Expected Dict as second field, got: {:?}", interfaces_dict_value);
        return;
    };
    let object_path_str = object_path.as_str();

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
            if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
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
        if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
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
            if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
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
            let display_string = compute_bluetooth_display_string(bluetooth_devices);
            if let Err(e) = bus::send_bluetooth_update(display_string) {
                error!("Failed to send Bluetooth battery update: {}", e);
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

// Properties.PropertiesChanged: fired when the value of an existing property
// flips. We branch on which interface owns the property — UPower.Device for
// the laptop battery, Battery1/MediaControl1 for bluetooth devices.
fn handle_properties_changed(msg: &zbus::Message, path: &str, bluetooth_devices: &mut HashMap<String, BluetoothDevice>) {
    info!("Dbus monitor: Received PropertiesChanged signal");
    let body = msg.body();
    let Ok(body_deserialized) = body.deserialize::<zvariant::Structure>() else {
        error!("Dbus monitor: Failed to deserialize PropertiesChanged message body as Structure");
        return;
    };
    let fields = body_deserialized.fields();
    let (interface_name_val, changed_properties_val, _invalidated_properties) = match fields {
        [a, b, c] => (a, b, c),
        other => {
            error!("Dbus monitor: Expected exactly 3 fields, got: {}", other.len());
            return;
        }
    };
    // Convert name, match if it is battery
    let interface_names = match interface_name_val {
        Value::Str(val) => val,
        other => {
            error!("Dbus monitor: Expected interface name to be a string, got: {:?}", other);
            return;
        }
    };

    match interface_names.as_str() {
        "org.freedesktop.UPower.Device" => {
            let changed_properties = match changed_properties_val {
                Value::Dict(dict) => dict,
                other => {
                    error!("Dbus monitor: Expected Dict for changed_properties, got: {:?}", other);
                    return;
                }
            };

            // Use the new battery properties processing function
            process_battery_device_properties(changed_properties);

            // Use dedicated function for percentage changes
            let percentage_key = Value::Str("Percentage".into());
            if let Ok(Some(percentage_value)) = changed_properties.get::<_, Value>(&percentage_key) {
                process_battery_percentage(percentage_value);
            }
        }
        "org.bluez.Battery1" => {
            let Value::Dict(_) = changed_properties_val else {
                error!("Dbus monitor: Expected Dict for changed_properties, got: {:?}", changed_properties_val);
                return;
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
            let display_string = compute_bluetooth_display_string(bluetooth_devices);
            if let Err(e) = bus::send_bluetooth_update(display_string) {
                error!("Failed to send Bluetooth battery update: {}", e);
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
        }
    }
}

// ObjectManager.InterfacesRemoved: counterpart to InterfacesAdded. Each removed
// interface flips a flag back to false; remove_if_idle drops the device once
// every flag is false and the name is gone. UPower device removal currently
// has no UI consequence (laptop battery comes and goes only via hardware).
fn handle_interfaces_removed(msg: &zbus::Message, bluetooth_devices: &mut HashMap<String, BluetoothDevice>) {
    info!("Dbus monitor: Received InterfacesRemoved signal from ObjectManager");
    let body = msg.body();
    let Ok(body_deserialized) = body.deserialize::<zvariant::Structure>() else {
        error!("Dbus monitor: Failed to deserialize InterfacesRemoved message body as Structure");
        return;
    };
    let fields = body_deserialized.fields();
    let (object_path_value, interfaces_array_value) = match fields {
        [a, b] => (a, b),
        other => {
            error!("Dbus monitor: Expected exactly 2 fields in InterfacesRemoved, got: {}", other.len());
            return;
        }
    };

    let object_path = match object_path_value {
        Value::ObjectPath(object_path) => object_path,
        other => {
            error!("Dbus monitor: Expected ObjectPath as first element, got {:?}", other);
            return;
        }
    };

    let interfaces = match interfaces_array_value {
        Value::Array(arr) => arr,
        other => {
            error!("Dbus monitor: Expected Array as second element, got {:?}", other);
            return;
        }
    };

    debug!("Dbus monitor: Interfaces removed from {}: {:?}", object_path, interfaces);

    let object_path_str = object_path.as_str();
    // Check for bt battery or media interfaces and handle them
    for iface in interfaces.iter() {
        let Value::Str(interface_name) = iface else { continue };
        match interface_name.as_str() {
            "org.bluez.Battery1" => {
                info!("Dbus monitor: Bluetooth battery interface removed from {}", object_path);
                if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
                    device.has_battery = false;
                    device.battery_percentage = None;
                    info!("Updated device {} to remove battery capability", object_path);
                    remove_if_idle(bluetooth_devices, object_path_str);
                } else {
                    debug!("Battery interface removed from device not in HashMap: {}", object_path);
                }
            }
            "org.bluez.MediaControl1" => {
                info!("Dbus monitor: Bluetooth media interface removed from {}", object_path);
                if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
                    device.has_media = false;
                    info!("Updated device {} to remove media capability", object_path);
                    remove_if_idle(bluetooth_devices, object_path_str);
                } else {
                    debug!("Media interface removed from device not in HashMap: {}", object_path);
                }
            }
            "org.bluez.Device1" => {
                info!("Dbus monitor: Bluetooth Device1 interface removed from {}", object_path);
                if let Some(device) = bluetooth_devices.get_mut(object_path_str) {
                    device.device_name = None;
                    info!("Cleared device name for {}", object_path);
                    remove_if_idle(bluetooth_devices, object_path_str);
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

    // Send GUI update after any Bluetooth device removal
    let display_string = compute_bluetooth_display_string(bluetooth_devices);
    if let Err(e) = bus::send_bluetooth_update(display_string) {
        error!("Failed to send Bluetooth battery update after device removal: {}", e);
    }
}

// Initial UPower battery query: read Percentage + State for the BAT0 device
// and push one update through the bus. Silently no-ops on desktop systems
// where the proxy/property is absent (logs at info!, not error!). Subsequent
// updates arrive via the PropertiesChanged match rule + handle_properties_changed.
async fn initial_battery_query(connection: &Connection) {
    // TODO: what if there is no battery (for example, in a desktop?)
    // Probably should monitor if a battery comes into existance so
    // you should not return

    // will .ok() later
    let properties_proxy = zbus::fdo::PropertiesProxy::new(
        connection,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower/devices/battery_BAT0",
    ).await
    .inspect_err(|_e| error!("different style to construction battery_BAT0 proxy failed"))
    .ok();

    let Some(proxy) = properties_proxy else { return };
    let Some(battery_interface_name) = InterfaceName::try_from("org.freedesktop.UPower.Device")
        .inspect_err(|e| error!("Failed to create interface name: {}", e))
        .ok()
    else { return };

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

    bus::send_battery_update(battery_text)
        .inspect_err(|e| error!("Failed to send battery update: {}", e))
        .ok();

    if let Some(state_value) = proxy.get(battery_interface_name, "State").await
        .inspect_err(|e|
            info!("No battery state detected initially (likely desktop system): {}", e)
        )
        .ok()
    {
        process_battery_state(state_value.into());
    }
}

// Initial BlueZ scan via ObjectManager.GetManagedObjects: enumerate every
// known device path, pick up Device1 (name), Battery1 (percentage), and
// MediaControl1 (presence), and seed bluetooth_devices. Sends one display
// update through the bus once the scan completes so the widget has data on
// first paint (or empty string if no devices).
async fn initial_bluetooth_scan(
    connection: &Connection,
    bluetooth_devices: &mut HashMap<String, BluetoothDevice>,
) {
    let bluez_proxy = zbus::fdo::PropertiesProxy::new(
        connection,
        "org.bluez",
        "/", // ObjectManager path
    ).await
    .inspect_err(|e| error!("Failed to create Bluez ObjectManager proxy: {}", e))
    .ok();

    let Some(_bluez_proxy) = bluez_proxy else { return };

    // Use ObjectManager to get all managed objects
    let object_manager = zbus::fdo::ObjectManagerProxy::new(connection, "org.bluez", "/").await
        .inspect_err(|e| error!("Failed to create Bluez ObjectManager: {}", e))
        .ok();
    let Some(object_manager) = object_manager else { return };

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
            let display_string = compute_bluetooth_display_string(bluetooth_devices);
            info!("Sent initial Bluetooth display: {}", display_string);
            if let Err(e) = bus::send_bluetooth_update(display_string) {
                error!("Failed to send initial Bluetooth display update: {}", e);
            }
        }
        Err(e) => {
            info!("No Bluetooth devices found or failed to query: {}", e);

            // Send "No BT" update even when no devices found
            let display_string = compute_bluetooth_display_string(bluetooth_devices);
            if let Err(e) = bus::send_bluetooth_update(display_string) {
                error!("Failed to send 'No BT' display update: {}", e);
            }
        }
    }
}

// Register the three D-Bus match rules we care about. If any one fails to
// build or to register we log + carry on — the other rules will still
// produce useful signals, and run_dbus_monitor_supervised will retry the
// whole monitor if the stream later dies.
async fn register_match_rules(dbus_proxy: &fdo::DBusProxy<'_>) {
    for (label, rule_result) in [
        ("battery", build_battery_match_rule()),
        ("bluez InterfacesAdded", build_bluez_object_manager_match_rule("InterfacesAdded")),
        ("bluez InterfacesRemoved", build_bluez_object_manager_match_rule("InterfacesRemoved")),
    ] {
        match rule_result {
            Err(e) => {
                error!("❌ Failed to build {} match rule: {:#}", label, e);
            }
            Ok(rule) => {
                if let Err(e) = dbus_proxy.add_match_rule(rule).await {
                    error!("❌ Failed to register {} match rule: {}", label, e);
                } else {
                    debug!("🔌 Registered {} match rule", label);
                }
            }
        }
    }
}

// Supervised wrapper around monitor_dbus. The inner loop holds one D-Bus
// connection and dispatches signals forever; it only returns when the
// MessageStream ends (system bus crash, connection drop) or when the initial
// connect/proxy setup fails. Same backoff policy as the Hyprland supervisors —
// the failure modes are equivalent (IPC peer gone, transient setup error).
pub async fn run_dbus_monitor_supervised() {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("🔌 Starting D-Bus monitor");
        match monitor_dbus().await {
            Ok(()) => {
                warn!("⚠️ D-Bus monitor returned cleanly (stream closed)");
            }
            Err(e) => {
                error!("❌ D-Bus monitor crashed: {:#}", e);
            }
        }

        if started.elapsed() >= reset_threshold {
            debug!("🔄 D-Bus monitor ran for {:?}, resetting backoff", started.elapsed());
            delay = Duration::from_secs(1);
        }

        warn!("🔄 Reconnecting D-Bus monitor in {:?}", delay);
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

pub async fn monitor_dbus() -> Result<()> {
    info!("Starting D-Bus monitoring task");
    let connection = Connection::system().await
        .map_err(|e| {
            error!("Failed to connect to system D-Bus: {}", e);
            e
        })?;

    initial_battery_query(&connection).await;

    // TODO: Consider adding has_device1 field to BluetoothDevice struct for full symmetry
    // with has_battery and has_media fields. Current approach uses device_name presence
    // as proxy for Device1 interface availability.
    let mut bluetooth_devices: HashMap<String, BluetoothDevice> = HashMap::new();
    initial_bluetooth_scan(&connection, &mut bluetooth_devices).await;

    // Subscribe to UPower property changes before creating MessageStream.
    // Without this subscription, the MessageStream receives no messages because
    // D-Bus requires explicit signal subscriptions via match rules.
    // Note: ObjectManagerProxy only reports interface additions/removals, not property changes.
    // As per https://openrr.github.io/openrr/zbus/fdo/struct.ObjectManagerProxy.html:
    // "Changes to properties on existing interfaces are not reported using this interface"
    // Therefore we must subscribe to org.freedesktop.DBus.Properties.PropertiesChanged.
    let dbus_proxy = fdo::DBusProxy::new(&connection).await?;
    register_match_rules(&dbus_proxy).await;

    // from the connection, we get the dbus_proxy, we add the rules to the proxy
    // which makes it so that when we do connection.into() to get a stream
    // we can think of the rules being *inside* that connection.
    //
    // Some code online seems to use select!, which merges multiple async sources
    // into one. We should think if select! + multiple streams is better. The
    // current approach is: one stream, multiple match rules, branch on event
    // shape in the loop below.
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

        match (interface, member) {
            ("org.freedesktop.DBus.ObjectManager", "InterfacesAdded") => {
                handle_interfaces_added(&msg, &mut bluetooth_devices);
            }
            ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
                handle_properties_changed(&msg, path, &mut bluetooth_devices);
            }
            ("org.freedesktop.DBus.ObjectManager", "InterfacesRemoved") => {
                handle_interfaces_removed(&msg, &mut bluetooth_devices);
            }
            _ => {
                warn!("Dbus monitor: Unhandled signal: path: {}, interface: {}, member: {}", path, interface, member);
            }
        }
    }

    error!("Dbus monitor: Message stream ended unexpectedly");

    Ok(())
}
