// The status bar is wired as four producer-consumer fan-outs: each subsystem
// (Hyprland title, Hyprland workspace, UPower battery, BlueZ) pushes labels
// into an unbounded mpsc channel and a glib-local task drains it onto the
// corresponding GTK widget on the main thread. This module owns the Bus (the
// four senders, cloned into each producer at spawn time) and the typed send
// helpers; the widget layer (setup_*_updates) owns the receivers. The
// PipeWire volume channel stays outside the Bus: its producer is a dedicated
// std::thread that already takes its sender as a parameter (see
// pw::start_pipewire_thread).
//
// Unbounded is intentional: producers are IPC listeners reading sockets and
// must never block on backpressure or the kernel buffer fills, the connection
// drops, and the listener dies. See branch experiment-title-sender-bounded
// for the autopsy.
//
// The senders used to live in process-wide OnceLock statics. That made
// wiring order a runtime property (the D-Bus monitor could race the
// bluetooth sender's initialization and drop its first update) and capped
// the suite at one bus-mediated test per process (OnceLock sets once, ever).
// Passing a Bus handle instead makes "consumers wired before producers
// spawn" a property of the call graph in activate(), and lets every test
// build its own private Bus.

use anyhow::{Context, Result};
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

// Producer-side handle: cheap to clone (four UnboundedSender clones), Send +
// Sync, so it moves freely into tokio tasks and hyprland-rs handler closures.
#[derive(Clone)]
pub struct Bus {
    workspace: mpsc::UnboundedSender<WorkspaceUpdate>,
    title: mpsc::UnboundedSender<String>,
    battery: mpsc::UnboundedSender<String>,
    bluetooth: mpsc::UnboundedSender<String>,
}

// Consumer side, produced exactly once per Bus by Bus::new. Receivers are not
// cloneable; each field is moved into its widget's glib-local drain task.
pub struct BusReceivers {
    pub workspace: mpsc::UnboundedReceiver<WorkspaceUpdate>,
    pub title: mpsc::UnboundedReceiver<String>,
    pub battery: mpsc::UnboundedReceiver<String>,
    pub bluetooth: mpsc::UnboundedReceiver<String>,
}

impl Bus {
    pub fn new() -> (Bus, BusReceivers) {
        let (workspace_tx, workspace_rx) = mpsc::unbounded_channel();
        let (title_tx, title_rx) = mpsc::unbounded_channel();
        let (battery_tx, battery_rx) = mpsc::unbounded_channel();
        let (bluetooth_tx, bluetooth_rx) = mpsc::unbounded_channel();

        (
            Bus {
                workspace: workspace_tx,
                title: title_tx,
                battery: battery_tx,
                bluetooth: bluetooth_tx,
            },
            BusReceivers {
                workspace: workspace_rx,
                title: title_rx,
                battery: battery_rx,
                bluetooth: bluetooth_rx,
            },
        )
    }

    // These helpers are intentionally synchronous. UnboundedSender::send is
    // non-blocking and returns Result, not a Future; declaring them `async fn`
    // would force every caller into an async context and mis-signal a yield
    // point that does not exist.

    pub fn send_workspace_update(&self, update: WorkspaceUpdate) -> Result<()> {
        self.workspace.send(update)
            .context("Failed to send workspace update")
    }

    pub fn send_title_update(&self, update: Option<String>) -> Result<()> {
        // TODO: maybe handle None variant as: remove the widget? maybe pass as optional and handle
        // that None case elsewere
        self.title.send(update.unwrap_or_default())
            .context("Failed to send title update")
    }

    pub fn send_battery_update(&self, update: String) -> Result<()> {
        self.battery.send(update)
            .context("Failed to send battery update")
    }

    pub fn send_bluetooth_update(&self, update: String) -> Result<()> {
        self.bluetooth.send(update)
            .context("Failed to send bluetooth update")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every test builds its own Bus, so unlike the old OnceLock statics there
    // is no per-process limit on bus-mediated tests. try_recv (rather than
    // .await) is enough because UnboundedSender::send is fully synchronous —
    // the message is in the queue before send returns.

    #[test]
    fn workspace_update_round_trips() {
        let (bus, mut rx) = Bus::new();
        bus.send_workspace_update(WorkspaceUpdate { name: "ws".to_string(), id: 1 })
            .expect("send_workspace_update should succeed");
        let ws = rx.workspace.try_recv().expect("workspace message in queue");
        assert_eq!(ws.name, "ws");
        assert_eq!(ws.id, 1);
    }

    #[test]
    fn title_update_round_trips_and_none_maps_to_empty() {
        let (bus, mut rx) = Bus::new();
        bus.send_title_update(Some("hello".to_string()))
            .expect("send_title_update should succeed");
        // The None-arm should round-trip as the empty string per the TODO in
        // send_title_update: "maybe handle None variant as: remove the widget?"
        bus.send_title_update(None)
            .expect("send_title_update(None) should map to empty string and succeed");
        assert_eq!(rx.title.try_recv().expect("title message"), "hello");
        assert_eq!(rx.title.try_recv().expect("title None -> empty"), "");
    }

    #[test]
    fn battery_and_bluetooth_updates_round_trip() {
        let (bus, mut rx) = Bus::new();
        bus.send_battery_update("🔋 80%".to_string())
            .expect("send_battery_update should succeed");
        bus.send_bluetooth_update("P80".to_string())
            .expect("send_bluetooth_update should succeed");
        assert_eq!(rx.battery.try_recv().expect("battery message"), "🔋 80%");
        assert_eq!(rx.bluetooth.try_recv().expect("bluetooth message"), "P80");
    }

    // With the receivers dropped, sends must fail with the layered context
    // (helper's message wrapping tokio's closed-channel error) rather than
    // panic. Widgets never drop their receivers in practice, but the
    // supervisors keep producers alive across GTK teardown so the error path
    // is reachable during shutdown.
    #[test]
    fn send_into_closed_channel_reports_layered_context() {
        let (bus, rx) = Bus::new();
        drop(rx);
        let err = bus.send_title_update(Some("x".to_string()))
            .expect_err("send into closed channel must fail");
        let chain = format!("{:#}", err);
        assert!(chain.contains("Failed to send title update"), "outer context missing: {}", chain);
        assert!(chain.to_lowercase().contains("channel closed"), "root cause missing: {}", chain);
    }
}
