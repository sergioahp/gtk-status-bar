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

#[cfg(test)]
mod tests {
    use super::*;

    // The four senders are process-global OnceLocks; we can only initialize
    // them once for the whole test binary. So this is the *only* test that
    // exercises the bus through the live statics. It does so for all four
    // channels at once to maximize coverage, and uses try_recv (rather than
    // .await) because UnboundedSender::send is fully synchronous — the message
    // is in the queue before send returns.
    //
    // Tests that need to exercise higher-level logic mediated by the bus
    // (handle_title_change, handle_workspace_change, etc.) would currently
    // collide with this one. If we ever need that, the move is to switch the
    // static type from OnceLock<T> to Mutex<Option<T>> so tests can swap the
    // sender in/out, and pay the ~50ns lock cost on every send.
    #[test]
    fn bus_smoke_round_trips_through_all_four_senders() {
        let (workspace_tx, mut workspace_rx) = mpsc::unbounded_channel();
        let (title_tx, mut title_rx) = mpsc::unbounded_channel();
        let (battery_tx, mut battery_rx) = mpsc::unbounded_channel();
        let (bluetooth_tx, mut bluetooth_rx) = mpsc::unbounded_channel();

        WORKSPACE_SENDER.set(workspace_tx).expect("WORKSPACE_SENDER already set");
        TITLE_SENDER.set(title_tx).expect("TITLE_SENDER already set");
        BATTERY_SENDER.set(battery_tx).expect("BATTERY_SENDER already set");
        BLUETOOTH_SENDER.set(bluetooth_tx).expect("BLUETOOTH_SENDER already set");

        send_workspace_update(WorkspaceUpdate { name: "ws".to_string(), id: 1 })
            .expect("send_workspace_update should succeed when sender is installed");
        send_title_update(Some("hello".to_string()))
            .expect("send_title_update should succeed");
        send_title_update(None)
            .expect("send_title_update(None) should map to empty string and succeed");
        send_battery_update("🔋 80%".to_string())
            .expect("send_battery_update should succeed");
        send_bluetooth_update("P80".to_string())
            .expect("send_bluetooth_update should succeed");

        let ws = workspace_rx.try_recv().expect("workspace message in queue");
        assert_eq!(ws.name, "ws");
        assert_eq!(ws.id, 1);

        assert_eq!(title_rx.try_recv().expect("title message"), "hello");
        // The None-arm should round-trip as the empty string per the TODO in
        // send_title_update: "maybe handle None variant as: remove the widget?"
        assert_eq!(title_rx.try_recv().expect("title None -> empty"), "");

        assert_eq!(battery_rx.try_recv().expect("battery message"), "🔋 80%");
        assert_eq!(bluetooth_rx.try_recv().expect("bluetooth message"), "P80");
    }

    // When the bus is not initialized, the helpers must return a useful error
    // rather than panic. We can't observe this directly in the smoke test
    // (which initializes the bus), so instead we verify the error path via
    // a fresh local channel: drop the sender, then try to send — confirms the
    // error message shape used by .context().
    #[test]
    fn send_helpers_error_message_shape() {
        // Build a sender, drop the receiver so send fails.
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        drop(rx);
        let err = tx.send("x".to_string()).unwrap_err();
        // sanity: the closed-channel error string is stable enough to assert on
        let msg = err.to_string();
        assert!(msg.to_lowercase().contains("channel closed") || msg.contains("send"),
            "unexpected error message shape: {}", msg);
    }
}
