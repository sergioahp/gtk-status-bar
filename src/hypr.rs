// Hyprland subsystem: workspace-client + workspace-label listeners.
//
// We connect to Hyprland's IPC event socket (.socket2.sock) via hyprland-rs's
// AsyncEventListener. activate() spawns supervised tokio tasks for the client
// and workspace listeners. If a listener errors out (EOF on the socket, parse
// failure on an unknown event variant, etc.), its wrapper retries with
// exponential backoff.

use std::time::{Duration, Instant};

use anyhow::Result;
use hyprland::event_listener::AsyncEventListener;
use hyprland::shared::{HyprData, HyprDataActive, HyprDataActiveOptional, HyprDataVec};
use tracing::{debug, error, info, warn};

use crate::bus::{Bus, WorkspaceClient, WorkspaceClientsUpdate, WorkspaceUpdate};

// Special workspaces have negative ids in Hyprland, but the activespecial
// event only carries names. This sentinel preserves the existing label data
// shape even though the workspace id is no longer used for title coloring.
const SPECIAL_WORKSPACE_COLOR_ID: hyprland::shared::WorkspaceId = -99;

pub fn format_workspace_name_from_string(name: &str, id: hyprland::shared::WorkspaceId) -> String {
    if name.is_empty() {
        return format!("Workspace {}", id);
    }
    format!("Workspace {}", name)
}

pub fn format_workspace_name_from_type(
    name: &hyprland::shared::WorkspaceType,
    id: hyprland::shared::WorkspaceId,
) -> String {
    match name {
        hyprland::shared::WorkspaceType::Regular(name) => {
            format_workspace_name_from_string(name, id)
        }
        hyprland::shared::WorkspaceType::Special(name_opt) => match name_opt {
            Some(name) if !name.is_empty() => format!("Special: {}", name),
            _ => format!("Special {}", id),
        },
    }
}

pub fn format_title_string(title: String, max_length: usize) -> String {
    if title.chars().count() <= max_length {
        title
    } else {
        // Reserve 1 of the max_length chars for the …, split the rest between
        // the two sides (right gets the odd char). The previous arithmetic
        // reserved nothing — output was max_length + 1 chars — and underflowed
        // for max_length < 2. saturating_sub keeps the degenerate max_length=0
        // case at a bare "…" instead of panicking.
        let chars_left = max_length.saturating_sub(1) / 2;
        let chars_right = max_length.saturating_sub(1) - chars_left;
        let crop_from_idx = title
            .char_indices()
            .nth(chars_left)
            .map(|(idx, _)| idx)
            .unwrap_or(chars_left);
        let crop_to_idx = title
            .char_indices()
            .nth(title.chars().count() - chars_right)
            .map(|(idx, _)| idx)
            .unwrap_or(title.len());
        format!("{}…{}", &title[..crop_from_idx], &title[crop_to_idx..])
    }
}

pub fn format_compact_title(title: &str, max_length: usize) -> String {
    title
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .chars()
        .take(max_length)
        .collect()
}

async fn get_workspace_clients_state() -> Result<WorkspaceClientsUpdate> {
    let active_client = hyprland::data::Client::get_active_async().await?;
    let workspace_id = match active_client.as_ref() {
        Some(client) => client.workspace.id,
        None => hyprland::data::Workspace::get_active_async().await?.id,
    };
    let active_address = active_client.as_ref().map(|client| &client.address);

    // Preserve Hyprland's compositor client-vector order after filtering.
    let clients = hyprland::data::Clients::get_async()
        .await?
        .to_vec()
        .into_iter()
        .filter(|client| client.mapped && client.workspace.id == workspace_id)
        .map(|client| {
            let active = active_address == Some(&client.address);
            let display_title = if client.title.trim().is_empty() {
                client.class.clone()
            } else {
                client.title
            };
            WorkspaceClient {
                address: client.address,
                compact_title: format_compact_title(&display_title, 10),
                title: format_title_string(display_title, 64),
                class: client.class,
                active,
            }
        })
        .collect();

    Ok(WorkspaceClientsUpdate {
        workspace_id,
        clients,
    })
}

async fn refresh_workspace_clients(reason: &'static str, bus: &Bus) -> Result<()> {
    let update = get_workspace_clients_state().await?;
    let active_address = update
        .clients
        .iter()
        .find(|client| client.active)
        .map(|client| client.address.to_string());
    debug!(
        reason,
        workspace_id = update.workspace_id,
        client_count = update.clients.len(),
        active_address,
        "Refreshing current workspace clients"
    );
    bus.send_clients_update(update)
}

async fn handle_workspace_change(
    workspace_data: hyprland::event_listener::WorkspaceEventData,
    bus: &Bus,
) -> Result<()> {
    debug!("Handling workspace change event");

    let display_name = format_workspace_name_from_type(&workspace_data.name, workspace_data.id);
    debug!("Workspace changed to: {}", display_name);

    // Send combined workspace update with both name and ID
    let update = WorkspaceUpdate {
        name: display_name,
        id: workspace_data.id,
    };
    bus.send_workspace_update(update)
}

// Supervised wrapper around setup_client_event_listener. The inner listener
// returns when Hyprland disconnects the IPC stream (EOF on .socket2.sock, parse
// failure on an unknown event variant, or any other I/O error in
// AsyncEventListener::start_listener_async). We log the cause, sleep with
// exponential backoff (1s -> 2s -> 4s -> ... capped at 60s), and reconnect.
// Backoff resets if the previous attempt ran for more than 30s, so a stable
// listener that briefly hiccups recovers fast, while a persistent failure
// (e.g. wrong env, Hyprland gone) doesn't busy-loop.
//
// This function never returns and is meant to be `tokio::spawn`ed from the
// widget setup.
pub async fn run_client_listener_supervised(bus: Bus) {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("🔌 Starting workspace client event listener");
        match setup_client_event_listener(&bus).await {
            Ok(()) => {
                warn!("⚠️ Workspace client event listener returned cleanly (unexpected)");
            }
            Err(e) => {
                error!("❌ Workspace client event listener crashed: {:#}", e);
            }
        }

        if started.elapsed() >= reset_threshold {
            debug!(
                "🔄 Workspace client listener ran for {:?}, resetting backoff",
                started.elapsed()
            );
            delay = Duration::from_secs(1);
        }

        warn!("🔄 Reconnecting workspace client listener in {:?}", delay);
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

// Same supervisor for the workspace listener; both consume Hyprland IPC and
// fail in the same shapes, so the policy is identical.
pub async fn run_workspace_listener_supervised(bus: Bus) {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("🔌 Starting workspace event listener");
        match setup_workspace_event_listener(&bus).await {
            Ok(()) => {
                warn!("⚠️ Workspace event listener returned cleanly (unexpected)");
            }
            Err(e) => {
                error!("❌ Workspace event listener crashed: {:#}", e);
            }
        }

        if started.elapsed() >= reset_threshold {
            debug!(
                "🔄 Workspace listener ran for {:?}, resetting backoff",
                started.elapsed()
            );
            delay = Duration::from_secs(1);
        }

        warn!("🔄 Reconnecting workspace listener in {:?}", delay);
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

pub async fn setup_client_event_listener(bus: &Bus) -> Result<()> {
    debug!("Setting up workspace client event listener");

    let initial_clients = get_workspace_clients_state().await.unwrap_or_else(|e| {
        error!("Failed to get initial workspace clients state: {}", e);
        WorkspaceClientsUpdate::default()
    });
    if let Err(e) = bus.send_clients_update(initial_clients) {
        error!("Failed to send initial workspace clients update: {}", e);
    }

    let mut event_listener = AsyncEventListener::new();

    // hyprland-rs's add_*_handler takes Fn(T) -> Pin<Box<dyn Future + Send>>;
    // its older `async_closure!` macro produced exactly that shape (and is now
    // deprecated). Native async-closure syntax returns `impl Future`, which
    // doesn't satisfy the trait bound, so we spell the Box::pin out instead.
    // Each handler clones the Bus twice: once into the closure (which must be
    // Fn, callable many times) and once per invocation into the async move.
    let title_bus = bus.clone();
    event_listener.add_window_title_changed_handler(move |_title_data| {
        let bus = title_bus.clone();
        Box::pin(async move {
            if let Err(e) = refresh_workspace_clients("title-changed", &bus).await {
                error!("Failed to refresh clients after title change: {}", e);
            }
        })
    });

    let window_bus = bus.clone();
    event_listener.add_active_window_changed_handler(move |_window_data| {
        let bus = window_bus.clone();
        Box::pin(async move {
            if let Err(e) = refresh_workspace_clients("active-window-changed", &bus).await {
                error!(
                    "Failed to refresh clients after active window change: {}",
                    e
                );
            }
        })
    });

    let opened_bus = bus.clone();
    event_listener.add_window_opened_handler(move |window_data| {
        let bus = opened_bus.clone();
        Box::pin(async move {
            debug!(window = ?window_data, "Window opened");
            if let Err(e) = refresh_workspace_clients("window-opened", &bus).await {
                error!("Failed to refresh clients after window open: {}", e);
            }
        })
    });

    let closed_bus = bus.clone();
    event_listener.add_window_closed_handler(move |address| {
        let bus = closed_bus.clone();
        Box::pin(async move {
            debug!(%address, "Window closed");
            if let Err(e) = refresh_workspace_clients("window-closed", &bus).await {
                error!("Failed to refresh clients after window close: {}", e);
            }
        })
    });

    let moved_bus = bus.clone();
    event_listener.add_window_moved_handler(move |window_data| {
        let bus = moved_bus.clone();
        Box::pin(async move {
            debug!(window = ?window_data, "Window moved");
            if let Err(e) = refresh_workspace_clients("window-moved", &bus).await {
                error!("Failed to refresh clients after window move: {}", e);
            }
        })
    });

    let workspace_bus = bus.clone();
    event_listener.add_workspace_changed_handler(move |workspace_data| {
        let bus = workspace_bus.clone();
        Box::pin(async move {
            debug!(workspace = ?workspace_data, "Workspace changed for client strip");
            if let Err(e) = refresh_workspace_clients("workspace-changed", &bus).await {
                error!("Failed to refresh clients after workspace change: {}", e);
            }
        })
    });

    let special_bus = bus.clone();
    event_listener.add_changed_special_handler(move |workspace_data| {
        let bus = special_bus.clone();
        Box::pin(async move {
            debug!(workspace = ?workspace_data, "Special workspace changed for client strip");
            if let Err(e) = refresh_workspace_clients("special-workspace-changed", &bus).await {
                error!("Failed to refresh clients for special workspace: {}", e);
            }
        })
    });

    let special_removed_bus = bus.clone();
    event_listener.add_special_removed_handler(move |monitor| {
        let bus = special_removed_bus.clone();
        Box::pin(async move {
            debug!(monitor, "Special workspace removed for client strip");
            if let Err(e) = refresh_workspace_clients("special-workspace-removed", &bus).await {
                error!(
                    "Failed to refresh clients after special workspace removal: {}",
                    e
                );
            }
        })
    });

    info!("Starting workspace client event listener");
    event_listener.start_listener_async().await?;

    Ok(())
}

pub async fn setup_workspace_event_listener(bus: &Bus) -> Result<()> {
    debug!("Setting up workspace event listener");

    let workspace_result = hyprland::data::Workspace::get_active_async().await;

    match workspace_result {
        Ok(workspace) => {
            let initial_state = format_workspace_name_from_string(&workspace.name, workspace.id);
            let update = WorkspaceUpdate {
                name: initial_state,
                id: workspace.id,
            };
            if let Err(e) = bus.send_workspace_update(update) {
                error!("Failed to send initial workspace update: {}", e);
            }
        }
        Err(e) => {
            error!("Failed to get initial workspace state: {}", e);
            let fallback_update = WorkspaceUpdate {
                name: "Workspace ?".to_string(),
                id: 1, // WorkspaceId is just an i32
            };
            if let Err(e) = bus.send_workspace_update(fallback_update) {
                error!("Failed to send fallback workspace update: {}", e);
            }
        }
    }

    let mut event_listener = AsyncEventListener::new();

    let workspace_bus = bus.clone();
    event_listener.add_workspace_changed_handler(move |workspace_data| {
        let bus = workspace_bus.clone();
        Box::pin(async move {
            if let Err(e) = handle_workspace_change(workspace_data, &bus).await {
                error!("Failed to handle workspace change: {}", e);
            }
        })
    });

    // Special workspaces arrive via their own Hyprland event ("activespecial"),
    // not the workspace-changed one, so without these two handlers toggling a
    // special workspace left the bar frozen on the previous regular workspace
    // (VM evidence run, item W3). hyprland-rs splits the event: non-empty name
    // means a special workspace became visible, empty name (SpecialRemoved)
    // means it was hidden again.
    let special_bus = bus.clone();
    event_listener.add_changed_special_handler(move |special_data| {
        let bus = special_bus.clone();
        Box::pin(async move {
            // The event carries names only; special workspaces have negative
            // ids in Hyprland, so preserve a sentinel id in the update.
            let name = special_data
                .workspace_name
                .strip_prefix("special:")
                .unwrap_or(&special_data.workspace_name)
                .to_string();
            let update = WorkspaceUpdate {
                name: format_workspace_name_from_type(
                    &hyprland::shared::WorkspaceType::Special(Some(name)),
                    SPECIAL_WORKSPACE_COLOR_ID,
                ),
                id: SPECIAL_WORKSPACE_COLOR_ID,
            };
            if let Err(e) = bus.send_workspace_update(update) {
                error!("Failed to send special workspace update: {}", e);
            }
        })
    });

    let special_removed_bus = bus.clone();
    event_listener.add_special_removed_handler(move |_monitor| {
        let bus = special_removed_bus.clone();
        Box::pin(async move {
            // The special workspace was hidden; restore the regular active
            // workspace (name + color) by querying it.
            match hyprland::data::Workspace::get_active_async().await {
                Ok(workspace) => {
                    let update = WorkspaceUpdate {
                        name: format_workspace_name_from_string(&workspace.name, workspace.id),
                        id: workspace.id,
                    };
                    if let Err(e) = bus.send_workspace_update(update) {
                        error!(
                            "Failed to send workspace update after special removal: {}",
                            e
                        );
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to query active workspace after special removal: {}",
                        e
                    );
                }
            }
        })
    });

    info!("Starting workspace event listener");
    event_listener.start_listener_async().await?;

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use hyprland::shared::WorkspaceType;

    // format_title_string: short input passes through unchanged.
    #[test]
    fn format_title_short_passthrough() {
        assert_eq!(format_title_string("hello".to_string(), 10), "hello");
    }

    // Exactly max_length chars also passes through (≤ comparison).
    #[test]
    fn format_title_exact_max_length_passthrough() {
        let s = "0123456789".to_string();
        assert_eq!(s.chars().count(), 10);
        assert_eq!(format_title_string(s.clone(), 10), s);
    }

    // Empty string is a no-op regardless of max_length.
    #[test]
    fn format_title_empty_passthrough() {
        assert_eq!(format_title_string(String::new(), 64), "");
    }

    // Long input gets cropped with an ellipsis in the middle, and the output
    // fits max_length exactly (the … counts toward the limit). For
    // max_length=10: chars_left=4, chars_right=5.
    #[test]
    fn format_title_long_cropped_with_ellipsis() {
        let input = "1234567890ABCDEF".to_string();
        let out = format_title_string(input, 10);
        assert_eq!(out, "1234…BCDEF");
        assert!(out.contains('…'));
        // Output is chars_left + 1 (…) + chars_right = max_length chars.
        assert_eq!(out.chars().count(), 10);
    }

    // Multi-byte UTF-8 must crop on character boundaries, not byte indices —
    // slicing a multi-byte char mid-byte would panic. Each emoji is 4 bytes
    // but counts as 1 char, so chars().count() and byte length diverge.
    #[test]
    fn format_title_multibyte_utf8_crops_on_char_boundary() {
        // 16 chars, each a 4-byte emoji => 64 bytes total.
        let input: String = "🚀".repeat(16);
        assert_eq!(input.chars().count(), 16);
        assert_eq!(input.len(), 64);
        let out = format_title_string(input, 10);
        // Should not panic, should contain the ellipsis.
        assert!(out.contains('…'));
        // 4 emoji + … + 5 emoji = 10 chars
        assert_eq!(out.chars().count(), 10);
    }

    // Degenerate limits must not underflow usize (the old arithmetic panicked
    // for max_length < 2 on any over-long input). max_length=1 crops to just
    // the ellipsis; max_length=0 degrades to the same single char.
    #[test]
    fn format_title_tiny_max_length_does_not_underflow() {
        assert_eq!(format_title_string("abcdef".to_string(), 1), "…");
        assert_eq!(format_title_string("abcdef".to_string(), 0), "…");
        // max_length=2: 0 left, 1 right.
        assert_eq!(format_title_string("abcdef".to_string(), 2), "…f");
    }

    #[test]
    fn compact_title_uses_the_first_word_without_an_ellipsis() {
        assert_eq!(format_compact_title("Mozilla Firefox", 10), "Mozilla");
        assert_eq!(
            format_compact_title("abcdefghijklmnop second", 8),
            "abcdefgh"
        );
        assert_eq!(format_compact_title("  hello   world  ", 10), "hello");
    }

    #[test]
    fn compact_title_handles_utf8_and_tiny_limits() {
        assert_eq!(format_compact_title("界🙂界🙂界 next", 4), "界🙂界🙂");
        assert_eq!(format_compact_title("hello", 0), "");
        assert_eq!(format_compact_title("", 10), "");
    }

    // format_workspace_name_from_string: empty name falls back to id.
    #[test]
    fn workspace_name_from_string_empty_uses_id() {
        assert_eq!(format_workspace_name_from_string("", 3), "Workspace 3");
    }

    #[test]
    fn workspace_name_from_string_non_empty() {
        assert_eq!(format_workspace_name_from_string("dev", 1), "Workspace dev");
    }

    // format_workspace_name_from_type: Regular delegates to the string form.
    #[test]
    fn workspace_name_from_type_regular_delegates() {
        let ws = WorkspaceType::Regular("scratch".to_string());
        assert_eq!(format_workspace_name_from_type(&ws, 7), "Workspace scratch");
    }

    // Special with a name uses "Special: <name>".
    #[test]
    fn workspace_name_from_type_special_with_name() {
        let ws = WorkspaceType::Special(Some("magic".to_string()));
        assert_eq!(format_workspace_name_from_type(&ws, 4), "Special: magic");
    }

    // Special with None falls back to "Special <id>".
    #[test]
    fn workspace_name_from_type_special_none_uses_id() {
        let ws = WorkspaceType::Special(None);
        assert_eq!(format_workspace_name_from_type(&ws, 5), "Special 5");
    }

    // Special with Some("") is treated like None per the guard `if !name.is_empty()`.
    #[test]
    fn workspace_name_from_type_special_empty_string_uses_id() {
        let ws = WorkspaceType::Special(Some(String::new()));
        assert_eq!(format_workspace_name_from_type(&ws, 9), "Special 9");
    }
}
