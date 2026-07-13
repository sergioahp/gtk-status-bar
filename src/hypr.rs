// Hyprland subsystem: title + workspace listeners.
//
// We connect to Hyprland's IPC event socket (.socket2.sock) via hyprland-rs's
// AsyncEventListener. activate() spawns supervised tokio tasks for the title
// and workspace listeners. If a listener errors out (EOF on the socket, parse
// failure on an unknown event variant, etc.), its wrapper retries with
// exponential backoff.

use std::time::{Duration, Instant};

use anyhow::Result;
use hyprland::event_listener::AsyncEventListener;
use hyprland::shared::{HyprDataActive, HyprDataActiveOptional};
use tracing::{debug, error, info, warn};

use crate::bus::{Bus, TitleUpdate, WorkspaceUpdate};

// Special workspaces have negative ids in Hyprland, but the activespecial
// event only carries names. Any negative id lands on the default arm of
// get_workspace_color; -99 is pinned by a widgets test.
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

async fn get_initial_title_state() -> Result<TitleUpdate> {
    // We do want to know when the operation is successfull but the title string is not there,
    // which would be because there is no active client
    debug!("Fetching initial title state");

    let client = hyprland::data::Client::get_active_async().await?;
    let update = match client {
        Some(client) => TitleUpdate {
            title: format_title_string(client.title, 64),
            class: client.class,
        },
        None => TitleUpdate::default(),
    };

    debug!(
        title = update.title,
        class = update.class,
        "Initial title state"
    );
    Ok(update)
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

async fn handle_title_change(
    title_data: hyprland::event_listener::WindowTitleEventData,
    bus: &Bus,
) -> Result<()> {
    debug!("Handling title change event");

    // If not active client skip event except if there is no active client, use title_data.address
    let active_client = hyprland::data::Client::get_active_async()
        .await?
        // log + early return, not as debug it is normal sometimes for it to not be an active client,
        // use combinators
        .filter(|client| client.address == title_data.address);

    if let Some(client) = active_client {
        let update = TitleUpdate {
            title: format_title_string(client.title, 64),
            class: client.class,
        };
        debug!(title = update.title, class = update.class, "Title changed");
        bus.send_title_update(update)
    } else {
        debug!("No active client matches the title change event");
        Ok(())
    }
}

async fn handle_active_window_change(
    window_data: Option<hyprland::event_listener::WindowEventData>,
    bus: &Bus,
) -> Result<()> {
    debug!("Handling active window change event");

    let update = match window_data {
        Some(data) => {
            debug!(
                "Window data - class: '{}', title: '{}', address: '{}'",
                data.class, data.title, data.address
            );
            TitleUpdate {
                title: format_title_string(data.title, 64),
                class: data.class,
            }
        }
        None => {
            debug!("No active window (window_data is None)");
            TitleUpdate::default()
        }
    };

    debug!(
        title = update.title,
        class = update.class,
        "Active window changed"
    );
    bus.send_title_update(update)
}

// Supervised wrapper around setup_title_event_listener. The inner listener
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
pub async fn run_title_listener_supervised(bus: Bus) {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("🔌 Starting title event listener");
        match setup_title_event_listener(&bus).await {
            Ok(()) => {
                warn!("⚠️ Title event listener returned cleanly (unexpected)");
            }
            Err(e) => {
                error!("❌ Title event listener crashed: {:#}", e);
            }
        }

        if started.elapsed() >= reset_threshold {
            debug!(
                "🔄 Title listener ran for {:?}, resetting backoff",
                started.elapsed()
            );
            delay = Duration::from_secs(1);
        }

        warn!("🔄 Reconnecting title listener in {:?}", delay);
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

pub async fn setup_title_event_listener(bus: &Bus) -> Result<()> {
    debug!("Setting up title event listener");

    let initial_state = get_initial_title_state().await.unwrap_or_else(|e| {
        error!("Failed to get initial title state: {}", e);
        TitleUpdate::default()
    });

    if let Err(e) = bus.send_title_update(initial_state) {
        error!("Failed to send initial title update: {}", e);
    }

    let mut event_listener = AsyncEventListener::new();

    // hyprland-rs's add_*_handler takes Fn(T) -> Pin<Box<dyn Future + Send>>;
    // its older `async_closure!` macro produced exactly that shape (and is now
    // deprecated). Native async-closure syntax returns `impl Future`, which
    // doesn't satisfy the trait bound, so we spell the Box::pin out instead.
    // Each handler clones the Bus twice: once into the closure (which must be
    // Fn, callable many times) and once per invocation into the async move.
    let title_bus = bus.clone();
    event_listener.add_window_title_changed_handler(move |title_data| {
        let bus = title_bus.clone();
        Box::pin(async move {
            if let Err(e) = handle_title_change(title_data, &bus).await {
                error!("Failed to handle title change: {}", e);
            }
        })
    });

    let window_bus = bus.clone();
    event_listener.add_active_window_changed_handler(move |window_data| {
        let bus = window_bus.clone();
        Box::pin(async move {
            if let Err(e) = handle_active_window_change(window_data, &bus).await {
                error!("Failed to handle active window change: {}", e);
            }
        })
    });

    info!("Starting title event listener");
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
            // ids in Hyprland, so use a sentinel that hits the default color
            // arm of get_workspace_color.
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
