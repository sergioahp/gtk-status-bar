// Desktop color-scheme integration. Reads org.freedesktop.appearance
// color-scheme from the settings portal at startup, then watches the portal's
// SettingChanged signal so the bar's GTK prefer-dark tracks a live light/dark
// switch instead of only the value present at launch.
//
// The producer here only decides a bool and forwards it; the GTK-side consumer
// (widgets::setup_color_scheme_updates) applies it to GtkSettings on the main
// thread, matching the consumer-before-producer wiring every other subsystem
// uses.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zbus::{Connection, Proxy};

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS_INTERFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";

// The portal encodes color-scheme as a uint: 1 = prefer dark, 2 = prefer light,
// 0 = no preference. We only act on an explicit preference; "no preference"
// leaves the bar on its current (dark) default.
fn scheme_to_prefer_dark(scheme: u32) -> Option<bool> {
    match scheme {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

// Read and SettingChanged both hand the value back wrapped in a `v`, and zbus
// can nest a second `Value::Value` on top; peel those layers until the integer
// surfaces.
fn value_to_prefer_dark(mut value: zvariant::Value<'_>) -> Option<bool> {
    while let zvariant::Value::Value(inner) = value {
        value = *inner;
    }
    scheme_to_prefer_dark(u32::try_from(value).ok()?)
}

// One-shot read for startup, before any widget realizes. Blocking so the value
// is applied to GtkSettings before the first frame. Returns None when the portal
// or the setting is unavailable (e.g. no color-scheme portal on the session).
pub fn detect_prefer_dark() -> Option<bool> {
    use zbus::blocking::Connection;

    let connection = Connection::session().ok()?;
    let reply = connection
        .call_method(
            Some(PORTAL_DEST),
            PORTAL_PATH,
            Some(SETTINGS_INTERFACE),
            "Read",
            &(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY),
        )
        .ok()?;
    let body = reply.body();
    let value: zvariant::Value = body.deserialize().ok()?;
    value_to_prefer_dark(value)
}

async fn read_prefer_dark(proxy: &Proxy<'_>) -> Option<bool> {
    let reply = proxy
        .call_method("Read", &(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY))
        .await
        .ok()?;
    let body = reply.body();
    let value: zvariant::Value = body.deserialize().ok()?;
    value_to_prefer_dark(value)
}

// Connect, emit the current preference so a reconnect re-syncs the bar, then
// forward every color-scheme change until a stream or the channel closes.
async fn run_color_scheme(tx: &mpsc::UnboundedSender<bool>) -> Result<()> {
    let connection = Connection::session()
        .await
        .context("connect color-scheme watcher to session D-Bus")?;
    let proxy = Proxy::new(&connection, PORTAL_DEST, PORTAL_PATH, SETTINGS_INTERFACE)
        .await
        .context("create portal Settings proxy")?;

    if let Some(prefer_dark) = read_prefer_dark(&proxy).await
        && tx.send(prefer_dark).is_err() {
            return Ok(());
        }

    let mut changes = proxy
        .receive_signal("SettingChanged")
        .await
        .context("subscribe to portal SettingChanged")?;
    while let Some(message) = changes.next().await {
        let body = message.body();
        let (namespace, key, value): (String, String, zvariant::Value) = match body.deserialize() {
            Ok(parts) => parts,
            Err(error) => {
                debug!(%error, "Could not decode SettingChanged signal");
                continue;
            }
        };
        if namespace != APPEARANCE_NAMESPACE || key != COLOR_SCHEME_KEY {
            continue;
        }
        let Some(prefer_dark) = value_to_prefer_dark(value) else {
            debug!("color-scheme changed to no-preference; leaving the bar as-is");
            continue;
        };
        info!(prefer_dark, "Desktop color-scheme changed");
        if tx.send(prefer_dark).is_err() {
            break;
        }
    }
    Ok(())
}

// Supervised wrapper: the portal comes and goes with xdg-desktop-portal, so
// retry with the same backoff policy as the other D-Bus-backed producers rather
// than giving up after one failure. Returns once the GTK consumer is gone.
pub async fn run_color_scheme_supervised(tx: mpsc::UnboundedSender<bool>) {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("Starting desktop color-scheme watcher");
        match run_color_scheme(&tx).await {
            Ok(()) => warn!("Color-scheme watcher stopped (stream closed)"),
            Err(error) => error!("Color-scheme watcher failed: {:#}", error),
        }

        if tx.is_closed() {
            debug!("Color-scheme consumer is gone; stopping watcher");
            return;
        }
        if started.elapsed() >= reset_threshold {
            delay = Duration::from_secs(1);
        }
        warn!(restart_delay = ?delay, "Restarting color-scheme watcher");
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_uint_maps_to_explicit_preference_only() {
        assert_eq!(scheme_to_prefer_dark(1), Some(true));
        assert_eq!(scheme_to_prefer_dark(2), Some(false));
        assert_eq!(scheme_to_prefer_dark(0), None);
        assert_eq!(scheme_to_prefer_dark(7), None);
    }

    #[test]
    fn value_is_unwrapped_through_nested_variants() {
        // The portal returns the uint double-wrapped (`v v u`), so peeling has to
        // survive more than one Value::Value layer.
        let nested = zvariant::Value::Value(Box::new(zvariant::Value::U32(1)));
        assert_eq!(value_to_prefer_dark(nested), Some(true));
        assert_eq!(value_to_prefer_dark(zvariant::Value::U32(2)), Some(false));
        assert_eq!(value_to_prefer_dark(zvariant::Value::U32(0)), None);
    }
}
