use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use zbus::fdo::DBusProxy;
use zbus::message::Header;
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
    pub icon_name: String,
    pub icon_pixmap: Option<IconPixmap>,
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
}

#[derive(Debug)]
pub struct TrayCommand {
    pub key: String,
    pub action: TrayAction,
    pub x: i32,
    pub y: i32,
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
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("missing D-Bus sender".into()))?;
        let item = normalize_registration(service, sender.as_str());

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
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("missing D-Bus sender".into()))?;
        let host = normalize_registration(service, sender.as_str());
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

fn normalize_registration(service: &str, sender: &str) -> String {
    if service.starts_with('/') {
        format!("{sender}{service}")
    } else if service.contains('/') {
        service.to_string()
    } else {
        format!("{service}{DEFAULT_ITEM_PATH}")
    }
}

pub fn split_service(service: &str) -> Result<(&str, &str)> {
    let slash = service
        .find('/')
        .with_context(|| format!("status notifier service has no object path: {service}"))?;
    Ok((&service[..slash], &service[slash..]))
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
        Ok(()) => info!("Serving StatusNotifierWatcher for tray applications"),
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

    Ok(TrayItem {
        key: key.to_string(),
        title,
        status,
        icon_name,
        icon_pixmap: largest_valid_pixmap(pixmaps),
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

async fn dispatch_command(connection: &Connection, command: TrayCommand) -> Result<()> {
    let proxy = item_proxy(connection, &command.key).await?;
    let method = match command.action {
        TrayAction::Activate => "Activate",
        TrayAction::SecondaryActivate => "SecondaryActivate",
        TrayAction::ContextMenu => "ContextMenu",
    };
    proxy
        .call_noreply(method, &(command.x, command.y))
        .await
        .with_context(|| format!("invoke {method} on {}", command.key))
}

pub async fn run_tray(
    updates: mpsc::UnboundedSender<TrayUpdate>,
    mut commands: mpsc::UnboundedReceiver<TrayCommand>,
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
                if let Err(error) = dispatch_command(&connection, command).await {
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
    fn registration_paths_are_normalized() {
        assert_eq!(
            normalize_registration("/CustomItem", ":1.42"),
            ":1.42/CustomItem"
        );
        assert_eq!(
            normalize_registration("org.example.App", ":1.42"),
            "org.example.App/StatusNotifierItem"
        );
        assert_eq!(
            normalize_registration("org.example.App/Tray", ":1.42"),
            "org.example.App/Tray"
        );
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
