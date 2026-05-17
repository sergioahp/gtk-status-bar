// The status bar is wired as four producer-consumer fan-outs: each subsystem
// (Hyprland title, Hyprland workspace, UPower battery, BlueZ) pushes labels
// into a process-wide unbounded mpsc channel and a glib-local task drains it
// onto the corresponding GTK widget on the main thread. This module owns the
// statics and the typed send helpers; subsystems import the helpers and the
// widget layer (setup_*_updates) owns the receivers + .set()s the senders.
//
// Unbounded is intentional: producers are IPC listeners reading sockets and
// must never block on backpressure or the kernel buffer fills, the connection
// drops, and the listener dies. See branch experiment-title-sender-bounded
// for the autopsy.

use anyhow::{Context, Result};
use std::sync::OnceLock;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct WorkspaceUpdate {
    pub name: String,
    pub id: hyprland::shared::WorkspaceId,
}

#[derive(Debug, Clone)]
pub struct VolumeUpdate {
    pub id: u32,
    pub name: String,
    pub volume_percent: Option<u8>,  // Main volume 0-100%
    pub channel_percent: Option<u8>, // First channel volume 0-100% (most accurate for user changes)
    pub is_muted: Option<bool>,
}

pub static WORKSPACE_SENDER: OnceLock<mpsc::UnboundedSender<WorkspaceUpdate>> = OnceLock::new();
pub static TITLE_SENDER:     OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();
pub static BATTERY_SENDER:   OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();
pub static BLUETOOTH_SENDER: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();

// These helpers are intentionally synchronous. OnceLock::get and
// UnboundedSender::send are both non-blocking; declaring them `async fn` would
// force every caller into an async context and mis-signal a yield point that
// does not exist.

pub fn send_workspace_update(update: WorkspaceUpdate) -> Result<()> {
    let sender = WORKSPACE_SENDER.get()
        .context("Global workspace sender not initialized")?;

    sender.send(update)
        .context("Failed to send workspace update")?;

    Ok(())
}

pub fn send_title_update(update: Option<String>) -> Result<()> {
    let sender = TITLE_SENDER.get()
        .context("Global title sender not initialized")?;

    // TODO: maybe handle None variant as: remove the widget? maybe pass as optional and handle
    // that None case elsewere
    sender.send(update.unwrap_or_default())
        .context("Failed to send title update")?;

    Ok(())
}

pub fn send_battery_update(update: String) -> Result<()> {
    let sender = BATTERY_SENDER.get()
        .context("Global battery sender not initialized")?;

    sender.send(update)
        .context("Failed to send battery update")?;

    Ok(())
}

pub fn send_bluetooth_update(update: String) -> Result<()> {
    let sender = BLUETOOTH_SENDER.get()
        .context("Global bluetooth sender not initialized")?;

    sender.send(update)
        .context("Failed to send bluetooth update")?;

    Ok(())
}
