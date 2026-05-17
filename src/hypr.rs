// Hyprland subsystem: title + workspace listeners.
//
// We connect to Hyprland's IPC event socket (.socket2.sock) via hyprland-rs's
// AsyncEventListener. The listener is spawned as a tokio task from the widget
// layer; if it errors out (EOF on the socket, parse failure on an unknown
// event variant, etc.), the spawned task currently logs and exits — see
// run_*_listener for the supervised wrappers that retry with backoff.

use std::time::{Duration, Instant};

use anyhow::Result;
use hyprland::shared::{HyprDataActive, HyprDataActiveOptional};
use hyprland::event_listener::AsyncEventListener;
use tracing::{debug, error, info, warn};

use crate::bus::{
    self, WorkspaceUpdate,
};

pub fn format_workspace_name_from_string(name: &str, id: hyprland::shared::WorkspaceId) -> String {
    if name.is_empty() {
        return format!("Workspace {}", id);
    }
    format!("Workspace {}", name)
}

pub fn format_workspace_name_from_type(name: &hyprland::shared::WorkspaceType, id: hyprland::shared::WorkspaceId) -> String {
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

pub fn format_title_string(title: String, max_length: usize) -> String {
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

    debug!("Initial title: {:?}", display_name);
    Ok(display_name)
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
    bus::send_workspace_update(update)
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
        bus::send_title_update(Some(formatted_title))
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
    bus::send_title_update(Some(formatted_title))
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
pub async fn run_title_listener_supervised() {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("🔌 Starting title event listener");
        match setup_title_event_listener().await {
            Ok(()) => {
                warn!("⚠️ Title event listener returned cleanly (unexpected)");
            }
            Err(e) => {
                error!("❌ Title event listener crashed: {:#}", e);
            }
        }

        if started.elapsed() >= reset_threshold {
            debug!("🔄 Title listener ran for {:?}, resetting backoff", started.elapsed());
            delay = Duration::from_secs(1);
        }

        warn!("🔄 Reconnecting title listener in {:?}", delay);
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

// Same supervisor for the workspace listener; both consume Hyprland IPC and
// fail in the same shapes, so the policy is identical.
pub async fn run_workspace_listener_supervised() {
    let max_delay = Duration::from_secs(60);
    let reset_threshold = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);

    loop {
        let started = Instant::now();
        info!("🔌 Starting workspace event listener");
        match setup_workspace_event_listener().await {
            Ok(()) => {
                warn!("⚠️ Workspace event listener returned cleanly (unexpected)");
            }
            Err(e) => {
                error!("❌ Workspace event listener crashed: {:#}", e);
            }
        }

        if started.elapsed() >= reset_threshold {
            debug!("🔄 Workspace listener ran for {:?}, resetting backoff", started.elapsed());
            delay = Duration::from_secs(1);
        }

        warn!("🔄 Reconnecting workspace listener in {:?}", delay);
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

pub async fn setup_title_event_listener() -> Result<()> {
    debug!("Setting up title event listener");

    let initial_state = get_initial_title_state().await
        .unwrap_or_else(|e| {
            error!("Failed to get initial title state: {}", e);
            "".to_string()
        });

    if let Err(e) = bus::send_title_update(Some(initial_state)) {
        error!("Failed to send initial title update: {}", e);
    }

    let mut event_listener = AsyncEventListener::new();

    // hyprland-rs's add_*_handler takes Fn(T) -> Pin<Box<dyn Future + Send>>;
    // its older `async_closure!` macro produced exactly that shape (and is now
    // deprecated). Native async-closure syntax returns `impl Future`, which
    // doesn't satisfy the trait bound, so we spell the Box::pin out instead.
    event_listener.add_window_title_changed_handler(|title_data| {
        Box::pin(async move {
            if let Err(e) = handle_title_change(title_data).await {
                error!("Failed to handle title change: {}", e);
            }
        })
    });

    event_listener.add_active_window_changed_handler(|window_data| {
        Box::pin(async move {
            if let Err(e) = handle_active_window_change(window_data).await {
                error!("Failed to handle active window change: {}", e);
            }
        })
    });

    info!("Starting title event listener");
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

    // Long input gets cropped with an ellipsis in the middle. Concretely for
    // max_length=10 the algorithm yields chars_left=4, chars_right=6, so we
    // keep the first 4 and last 6 chars of the input and join with "…".
    #[test]
    fn format_title_long_cropped_with_ellipsis() {
        let input = "1234567890ABCDEF".to_string();
        let out = format_title_string(input, 10);
        assert_eq!(out, "1234…ABCDEF");
        assert!(out.contains('…'));
        // Output is chars_left + 1 (…) + chars_right chars.
        assert_eq!(out.chars().count(), 11);
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
        // 4 emoji + … + 6 emoji = 11 chars
        assert_eq!(out.chars().count(), 11);
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

pub async fn setup_workspace_event_listener() -> Result<()> {
    debug!("Setting up workspace event listener");

    let workspace_result = hyprland::data::Workspace::get_active_async().await;

    match workspace_result {
        Ok(workspace) => {
            let initial_state = format_workspace_name_from_string(&workspace.name, workspace.id);
            let update = WorkspaceUpdate {
                name: initial_state,
                id: workspace.id,
            };
            if let Err(e) = bus::send_workspace_update(update) {
                error!("Failed to send initial workspace update: {}", e);
            }
        }
        Err(e) => {
            error!("Failed to get initial workspace state: {}", e);
            let fallback_update = WorkspaceUpdate {
                name: "Workspace ?".to_string(),
                id: 1, // WorkspaceId is just an i32
            };
            if let Err(e) = bus::send_workspace_update(fallback_update) {
                error!("Failed to send fallback workspace update: {}", e);
            }
        }
    }

    let mut event_listener = AsyncEventListener::new();

    event_listener.add_workspace_changed_handler(|workspace_data| {
        Box::pin(async move {
            if let Err(e) = handle_workspace_change(workspace_data).await {
                error!("Failed to handle workspace change: {}", e);
            }
        })
    });

    info!("Starting workspace event listener");
    event_listener.start_listener_async().await?;

    Ok(())
}
