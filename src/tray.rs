use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use zbus::fdo::DBusProxy;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, Proxy};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";

pub type IconPixmap = (i32, i32, Vec<u8>);

#[derive(Debug, Clone)]
pub struct TrayItem {
    pub key: String,
    pub title: String,
    pub status: String,
    pub item_is_menu: bool,
    pub icon_name: String,
    pub icon_pixmap: Option<IconPixmap>,
    pub icon_theme_path: String,
    /// Object path of the item's `com.canonical.dbusmenu` interface, or empty
    /// when the item exposes no menu at all. Read from the SNI `Menu` property.
    pub menu_path: String,
}

// A single entry of a tray item's `com.canonical.dbusmenu` layout. The tree of
// these is what we render into a native GTK popover — instead of asking the app
// to draw its own menu (which layer-shell hosts cannot reliably do), we mirror
// eww's approach and paint the dbusmenu ourselves.
#[derive(Debug, Clone)]
pub struct TrayMenuItem {
    pub id: i32,
    pub label: Option<String>,
    pub icon_name: Option<String>,
    pub enabled: bool,
    pub visible: bool,
    pub is_separator: bool,
    pub toggle_type: Option<String>,
    pub toggle_state: Option<bool>,
    pub children: Vec<TrayMenuItem>,
}

#[derive(Debug, Clone)]
pub struct TrayMenu {
    pub key: String,
    /// dbusmenu object path the layout was fetched from. Menu entry activations
    /// carry it back in TrayCommand so the backend can deliver `Event` without
    /// re-resolving the item's `Menu` property.
    pub menu_path: String,
    pub items: Vec<TrayMenuItem>,
    pub request_id: u64,
    pub keyboard_grab: bool,
}

#[derive(Debug)]
pub enum TrayUpdate {
    Upsert(TrayItem),
    Remove(String),
}

#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    Activate,
    SecondaryActivate,
    ContextMenu {
        request_id: u64,
        keyboard_grab: bool,
    },
    /// Activate a specific entry of the item's `com.canonical.dbusmenu` layout.
    /// The payload is the dbusmenu entry id, forwarded to the `Event` method;
    /// the click coordinates of TrayCommand are unused for this action.
    MenuEvent(i32),
}

#[derive(Debug)]
pub struct TrayCommand {
    pub key: String,
    pub action: TrayAction,
    pub x: i32,
    pub y: i32,
    /// Object path of the item's `com.canonical.dbusmenu`, captured from the
    /// item's last known state when the click happened so the backend does not
    /// have to re-fetch it to render the menu or deliver a menu event.
    pub menu_path: String,
}

// Tray traffic is bidirectional, so it does not fit the one-way label channels
// in bus::Bus. Keep the same refactor invariant with typed endpoints instead:
// activate() creates both halves, gives TrayUi to the GTK consumer, and only
// then spawns the backend with TrayBackend.
pub struct TrayBackend {
    updates: mpsc::UnboundedSender<TrayUpdate>,
    commands: mpsc::UnboundedReceiver<TrayCommand>,
    menus: mpsc::UnboundedSender<TrayMenu>,
}

pub struct TrayUi {
    pub updates: mpsc::UnboundedReceiver<TrayUpdate>,
    pub commands: mpsc::UnboundedSender<TrayCommand>,
    pub menus: mpsc::UnboundedReceiver<TrayMenu>,
}

pub fn channels() -> (TrayBackend, TrayUi) {
    let (update_tx, update_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (menu_tx, menu_rx) = mpsc::unbounded_channel();

    (
        TrayBackend {
            updates: update_tx,
            commands: command_rx,
            menus: menu_tx,
        },
        TrayUi {
            updates: update_rx,
            commands: command_tx,
            menus: menu_rx,
        },
    )
}

#[derive(Default)]
struct StatusNotifierWatcher {
    items: Vec<String>,
    hosts: Vec<String>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl StatusNotifierWatcher {
    async fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let item = match normalize_registration(service, &header, connection).await {
            Ok(item) => item,
            Err(error) => {
                warn!(service, %error, "Could not normalize tray item registration");
                return Err(error);
            }
        };

        if !self.items.contains(&item) {
            self.items.push(item.clone());
            // The registration itself succeeded once the state is updated; a
            // failed broadcast must not turn the app's Register call into an
            // error, so signal emission is best-effort from here on.
            if let Err(error) = Self::status_notifier_item_registered(&emitter, &item).await {
                warn!(item, %error, "Could not broadcast tray item registration");
            }
            if let Err(error) = self
                .registered_status_notifier_items_changed(&emitter)
                .await
            {
                warn!(item, %error, "Could not broadcast tray item list change");
            }
            info!(item, "Status notifier item registered");
        }
        Ok(())
    }

    async fn register_status_notifier_host(
        &mut self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let host = match normalize_registration(service, &header, connection).await {
            Ok(host) => host,
            Err(error) => {
                warn!(service, %error, "Could not normalize tray host registration");
                return Err(error);
            }
        };
        let first = self.hosts.is_empty();
        if !self.hosts.contains(&host) {
            self.hosts.push(host);
            if first {
                if let Err(error) = Self::status_notifier_host_registered(&emitter).await {
                    warn!(%error, "Could not broadcast tray host registration");
                }
                if let Err(error) = self
                    .is_status_notifier_host_registered_changed(&emitter)
                    .await
                {
                    warn!(%error, "Could not broadcast tray host flag change");
                }
            }
        }
        Ok(())
    }

    #[zbus(property, name = "RegisteredStatusNotifierItems")]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.clone()
    }

    #[zbus(property, name = "IsStatusNotifierHostRegistered")]
    fn is_status_notifier_host_registered(&self) -> bool {
        !self.hosts.is_empty()
    }

    #[zbus(property, name = "ProtocolVersion")]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal, name = "StatusNotifierItemRegistered")]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "StatusNotifierItemUnregistered")]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "StatusNotifierHostRegistered")]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal, name = "StatusNotifierHostUnregistered")]
    async fn status_notifier_host_unregistered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

impl StatusNotifierWatcher {
    // Drop every registration owned by a vanished bus connection. Signal
    // emission is best-effort: the state change already happened, and a failed
    // broadcast must not stop the remaining cleanup.
    async fn remove_owner(&mut self, owner: &str, emitter: &SignalEmitter<'_>) {
        let removed_items: Vec<String> = self
            .items
            .iter()
            .filter(|service| service_owner(service) == Some(owner))
            .cloned()
            .collect();
        if !removed_items.is_empty() {
            self.items
                .retain(|service| service_owner(service) != Some(owner));
            for service in removed_items {
                if let Err(error) = Self::status_notifier_item_unregistered(emitter, &service).await
                {
                    warn!(item = service, %error, "Could not broadcast tray item removal");
                }
                info!(item = service, "Status notifier item disappeared");
            }
            if let Err(error) = self.registered_status_notifier_items_changed(emitter).await {
                warn!(%error, "Could not broadcast tray item list change");
            }
        }

        let had_hosts = !self.hosts.is_empty();
        self.hosts
            .retain(|service| service_owner(service) != Some(owner));
        if had_hosts && self.hosts.is_empty() {
            if let Err(error) = Self::status_notifier_host_unregistered(emitter).await {
                warn!(%error, "Could not broadcast tray host removal");
            }
            if let Err(error) = self
                .is_status_notifier_host_registered_changed(emitter)
                .await
            {
                warn!(%error, "Could not broadcast tray host flag change");
            }
        }
    }
}

// Canonicalize a registration into `{unique_bus_name}{object_path}`, matching the
// scheme used by other trays (e.g. eww). Tray applications identify themselves in
// three different ways: a bare object path (`/StatusNotifierItem`), their unique
// bus name (`:1.42`), or a well-known name (`org.kde.StatusNotifierItem-2053-1`).
// The latter is a problem for deduplication: an application that re-registers when
// a new tray host appears often does so under a fresh well-known name suffix, so
// keying by the raw service string produces a new entry every time and the icon
// multiplies once per running tray. Resolving everything back to the unique bus
// name of the sender collapses those re-registrations onto a single stable key.
async fn normalize_registration(
    service: &str,
    header: &Header<'_>,
    connection: &Connection,
) -> zbus::fdo::Result<String> {
    let Some(sender) = header.sender() else {
        warn!(service, "Tray registration arrived without a D-Bus sender");
        return Err(zbus::fdo::Error::InvalidArgs("missing D-Bus sender".into()));
    };

    // A bare object path: the item lives on the sender's own connection, which is
    // already the unique name we want to key on.
    if service.starts_with('/') {
        return Ok(format!("{sender}{service}"));
    }

    // Otherwise the service is a bus name. If it is already unique we keep it; a
    // well-known name has to be resolved to its owner so re-registrations collapse
    // onto one key.
    let bus_name = match BusName::try_from(service) {
        Ok(bus_name) => bus_name,
        Err(error) => {
            warn!(service, %error, "Tray registration carried an invalid bus name");
            return Err(zbus::fdo::Error::InvalidArgs(error.to_string()));
        }
    };
    let well_known = match bus_name {
        BusName::Unique(unique) => return Ok(format!("{unique}{DEFAULT_ITEM_PATH}")),
        BusName::WellKnown(well_known) => well_known,
    };

    let dbus = match DBusProxy::new(connection).await {
        Ok(dbus) => dbus,
        Err(error) => {
            warn!(%error, "Could not open D-Bus proxy to resolve tray owner");
            return Err(zbus::fdo::Error::Failed(format!(
                "could not open D-Bus proxy: {error}"
            )));
        }
    };
    match dbus.get_name_owner(BusName::WellKnown(well_known)).await {
        Ok(owner) => Ok(format!("{owner}{DEFAULT_ITEM_PATH}")),
        Err(error) => {
            warn!(service, %error, "Could not resolve unique owner of tray bus name");
            Err(error)
        }
    }
}

pub fn split_service(service: &str) -> Result<(&str, &str)> {
    let slash = service
        .find('/')
        .with_context(|| format!("status notifier service has no object path: {service}"))?;
    Ok((&service[..slash], &service[slash..]))
}

fn service_owner(service: &str) -> Option<&str> {
    match service.find('/') {
        Some(slash) => Some(&service[..slash]),
        None => None,
    }
}

async fn monitor_watcher_owners(connection: Connection) -> Result<()> {
    let dbus = DBusProxy::new(&connection)
        .await
        .context("open D-Bus proxy for watcher owner monitoring")?;
    let mut owner_changes = dbus
        .receive_name_owner_changed()
        .await
        .context("subscribe to D-Bus name owner changes")?;
    while let Some(change) = owner_changes.next().await {
        let args = match change.args() {
            Ok(args) => args,
            Err(error) => {
                warn!(%error, "Could not decode D-Bus name owner change");
                continue;
            }
        };
        if args.new_owner.is_some() {
            continue;
        }

        let interface = match connection
            .object_server()
            .interface::<_, StatusNotifierWatcher>(WATCHER_PATH)
            .await
        {
            Ok(interface) => interface,
            Err(error) => {
                // The watcher was exported before this task started, so losing
                // the interface is structural, not transient — stop and let the
                // spawn wrapper log it instead of looping on the same failure.
                return Err(error).context("access local StatusNotifierWatcher state");
            }
        };
        let emitter = interface.signal_emitter().clone();
        let mut watcher = interface.get_mut().await;
        watcher.remove_owner(args.name.as_str(), &emitter).await;
    }
    Ok(())
}

// Make sure a StatusNotifierWatcher exists on the bus, serving one ourselves if
// nobody else does. Returns the owner-monitor task when we won the name, so the
// caller can abort it during teardown.
async fn ensure_watcher(connection: &Connection) -> Result<Option<JoinHandle<()>>> {
    let dbus = DBusProxy::new(connection)
        .await
        .context("open D-Bus proxy for watcher setup")?;
    let watcher_name =
        BusName::try_from(WATCHER_NAME).context("parse StatusNotifierWatcher bus name")?;
    if dbus
        .name_has_owner(watcher_name)
        .await
        .context("probe for an existing StatusNotifierWatcher")?
    {
        info!("Using the existing StatusNotifierWatcher");
        return Ok(None);
    }

    connection
        .object_server()
        .at(WATCHER_PATH, StatusNotifierWatcher::default())
        .await
        .context("export StatusNotifierWatcher")?;
    match connection.request_name(WATCHER_NAME).await {
        Ok(()) => {
            info!("Serving StatusNotifierWatcher for tray applications");
            let connection = connection.clone();
            let monitor = tokio::spawn(async move {
                if let Err(error) = monitor_watcher_owners(connection).await {
                    warn!(%error, "StatusNotifierWatcher owner monitor stopped");
                }
            });
            Ok(Some(monitor))
        }
        Err(zbus::Error::NameTaken) => {
            info!("Another StatusNotifierWatcher won startup race");
            Ok(None)
        }
        Err(error) => Err(error).context("request StatusNotifierWatcher bus name"),
    }
}

async fn watcher_proxy(connection: &Connection) -> Result<Proxy<'_>> {
    Proxy::new(connection, WATCHER_NAME, WATCHER_PATH, WATCHER_INTERFACE)
        .await
        .context("create StatusNotifierWatcher proxy")
}

async fn item_proxy<'a>(connection: &'a Connection, service: &'a str) -> Result<Proxy<'a>> {
    let (destination, path) = split_service(service)?;
    Proxy::new(connection, destination, path, ITEM_INTERFACE)
        .await
        .with_context(|| format!("create status notifier item proxy for {service}"))
}

async fn optional_property<T>(proxy: &Proxy<'_>, name: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
    T::Error: Into<zbus::Error>,
{
    match proxy.get_property(name).await {
        Ok(value) => Some(value),
        Err(error) => {
            debug!(property = name, %error, "Tray item property is unavailable");
            None
        }
    }
}

fn largest_valid_pixmap(pixmaps: Vec<IconPixmap>) -> Option<IconPixmap> {
    pixmaps
        .into_iter()
        .filter(|(width, height, data)| {
            *width > 0 && *height > 0 && data.len() == (*width as usize) * (*height as usize) * 4
        })
        .max_by_key(|(width, height, _)| width * height)
}

// Read a full snapshot of one status notifier item. Every property is optional
// on the wire — items routinely omit half of them — so each falls back to a
// neutral default instead of failing the whole item.
async fn fetch_item(connection: &Connection, key: &str) -> Result<TrayItem> {
    let proxy = item_proxy(connection, key).await?;
    let title: String = optional_property(&proxy, "Title").await.unwrap_or_default();
    let status: String = optional_property(&proxy, "Status")
        .await
        .unwrap_or_else(|| "Active".to_string());
    let item_is_menu: bool = optional_property(&proxy, "ItemIsMenu")
        .await
        .unwrap_or_default();

    let needs_attention = status.eq_ignore_ascii_case("NeedsAttention");
    let icon_name_property = if needs_attention {
        "AttentionIconName"
    } else {
        "IconName"
    };
    let pixmap_property = if needs_attention {
        "AttentionIconPixmap"
    } else {
        "IconPixmap"
    };
    let icon_name: String = optional_property(&proxy, icon_name_property)
        .await
        .unwrap_or_default();
    let pixmaps: Vec<IconPixmap> = optional_property(&proxy, pixmap_property)
        .await
        .unwrap_or_default();
    let icon_theme_path: String = optional_property(&proxy, "IconThemePath")
        .await
        .unwrap_or_default();
    let menu_path = optional_property::<zvariant::OwnedObjectPath>(&proxy, "Menu")
        .await
        .map(|menu_path| menu_path.as_str().to_string())
        .unwrap_or_default();

    Ok(TrayItem {
        key: key.to_string(),
        title,
        status,
        item_is_menu,
        icon_name,
        icon_pixmap: largest_valid_pixmap(pixmaps),
        icon_theme_path,
        menu_path,
    })
}

async fn monitor_item(
    connection: Connection,
    key: String,
    updates: mpsc::UnboundedSender<TrayUpdate>,
) {
    let send_latest = || async {
        match fetch_item(&connection, &key).await {
            Ok(item) => {
                if let Err(error) = updates.send(TrayUpdate::Upsert(item)) {
                    debug!(item = key, %error, "Tray UI receiver was dropped");
                }
            }
            Err(error) => warn!(item = key, %error, "Could not read tray item"),
        }
    };
    send_latest().await;

    let proxy = match item_proxy(&connection, &key).await {
        Ok(proxy) => proxy,
        Err(error) => {
            warn!(item = key, %error, "Could not create tray item signal proxy");
            return;
        }
    };
    let mut signals = match proxy.receive_all_signals().await {
        Ok(signals) => signals,
        Err(error) => {
            warn!(item = key, %error, "Could not subscribe to tray item signals");
            return;
        }
    };
    while let Some(message) = signals.next().await {
        let header = message.header();
        let Some(member) = header.member() else {
            debug!(item = key, "Ignoring tray signal without a member name");
            continue;
        };
        if member.as_str().starts_with("New") {
            send_latest().await;
        }
    }
}

fn start_item_monitor(
    connection: &Connection,
    service: String,
    updates: &mpsc::UnboundedSender<TrayUpdate>,
    monitors: &mut HashMap<String, JoinHandle<()>>,
) {
    if monitors.contains_key(&service) {
        return;
    }
    let handle = tokio::spawn(monitor_item(
        connection.clone(),
        service.clone(),
        updates.clone(),
    ));
    monitors.insert(service, handle);
}

const DBUSMENU_INTERFACE: &str = "com.canonical.dbusmenu";

// Pull the `dbusmenu` layout out of a tray item and turn it into our own
// TrayMenu tree, which the widget layer paints as a native GTK popover. This is
// the GTK4 equivalent of eww's `dbusmenu-gtk3::Menu`: the host renders the
// menu instead of asking the app to draw it (which a layer-shell bar cannot do
// reliably — the app would have no valid parent surface for its window).
async fn fetch_menu(connection: &Connection, key: &str, menu_path: &str) -> Result<TrayMenu> {
    let (destination, _) = split_service(key)?;
    let menu_proxy = Proxy::new(connection, destination, menu_path, DBUSMENU_INTERFACE)
        .await
        .with_context(|| format!("create dbusmenu proxy for {key}"))?;

    // Must be a Vec (serialized as the `as` array the signature expects), not a
    // fixed-size `[&str; N]` — zbus serializes those as a struct `(ss…)`, which
    // the bus rejects with "Invalid arguments 'ii(ss…)' expecting 'iias'".
    let requested: Vec<&str> = vec![
        "type",
        "label",
        "icon-name",
        "icon-data",
        "enabled",
        "visible",
        "toggle-type",
        "toggle-state",
        "children-display",
        "disposition",
    ];
    let message = menu_proxy
        .call_method("GetLayout", &(0i32, -1i32, requested))
        .await
        .with_context(|| format!("call dbusmenu GetLayout on {menu_path} of {key}"))?;

    // The reply is `(u revision, (i id, a{sv} properties, av children))`. We read
    // it as a raw Structure and walk it by hand, one layer at a time, because the
    // children are recursively nested structs, which zbus's derive machinery
    // cannot name.
    let body = message.body();
    let structure: zvariant::Structure<'_> = match body.deserialize() {
        Ok(structure) => structure,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("decode dbusmenu GetLayout reply for {key}"));
        }
    };
    // Field 0 is the layout revision, which we do not track: every menu open
    // fetches a fresh layout anyway.
    let outer = structure.fields();
    debug!(
        item = key,
        fields = outer.len(),
        "dbusmenu GetLayout reply decoded"
    );
    let root_node = match outer.get(1) {
        Some(zvariant::Value::Structure(root_node)) => root_node,
        Some(other) => {
            return Err(anyhow!(
                "dbusmenu layout for {key} has a non-struct root node: {:?}",
                other.value_signature()
            ));
        }
        None => {
            return Err(anyhow!(
                "dbusmenu layout reply for {key} is missing the root node"
            ));
        }
    };
    let Some(root) = parse_menu_node(root_node) else {
        return Err(anyhow!(
            "dbusmenu root node for {key} does not parse as a layout node"
        ));
    };

    debug!(
        item = key,
        count = root.children.len(),
        "dbusmenu layout parsed"
    );

    Ok(TrayMenu {
        key: key.to_string(),
        menu_path: menu_path.to_string(),
        items: root.children,
        request_id: 0,
        keyboard_grab: false,
    })
}

// Forward a menu entry activation back to the app through dbusmenu's `Event`
// method. The visual menu is ours, but the app still owns the action.
// `menu_path` normally arrives with the command (captured when the menu was
// rendered); an empty one is re-resolved from the item as a fallback.
async fn dbusmenu_event(
    connection: &Connection,
    key: &str,
    menu_path: &str,
    id: i32,
) -> Result<()> {
    let (destination, _) = split_service(key)?;
    let menu_path = if menu_path.is_empty() {
        let item = item_proxy(connection, key).await?;
        match optional_property::<zvariant::OwnedObjectPath>(&item, "Menu").await {
            Some(path) => path.as_str().to_string(),
            None => bail!("tray item {key} has no dbusmenu to deliver event {id} to"),
        }
    } else {
        menu_path.to_string()
    };
    let menu_proxy = Proxy::new(
        connection,
        destination,
        menu_path.as_str(),
        DBUSMENU_INTERFACE,
    )
    .await
    .with_context(|| format!("create dbusmenu proxy for event on {key}"))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as u32)
        .unwrap_or(0);
    menu_proxy
        .call_method(
            "Event",
            &(id, "clicked", zvariant::Value::U32(0), timestamp),
        )
        .await
        .with_context(|| format!("deliver dbusmenu event for entry {id} of {key}"))?;
    Ok(())
}

fn menu_prop<'a>(props: &'a zvariant::Dict<'a, 'a>, name: &str) -> Option<&'a zvariant::Value<'a>> {
    for (candidate, value) in props.iter() {
        if let zvariant::Value::Str(text) = candidate {
            if text.as_str() == name {
                // dbusmenu properties are `a{sv}`; each value is a D-Bus variant,
                // so zvariant hands it back as `Value::Value(inner)`. Unwrap that
                // one level so the actual string/bool/int is what we match on.
                return match value {
                    zvariant::Value::Value(inner) => Some(inner),
                    other => Some(other),
                };
            }
        }
    }
    None
}

fn menu_prop_str(props: &zvariant::Dict<'_, '_>, key: &str) -> Option<String> {
    match menu_prop(props, key)? {
        zvariant::Value::Str(text) => Some(text.as_str().to_string()),
        _ => None,
    }
}

fn menu_prop_bool(props: &zvariant::Dict<'_, '_>, key: &str, default: bool) -> bool {
    match menu_prop(props, key) {
        Some(zvariant::Value::Bool(value)) => *value,
        _ => default,
    }
}

fn menu_prop_toggle_state(props: &zvariant::Dict<'_, '_>) -> Option<bool> {
    match menu_prop(props, "toggle-state")? {
        zvariant::Value::I32(value) => Some(*value != 0),
        zvariant::Value::U32(value) => Some(*value != 0),
        _ => None,
    }
}

// dbusmenu labels use `_` as the keyboard mnemonic marker and `__` as an
// escaped literal underscore. We render no mnemonics, so drop the markers and
// collapse the escapes.
fn strip_mnemonic(label: String) -> String {
    let mut stripped = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(current) = chars.next() {
        if current != '_' {
            stripped.push(current);
            continue;
        }
        match chars.next() {
            // "__" is an escaped literal underscore.
            Some('_') => stripped.push('_'),
            // A single "_" marks the next character as the access key; the
            // character itself is still displayed.
            Some(marked) => stripped.push(marked),
            None => {}
        }
    }
    stripped
}

fn as_structure<'a>(value: &'a zvariant::Value<'a>) -> Option<&'a zvariant::Structure<'a>> {
    match value {
        zvariant::Value::Structure(structure) => Some(structure),
        zvariant::Value::Value(inner) => as_structure(inner),
        _ => None,
    }
}

fn parse_menu_node(node: &zvariant::Structure<'_>) -> Option<TrayMenuItem> {
    let fields = node.fields();
    let id = match fields.first() {
        Some(zvariant::Value::I32(value)) => *value,
        _ => {
            debug!("dbusmenu node is missing its id field");
            return None;
        }
    };
    let props = match fields.get(1) {
        Some(zvariant::Value::Dict(dict)) => dict,
        _ => {
            debug!("dbusmenu node is missing its properties field");
            return None;
        }
    };
    let children_raw = match fields.get(2) {
        Some(zvariant::Value::Array(array)) => array,
        _ => {
            debug!("dbusmenu node is missing its children field");
            return None;
        }
    };

    let is_separator = menu_prop_str(props, "type").as_deref() == Some("separator");
    let label = menu_prop_str(props, "label").map(strip_mnemonic);
    let icon_name = menu_prop_str(props, "icon-name");
    let enabled = menu_prop_bool(props, "enabled", true);
    let visible = menu_prop_bool(props, "visible", true);
    let toggle_type = menu_prop_str(props, "toggle-type");
    let toggle_state = menu_prop_toggle_state(props);

    let mut children = Vec::new();
    for child in children_raw.iter() {
        if let Some(structure) = as_structure(child) {
            if let Some(item) = parse_menu_node(structure) {
                children.push(item);
            }
        } else {
            debug!("dbusmenu child is not a layout struct");
        }
    }

    Some(TrayMenuItem {
        id,
        label,
        icon_name,
        enabled,
        visible,
        is_separator,
        toggle_type,
        toggle_state,
        children,
    })
}

// Fire-and-forget one of the SNI click methods (Activate, SecondaryActivate,
// ContextMenu) — they all take the click coordinates. Failures are logged, not
// propagated: a misbehaving tray app must not take the backend down.
async fn call_item_noreply(connection: &Connection, key: &str, method: &str, x: i32, y: i32) {
    let proxy = match item_proxy(connection, key).await {
        Ok(proxy) => proxy,
        Err(error) => {
            warn!(item = key, method, %error, "Could not reach tray item for click");
            return;
        }
    };
    if let Err(error) = proxy.call_noreply(method, &(x, y)).await {
        warn!(item = key, method, %error, "Tray item click method failed");
    }
}

async fn dispatch_command(
    connection: &Connection,
    command: TrayCommand,
    menu_tx: &mpsc::UnboundedSender<TrayMenu>,
) {
    debug!(
        item = command.key,
        action = ?command.action,
        x = command.x,
        y = command.y,
        "Tray backend received command"
    );
    match command.action {
        TrayAction::Activate => {
            call_item_noreply(connection, &command.key, "Activate", command.x, command.y).await;
        }
        TrayAction::SecondaryActivate => {
            call_item_noreply(
                connection,
                &command.key,
                "SecondaryActivate",
                command.x,
                command.y,
            )
            .await;
        }
        TrayAction::ContextMenu {
            request_id,
            keyboard_grab,
        } => {
            // Mirror eww: paint the dbusmenu ourselves. Fall back to the SNI's
            // own ContextMenu method for the rare item that draws its own menu.
            let menu_path = if command.menu_path.is_empty() {
                match item_proxy(connection, &command.key).await {
                    Ok(proxy) => optional_property::<zvariant::OwnedObjectPath>(&proxy, "Menu")
                        .await
                        .map(|path| path.as_str().to_string())
                        .unwrap_or_default(),
                    Err(error) => {
                        warn!(item = command.key, %error, "Could not reach tray item to resolve its menu");
                        String::new()
                    }
                }
            } else {
                command.menu_path.clone()
            };
            if menu_path.is_empty() {
                call_item_noreply(
                    connection,
                    &command.key,
                    "ContextMenu",
                    command.x,
                    command.y,
                )
                .await;
                return;
            }
            match fetch_menu(connection, &command.key, &menu_path).await {
                Ok(mut menu) => {
                    menu.request_id = request_id;
                    menu.keyboard_grab = keyboard_grab;
                    if let Err(error) = menu_tx.send(menu) {
                        warn!(item = command.key, %error, "Could not deliver tray menu to UI");
                    }
                }
                Err(error) => {
                    // The advertised menu path was unusable; give the app one
                    // chance to draw its own menu instead.
                    warn!(item = command.key, error = ?error, "Could not read tray menu; asking the app to draw it");
                    call_item_noreply(
                        connection,
                        &command.key,
                        "ContextMenu",
                        command.x,
                        command.y,
                    )
                    .await;
                }
            }
        }
        TrayAction::MenuEvent(id) => {
            if let Err(error) =
                dbusmenu_event(connection, &command.key, &command.menu_path, id).await
            {
                warn!(item = command.key, entry = id, %error, "Tray menu event failed");
            }
        }
    }
}

// One tray backend session: connect, make sure a watcher exists, register as a
// host, then relay item updates and click commands until a stream or channel
// closes under us. The channel ends are borrowed so the supervisor can retry
// with the same GTK-side wiring.
async fn run_tray(
    updates: &mpsc::UnboundedSender<TrayUpdate>,
    commands: &mut mpsc::UnboundedReceiver<TrayCommand>,
    menu_tx: &mpsc::UnboundedSender<TrayMenu>,
) -> Result<()> {
    let connection = Connection::session()
        .await
        .context("connect tray to session D-Bus")?;
    let watcher_monitor = ensure_watcher(&connection).await?;

    let watcher = watcher_proxy(&connection).await?;
    watcher
        .call_noreply("RegisterStatusNotifierHost", &WATCHER_PATH)
        .await
        .context("register status notifier host")?;

    let mut registered = watcher
        .receive_signal("StatusNotifierItemRegistered")
        .await
        .context("subscribe to tray item registrations")?;
    let mut unregistered = watcher
        .receive_signal("StatusNotifierItemUnregistered")
        .await
        .context("subscribe to tray item removals")?;
    let initial: Vec<String> = match watcher.get_property("RegisteredStatusNotifierItems").await {
        Ok(items) => items,
        Err(error) => {
            warn!(%error, "Could not read initially registered tray items");
            Vec::new()
        }
    };

    let mut monitors = HashMap::new();
    for service in initial {
        start_item_monitor(&connection, service, updates, &mut monitors);
    }

    loop {
        tokio::select! {
            message = registered.next() => {
                let Some(message) = message else { break };
                // A malformed signal must not take the whole backend down; skip it.
                match message.body().deserialize::<String>() {
                    Ok(service) => start_item_monitor(&connection, service, updates, &mut monitors),
                    Err(error) => warn!(%error, "Could not decode tray item registration signal"),
                }
            }
            message = unregistered.next() => {
                let Some(message) = message else { break };
                let service: String = match message.body().deserialize() {
                    Ok(service) => service,
                    Err(error) => {
                        warn!(%error, "Could not decode tray item removal signal");
                        continue;
                    }
                };
                if let Some(handle) = monitors.remove(&service) {
                    handle.abort();
                }
                if let Err(error) = updates.send(TrayUpdate::Remove(service.clone())) {
                    debug!(item = service, %error, "Tray UI receiver was dropped");
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                dispatch_command(&connection, command, menu_tx).await;
            }
        }
    }

    // The loop only breaks when a stream or channel closed under us; abort the
    // spawned tasks so a supervised retry starts from a clean slate.
    for handle in monitors.into_values() {
        handle.abort();
    }
    if let Some(handle) = watcher_monitor {
        handle.abort();
    }
    debug!("Tray backend stopped");
    Ok(())
}

// Supervised wrapper around run_tray. The inner session only returns when the
// watcher signal streams or the command channel close under it (session bus
// restart, UI teardown). Same backoff policy as the Hyprland/D-Bus supervisors —
// the failure modes are equivalent (IPC peer gone, transient setup error).
// Spec-compliant SNI applications watch the watcher name and re-register when a
// new one appears, so items repopulate after a reconnect on their own.
//
// This function never returns and is meant to be `tokio::spawn`ed from the
// widget setup. It owns the channel ends across retries: the GTK side keeps
// its receiver and senders wired to the same channels for the process lifetime.
pub async fn run_tray_supervised(backend: TrayBackend) {
    let TrayBackend {
        updates,
        mut commands,
        menus,
    } = backend;
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("🔌 Starting system tray backend");
        match run_tray(&updates, &mut commands, &menus).await {
            Ok(()) => {
                warn!("⚠️ Tray backend returned cleanly (stream closed)");
            }
            Err(e) => {
                error!("❌ Tray backend crashed: {:#}", e);
            }
        }

        if started.elapsed() >= reset_threshold {
            debug!(
                "🔄 Tray backend ran for {:?}, resetting backoff",
                started.elapsed()
            );
            delay = Duration::from_secs(1);
        }

        warn!("🔄 Restarting tray backend in {:?}", delay);
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_endpoints_round_trip_both_directions() {
        let (mut backend, mut ui) = channels();
        backend
            .updates
            .send(TrayUpdate::Remove("item".to_string()))
            .expect("UI update receiver should be connected");
        assert!(matches!(
            ui.updates.try_recv(),
            Ok(TrayUpdate::Remove(key)) if key == "item"
        ));

        ui.commands
            .send(TrayCommand {
                key: "item".to_string(),
                action: TrayAction::Activate,
                x: 4,
                y: 7,
                menu_path: String::new(),
            })
            .expect("backend command receiver should be connected");
        let command = backend
            .commands
            .try_recv()
            .expect("command should reach backend");
        assert_eq!(command.key, "item");
        assert!(matches!(command.action, TrayAction::Activate));
        assert_eq!((command.x, command.y), (4, 7));
    }

    #[test]
    fn service_is_split_at_first_path_separator() {
        assert_eq!(
            split_service(":1.42/StatusNotifierItem")
                .expect("test input has a status notifier object path"),
            (":1.42", "/StatusNotifierItem")
        );
        assert!(split_service(":1.42").is_err());
    }

    #[test]
    fn service_owner_uses_destination_prefix() {
        assert_eq!(service_owner(":1.42/StatusNotifierItem"), Some(":1.42"));
        assert_eq!(
            service_owner("org.example.App/Tray"),
            Some("org.example.App")
        );
        assert_eq!(service_owner("org.example.App"), None);
    }

    #[test]
    fn picks_largest_well_formed_pixmap() {
        let small = (1, 1, vec![0; 4]);
        let large = (2, 2, vec![0; 16]);
        let malformed = (32, 32, vec![0; 12]);
        assert_eq!(
            largest_valid_pixmap(vec![small, large.clone(), malformed]),
            Some(large)
        );
    }

    #[test]
    fn mnemonic_markers_are_dropped_and_escapes_collapsed() {
        assert_eq!(strip_mnemonic("_File".to_string()), "File");
        assert_eq!(strip_mnemonic("Save _As".to_string()), "Save As");
        assert_eq!(strip_mnemonic("snake__case".to_string()), "snake_case");
        assert_eq!(strip_mnemonic("plain".to_string()), "plain");
        assert_eq!(strip_mnemonic("trailing_".to_string()), "trailing");
    }

    // Build a layout node the same shape GetLayout returns: `(i id, a{sv}
    // properties, av children)`. The HashMap conversion wraps every property
    // value in a variant, which is also how zbus hands back `a{sv}` entries on
    // the wire — so these nodes exercise menu_prop's variant-unwrapping layer.
    fn layout_node<'a>(
        id: i32,
        props: Vec<(&'a str, zvariant::Value<'a>)>,
        children: Vec<zvariant::Value<'a>>,
    ) -> zvariant::Structure<'a> {
        let props: std::collections::HashMap<&str, zvariant::Value> = props.into_iter().collect();
        zvariant::StructureBuilder::new()
            .add_field(id)
            .add_field(props)
            .add_field(children)
            .build()
            .expect("test layout node is a valid structure")
    }

    #[test]
    fn parses_menu_node_with_variant_wrapped_properties() {
        let child = layout_node(
            7,
            vec![
                ("label", zvariant::Value::from("_Quit")),
                ("enabled", zvariant::Value::from(false)),
                ("toggle-type", zvariant::Value::from("checkmark")),
                ("toggle-state", zvariant::Value::from(1i32)),
            ],
            Vec::new(),
        );
        let root = layout_node(
            0,
            vec![("children-display", zvariant::Value::from("submenu"))],
            vec![zvariant::Value::Structure(child)],
        );

        let parsed = parse_menu_node(&root).expect("root node parses");
        assert_eq!(parsed.id, 0);
        assert_eq!(parsed.children.len(), 1);

        let entry = &parsed.children[0];
        assert_eq!(entry.id, 7);
        assert_eq!(entry.label.as_deref(), Some("Quit"));
        assert!(!entry.enabled);
        assert!(entry.visible, "visible defaults to true when omitted");
        assert!(!entry.is_separator);
        assert_eq!(entry.toggle_type.as_deref(), Some("checkmark"));
        assert_eq!(entry.toggle_state, Some(true));
    }

    #[test]
    fn parses_separator_node() {
        let separator = layout_node(
            3,
            vec![("type", zvariant::Value::from("separator"))],
            Vec::new(),
        );
        let parsed = parse_menu_node(&separator).expect("separator parses");
        assert!(parsed.is_separator);
        assert_eq!(parsed.label, None);
    }
}
