// Hyprland subsystem: title + workspace listeners.
//
// We connect to Hyprland's IPC event socket (.socket2.sock) via hyprland-rs's
// AsyncEventListener. The listener is spawned as a tokio task from the widget
// layer; if it errors out (EOF on the socket, parse failure on an unknown
// event variant, etc.), the spawned task currently logs and exits — see
// run_*_listener for the supervised wrappers that retry with backoff.

use anyhow::Result;
use hyprland::shared::{HyprDataActive, HyprDataActiveOptional};
use hyprland::event_listener::AsyncEventListener;
use hyprland::prelude::async_closure;
use tracing::{debug, error, info};

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
