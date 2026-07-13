// The status bar is wired as producer-consumer fan-outs: each subsystem
// (Hyprland clients, Hyprland workspace, UPower battery, BlueZ) pushes labels
// into an unbounded mpsc channel and a glib-local task drains it onto the
// corresponding GTK widget on the main thread. This module owns the Bus (the
// five senders, cloned into each producer at spawn time) and the typed send
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceClient {
    pub address: hyprland::shared::Address,
    pub title: String,
    pub compact_title: String,
    pub class: String,
    pub active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceClientsUpdate {
    pub workspace_id: hyprland::shared::WorkspaceId,
    pub clients: Vec<WorkspaceClient>,
}

#[derive(Debug, Clone)]
pub struct VolumeUpdate {
    pub name: String,
    pub volume_percent: Option<u8>,  // Main volume 0-100%
    pub channel_percent: Option<u8>, // First channel volume 0-100% (most accurate for user changes)
    pub is_muted: Option<bool>,
}

// Producer-side handle: cheap to clone (five UnboundedSender clones), Send +
// Sync, so it moves freely into tokio tasks and hyprland-rs handler closures.
#[derive(Clone)]
pub struct Bus {
    workspace: mpsc::UnboundedSender<WorkspaceUpdate>,
    clients: mpsc::UnboundedSender<WorkspaceClientsUpdate>,
    battery: mpsc::UnboundedSender<String>,
    bluetooth: mpsc::UnboundedSender<String>,
    network: mpsc::UnboundedSender<String>,
}

// Consumer side, produced exactly once per Bus by Bus::new. Receivers are not
// cloneable; each field is moved into its widget's glib-local drain task.
pub struct BusReceivers {
    pub workspace: mpsc::UnboundedReceiver<WorkspaceUpdate>,
    pub clients: mpsc::UnboundedReceiver<WorkspaceClientsUpdate>,
    pub battery: mpsc::UnboundedReceiver<String>,
    pub bluetooth: mpsc::UnboundedReceiver<String>,
    pub network: mpsc::UnboundedReceiver<String>,
}

impl Bus {
    pub fn new() -> (Bus, BusReceivers) {
        let (workspace_tx, workspace_rx) = mpsc::unbounded_channel();
        let (clients_tx, clients_rx) = mpsc::unbounded_channel();
        let (battery_tx, battery_rx) = mpsc::unbounded_channel();
        let (bluetooth_tx, bluetooth_rx) = mpsc::unbounded_channel();
        let (network_tx, network_rx) = mpsc::unbounded_channel();

        (
            Bus {
                workspace: workspace_tx,
                clients: clients_tx,
                battery: battery_tx,
                bluetooth: bluetooth_tx,
                network: network_tx,
            },
            BusReceivers {
                workspace: workspace_rx,
                clients: clients_rx,
                battery: battery_rx,
                bluetooth: bluetooth_rx,
                network: network_rx,
            },
        )
    }

    // These helpers are intentionally synchronous. UnboundedSender::send is
    // non-blocking and returns Result, not a Future; declaring them `async fn`
    // would force every caller into an async context and mis-signal a yield
    // point that does not exist.

    pub fn send_workspace_update(&self, update: WorkspaceUpdate) -> Result<()> {
        self.workspace
            .send(update)
            .context("Failed to send workspace update")
    }

    pub fn send_clients_update(&self, update: WorkspaceClientsUpdate) -> Result<()> {
        self.clients
            .send(update)
            .context("Failed to send workspace clients update")
    }

    pub fn send_battery_update(&self, update: String) -> Result<()> {
        self.battery
            .send(update)
            .context("Failed to send battery update")
    }

    pub fn send_bluetooth_update(&self, update: String) -> Result<()> {
        self.bluetooth
            .send(update)
            .context("Failed to send bluetooth update")
    }

    pub fn send_network_update(&self, update: String) -> Result<()> {
        self.network
            .send(update)
            .context("Failed to send network update")
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
        bus.send_workspace_update(WorkspaceUpdate {
            name: "ws".to_string(),
            id: 1,
        })
        .expect("send_workspace_update should succeed");
        let ws = rx.workspace.try_recv().expect("workspace message in queue");
        assert_eq!(ws.name, "ws");
        assert_eq!(ws.id, 1);
    }

    #[test]
    fn workspace_clients_update_round_trips() {
        let (bus, mut rx) = Bus::new();
        let update = WorkspaceClientsUpdate {
            workspace_id: 2,
            clients: vec![WorkspaceClient {
                address: hyprland::shared::Address::new("1234"),
                title: "hello world".to_string(),
                compact_title: "hello".to_string(),
                class: "kitty".to_string(),
                active: true,
            }],
        };
        bus.send_clients_update(update.clone())
            .expect("send_clients_update should succeed");
        assert_eq!(
            rx.clients.try_recv().expect("workspace clients message"),
            update
        );
    }

    #[test]
    fn status_updates_round_trip() {
        let (bus, mut rx) = Bus::new();
        bus.send_battery_update("🔋 80%".to_string())
            .expect("send_battery_update should succeed");
        bus.send_bluetooth_update("P80".to_string())
            .expect("send_bluetooth_update should succeed");
        bus.send_network_update("🌐 ✓".to_string())
            .expect("send_network_update should succeed");
        assert_eq!(rx.battery.try_recv().expect("battery message"), "🔋 80%");
        assert_eq!(rx.bluetooth.try_recv().expect("bluetooth message"), "P80");
        assert_eq!(rx.network.try_recv().expect("network message"), "🌐 ✓");
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
        let err = bus
            .send_clients_update(WorkspaceClientsUpdate::default())
            .expect_err("send into closed channel must fail");
        let chain = format!("{:#}", err);
        assert!(
            chain.contains("Failed to send workspace clients update"),
            "outer context missing: {}",
            chain
        );
        assert!(
            chain.to_lowercase().contains("channel closed"),
            "root cause missing: {}",
            chain
        );
    }
}
