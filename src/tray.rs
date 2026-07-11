use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
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
    pub items: Vec<TrayMenuItem>,
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
    ContextMenu,
    /// Activate a specific entry of the item's `com.canonical.dbusmenu` layout.
    /// `x` carries the dbusmenu entry id so we can forward it to the `Event`
    /// method (the rest of TrayCommand is unused for this action).
    MenuEvent(i32),
}

#[derive(Debug)]
pub struct TrayCommand {
    pub key: String,
    pub action: TrayAction,
    pub x: i32,
    pub y: i32,
    /// Object path of the item's `com.canonical.dbusmenu`, captured when the
    /// click happened so the backend does not have to re-fetch it to render the
    /// menu locally.
    pub menu_path: String,
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
            Self::status_notifier_item_registered(&emitter, &item).await?;
            self.registered_status_notifier_items_changed(&emitter)
                .await?;
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
                Self::status_notifier_host_registered(&emitter).await?;
                self.is_status_notifier_host_registered_changed(&emitter)
                    .await?;
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
    async fn remove_owner(&mut self, owner: &str, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
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
                Self::status_notifier_item_unregistered(emitter, &service).await?;
                info!(item = service, "Status notifier item disappeared");
            }
            self.registered_status_notifier_items_changed(emitter)
                .await?;
        }

        let had_hosts = !self.hosts.is_empty();
        self.hosts
            .retain(|service| service_owner(service) != Some(owner));
        if had_hosts && self.hosts.is_empty() {
            Self::status_notifier_host_unregistered(emitter).await?;
            self.is_status_notifier_host_registered_changed(emitter)
                .await?;
        }
        Ok(())
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
    let dbus = DBusProxy::new(&connection).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;
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
                warn!(%error, "Could not access local StatusNotifierWatcher state");
                return Ok(());
            }
        };
        let emitter = interface.signal_emitter().clone();
        let mut watcher = interface.get_mut().await;
        if let Err(error) = watcher.remove_owner(args.name.as_str(), &emitter).await {
            warn!(owner = %args.name, %error, "Could not remove vanished tray owner");
        }
    }
    Ok(())
}

async fn ensure_watcher(connection: &Connection) -> Result<()> {
    let dbus = DBusProxy::new(connection).await?;
    if dbus.name_has_owner(WATCHER_NAME.try_into()?).await? {
        info!("Using the existing StatusNotifierWatcher");
        return Ok(());
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
            tokio::spawn(async move {
                if let Err(error) = monitor_watcher_owners(connection).await {
                    warn!(%error, "StatusNotifierWatcher owner monitor stopped");
                }
            });
        }
        Err(zbus::Error::NameTaken) => info!("Another StatusNotifierWatcher won startup race"),
        Err(error) => return Err(error).context("request StatusNotifierWatcher bus name"),
    }
    Ok(())
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

async fn fetch_item(connection: &Connection, key: &str) -> Result<TrayItem> {
    let proxy = item_proxy(connection, key).await?;
    let title = match optional_property(&proxy, "Title").await {
        Some(title) => title,
        None => String::new(),
    };
    let status: String = match optional_property(&proxy, "Status").await {
        Some(status) => status,
        None => "Active".to_string(),
    };
    let item_is_menu = match optional_property(&proxy, "ItemIsMenu").await {
        Some(item_is_menu) => item_is_menu,
        None => false,
    };

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
    let icon_name = match optional_property(&proxy, icon_name_property).await {
        Some(icon_name) => icon_name,
        None => String::new(),
    };
    let pixmaps = match optional_property(&proxy, pixmap_property).await {
        Some(pixmaps) => pixmaps,
        None => Vec::new(),
    };
    let icon_theme_path = match optional_property(&proxy, "IconThemePath").await {
        Some(icon_theme_path) => icon_theme_path,
        None => String::new(),
    };
    let menu_path = match optional_property::<zvariant::OwnedObjectPath>(&proxy, "Menu").await {
        Some(menu_path) => menu_path.as_str().to_string(),
        None => String::new(),
    };

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
        .map_err(|error| {
            warn!(
                item = key,
                path = menu_path,
                destination,
                ?error,
                "dbusmenu GetLayout call failed"
            );
            anyhow::anyhow!("fetch dbusmenu layout for {key}: {error}")
        })?;

    // The reply is `(u revision, (i id, a{sv} properties, av children))`. We read
    // it as a raw Structure and walk it by hand because the children are
    // recursively nested structs, which zbus's derive machinery cannot name.
    let body = message.body();
    let structure: zvariant::Structure<'_> = body
        .deserialize()
        .map_err(|error| anyhow::anyhow!("decode dbusmenu GetLayout reply for {key}: {error}"))?;
    let outer = structure.fields();
    let _revision = outer.first();
    let inner = match outer.get(1) {
        Some(zvariant::Value::Structure(structure)) => structure,
        _ => {
            return Err(anyhow::anyhow!(
                "dbusmenu layout for {key} has no inner node struct"
            ));
        }
    };
    let root = parse_menu_node(inner)
        .ok_or_else(|| anyhow::anyhow!("parse dbusmenu layout for {key}"))?;

    debug!(
        item = key,
        count = root.children.len(),
        "dbusmenu layout parsed"
    );

    Ok(TrayMenu {
        key: key.to_string(),
        items: root.children,
    })
}

// Forward a menu entry activation back to the app through dbusmenu's `Event`
// method. The visual menu is ours, but the app still owns the action.
async fn dbusmenu_event(connection: &Connection, key: &str, id: i32) -> Result<()> {
    let (destination, _) = split_service(key)?;
    let item = item_proxy(connection, key).await?;
    let menu_path: zvariant::OwnedObjectPath = match optional_property(&item, "Menu").await {
        Some(path) => path,
        None => {
            warn!(item = key, "Tray item has no dbusmenu to deliver event to");
            return Ok(());
        }
    };
    let menu_proxy = Proxy::new(connection, destination, menu_path.as_str(), DBUSMENU_INTERFACE)
        .await
        .with_context(|| format!("create dbusmenu proxy for event on {key}"))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as u32)
        .unwrap_or(0);
    menu_proxy
        .call_method("Event", &(id, "clicked", zvariant::Value::U32(0), timestamp))
        .await
        .with_context(|| format!("deliver dbusmenu event for entry {id} of {key}"))?;
    Ok(())
}

fn menu_prop<'a>(
    props: &'a zvariant::Dict<'a, 'a>,
    name: &str,
) -> Option<&'a zvariant::Value<'a>> {
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

// dbusmenu uses `_` as a keyboard mnemonic marker; strip it so it does not show
// up as a literal underscore in our native menu.
fn strip_mnemonic(label: String) -> String {
    label.replace('_', "")
}

fn as_structure<'a>(value: &'a zvariant::Value<'a>) -> Option<&'a zvariant::Structure<'a>> {
    match value {
        zvariant::Value::Structure(structure) => Some(structure),
        zvariant::Value::Value(inner) => as_structure(&**inner),
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

async fn dispatch_command(
    connection: &Connection,
    command: TrayCommand,
    menu_tx: &mpsc::UnboundedSender<TrayMenu>,
) -> Result<()> {
    debug!(
        item = command.key,
        action = ?command.action,
        x = command.x,
        y = command.y,
        "Tray backend received command"
    );
    match command.action {
        TrayAction::Activate => {
            let proxy = item_proxy(connection, &command.key).await?;
            proxy
                .call_noreply("Activate", &(command.x, command.y))
                .await
                .with_context(|| format!("invoke Activate on {}", command.key))?;
        }
        TrayAction::SecondaryActivate => {
            let proxy = item_proxy(connection, &command.key).await?;
            proxy
                .call_noreply("SecondaryActivate", &(command.x, command.y))
                .await
                .with_context(|| format!("invoke SecondaryActivate on {}", command.key))?;
        }
            TrayAction::ContextMenu => {
                // Mirror eww: paint the dbusmenu ourselves. Fall back to the SNI's
                // own ContextMenu method for the rare item that draws its own menu.
                let menu_path = if !command.menu_path.is_empty() {
                    Some(command.menu_path.clone())
                } else {
                    let proxy = item_proxy(connection, &command.key).await?;
                    optional_property::<zvariant::OwnedObjectPath>(&proxy, "Menu")
                        .await
                        .map(|path| path.as_str().to_string())
                };
                match menu_path {
                Some(path) if !path.is_empty() => match fetch_menu(connection, &command.key, &path).await {
                    Ok(menu) => {
                        if let Err(error) = menu_tx.send(menu) {
                            warn!(item = command.key, %error, "Could not deliver tray menu to UI");
                        }
                    }
                    Err(error) => warn!(item = command.key, error = ?error, "Could not read tray menu; app must draw it"),
                },
                _ => {
                    let proxy = item_proxy(connection, &command.key).await?;
                    proxy
                        .call_noreply("ContextMenu", &(command.x, command.y))
                        .await
                        .with_context(|| format!("invoke ContextMenu on {}", command.key))?;
                }
            }
        }
        TrayAction::MenuEvent(id) => {
            if let Err(error) = dbusmenu_event(connection, &command.key, id).await {
                warn!(%error, "Tray menu event failed");
            }
        }
    }
    Ok(())
}

pub async fn run_tray(
    updates: mpsc::UnboundedSender<TrayUpdate>,
    mut commands: mpsc::UnboundedReceiver<TrayCommand>,
    menu_tx: mpsc::UnboundedSender<TrayMenu>,
) -> Result<()> {
    let connection = Connection::session()
        .await
        .context("connect tray to session D-Bus")?;
    ensure_watcher(&connection).await?;

    let watcher = watcher_proxy(&connection).await?;
    watcher
        .call_noreply("RegisterStatusNotifierHost", &WATCHER_PATH)
        .await
        .context("register status notifier host")?;

    let mut registered = watcher
        .receive_signal("StatusNotifierItemRegistered")
        .await?;
    let mut unregistered = watcher
        .receive_signal("StatusNotifierItemUnregistered")
        .await?;
    let initial: Vec<String> = match watcher.get_property("RegisteredStatusNotifierItems").await {
        Ok(items) => items,
        Err(error) => {
            warn!(%error, "Could not read initially registered tray items");
            Vec::new()
        }
    };

    let mut monitors = HashMap::new();
    for service in initial {
        start_item_monitor(&connection, service, &updates, &mut monitors);
    }

    loop {
        tokio::select! {
            message = registered.next() => {
                let Some(message) = message else { break };
                let service: String = message.body().deserialize()?;
                start_item_monitor(&connection, service, &updates, &mut monitors);
            }
            message = unregistered.next() => {
                let Some(message) = message else { break };
                let service: String = message.body().deserialize()?;
                if let Some(handle) = monitors.remove(&service) {
                    handle.abort();
                }
                if let Err(error) = updates.send(TrayUpdate::Remove(service.clone())) {
                    debug!(item = service, %error, "Tray UI receiver was dropped");
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                if let Err(error) = dispatch_command(&connection, command, &menu_tx).await {
                    warn!(%error, "Tray click failed");
                }
            }
        }
    }

    debug!("Tray backend stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
