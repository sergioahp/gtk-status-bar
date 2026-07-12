// Widget layer: builds the GTK4 bar tree and owns the consumer side of every
// subsystem channel. Each setup_*_updates takes a receiver from Bus::new and
// drains it onto its label on the GTK main thread (glib::spawn_future_local).
// The producers are spawned separately by activate() with Bus clones, AFTER
// all consumers here are wired — so a producer's first send can never race an
// unwired channel. This module never knows what's inside the channel, only
// that strings/structs come out and labels go in. The volume path is the
// exception: pw's producer is a std::thread, so setup_volume_updates still
// owns both the channel and the thread spawn.

use anyhow::Result;
use chrono::Local;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::bus::{VolumeUpdate, WorkspaceUpdate};
use crate::pw;

// Widget constructors are infallible — gtk4::Label::new, add_css_class, and
// set_halign all return (). The previous Result<…> signatures were speculative,
// forcing every caller to `?`-thread an error that could not be produced.
pub fn create_workspace_widget() -> gtk4::Label {
    debug!("Creating workspace widget");
    let label = gtk4::Label::new(Some("Workspace ?"));
    label.add_css_class("workspace-widget");
    label.set_halign(gtk4::Align::Center);
    label
}

pub fn create_volume_widget() -> gtk4::Label {
    debug!("Creating volume widget");
    let label = gtk4::Label::new(Some("Volume ?"));
    label.add_css_class("volume-widget");
    label.set_halign(gtk4::Align::Center);
    label
}

pub fn create_title_widget() -> gtk4::Label {
    debug!("Creating title widget");
    let label = gtk4::Label::new(Some("Application Title"));
    label.add_css_class("title-widget");
    label.set_halign(gtk4::Align::End);
    label
}

pub fn create_time_widget() -> gtk4::Label {
    debug!("Creating time widget");
    let time_str = get_current_time();
    let label = gtk4::Label::new(Some(&time_str));
    label.add_css_class("time-widget");
    label.set_halign(gtk4::Align::End);
    label
}

pub fn get_current_time() -> String {
    Local::now().format("%l:%M %p").to_string()
}

pub fn update_time_widget(label: gtk4::Label) {
    debug!("Setting up time widget updates");

    let label_weak = label.downgrade();
    glib::timeout_add_seconds_local(1, move || {
        let Some(label) = label_weak.upgrade() else {
            debug!("Time widget label dropped, stopping updates");
            return glib::ControlFlow::Break;
        };

        label.set_text(&get_current_time());
        glib::ControlFlow::Continue
    });
}

pub fn create_bt_widget() -> gtk4::Label {
    debug!("Creating bluetooth widget");
    let label = gtk4::Label::new(None); // Start with no text, will be hidden until devices found
    label.add_css_class("bt-widget");
    label.set_halign(gtk4::Align::End);
    label
}

pub fn create_battery_widget() -> gtk4::Label {
    debug!("Creating battery widget");
    let label = gtk4::Label::new(Some("🔋 ??%"));
    label.add_css_class("battery-widget");
    label.set_halign(gtk4::Align::End);
    label
}

pub fn create_left_group() -> (gtk4::Box, gtk4::Label) {
    debug!("Creating left group");

    let left_container = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    left_container.add_css_class("left-container");
    left_container.set_valign(gtk4::Align::Start);
    left_container.set_hexpand(false);

    let left_group = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    left_group.add_css_class("left-group");
    left_group.set_hexpand(false);

    let workspace_widget = create_workspace_widget();
    left_group.append(&workspace_widget);

    let left_spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    left_spacer.set_hexpand(true);

    left_container.append(&left_group);
    left_container.append(&left_spacer);

    (left_container, workspace_widget)
}

pub fn create_center_group() -> (gtk4::Box, gtk4::Label, gtk4::Box) {
    debug!("Creating center group");

    let center_spacer_start = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    center_spacer_start.set_hexpand(true);

    let center_group = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    center_group.add_css_class("center-group");
    center_group.set_valign(gtk4::Align::Center);
    center_group.set_hexpand(false);

    let title_widget = create_title_widget();
    center_group.append(&title_widget);

    let center_spacer_end = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    center_spacer_end.set_hexpand(true);

    (center_spacer_start, title_widget, center_spacer_end)
}

pub fn create_right_group() -> (
    gtk4::Box,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
) {
    debug!("Creating right group");

    let right_container = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    right_container.add_css_class("right-container");
    right_container.set_hexpand(false);
    right_container.set_valign(gtk4::Align::End);

    let right_spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    right_spacer.set_hexpand(true);

    let right_group = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    right_group.add_css_class("right-group");
    right_group.set_hexpand(false);

    let bt_widget = create_bt_widget();
    right_group.append(&bt_widget);

    let volume_widget = create_volume_widget();
    right_group.append(&volume_widget);

    let battery_widget = create_battery_widget();
    right_group.append(&battery_widget);

    let time_widget = create_time_widget();
    right_group.append(&time_widget);

    right_container.append(&right_spacer);
    right_container.append(&right_group);

    (
        right_container,
        bt_widget,
        volume_widget,
        battery_widget,
        time_widget,
    )
}

pub fn create_experimental_bar() -> (
    gtk4::Box,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
) {
    debug!("Creating experimental bar");

    let main_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    main_box.set_hexpand(true);
    main_box.set_valign(gtk4::Align::Center);

    let (left_group, workspace_widget) = create_left_group();
    let (center_spacer_start, title_widget, center_spacer_end) = create_center_group();
    let (right_group, bt_widget, volume_widget, battery_widget, time_widget) = create_right_group();

    main_box.append(&left_group);
    main_box.append(&center_spacer_start);
    main_box.append(&title_widget);
    main_box.append(&center_spacer_end);
    main_box.append(&right_group);

    (
        main_box,
        bt_widget,
        volume_widget,
        battery_widget,
        time_widget,
        workspace_widget,
        title_widget,
    )
}

pub fn load_css_styles(window: &gtk4::ApplicationWindow) {
    debug!("Loading CSS styles");

    let css_provider = gtk4::CssProvider::new();
    let css_data = include_str!("../style.css");
    css_provider.load_from_data(css_data);

    gtk4::style_context_add_provider_for_display(
        &gtk4::prelude::WidgetExt::display(window),
        &css_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER,
    );

    info!("CSS styles loaded successfully");
}

pub fn configure_layer_shell(window: &gtk4::ApplicationWindow) {
    debug!("Configuring layer shell");

    window.init_layer_shell();
    window.set_layer(Layer::Bottom);
    window.auto_exclusive_zone_enable();

    let anchors = [
        (Edge::Left, true),
        (Edge::Right, true),
        (Edge::Top, true),
        (Edge::Bottom, false),
    ];

    for (anchor, state) in anchors {
        window.set_anchor(anchor, state);
    }

    window.set_default_height(30);

    info!("Layer shell configured successfully");
}

fn update_title_widget_workspace_color(
    title_widget: &gtk4::Label,
    workspace_id: hyprland::shared::WorkspaceId,
) {
    // Get workspace color based on ID
    let color = get_workspace_color(workspace_id);

    // Apply color directly via CSS provider for immediate update
    let css_provider = gtk4::CssProvider::new();
    let css = format!(".title-widget {{ background-color: {}; }}", color);

    css_provider.load_from_data(&css);

    let style_context = title_widget.style_context();
    style_context.add_provider(&css_provider, gtk4::STYLE_PROVIDER_PRIORITY_USER + 1);

    debug!(
        "Updated title widget color to: {} for workspace: {}",
        color, workspace_id
    );
}

fn get_workspace_color(workspace_id: hyprland::shared::WorkspaceId) -> &'static str {
    match workspace_id {
        1 => "rgba(122, 162, 247, 0.5)",
        2 => "rgba(125, 207, 255, 0.5)",
        3 => "rgba(158, 206, 106, 0.5)",
        4 => "rgba(187, 154, 247, 0.5)",
        5 => "rgba(247, 118, 142, 0.5)",
        6 => "rgba(255, 158, 102, 0.5)",
        7 => "rgba(157, 124, 216, 0.5)",
        8 => "rgba(224, 175, 104, 0.5)",
        9 => "rgba(42, 195, 222, 0.5)",
        10 => "rgba(13, 185, 215, 0.5)",
        _ => "rgba(67, 233, 123, 0.5)", // Default color
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Workspaces 1..=10 have explicit color entries; everything else hits the
    // default arm. Tests pin the boundaries — a typo in the match arms
    // (e.g. duplicate id, wrong default fallthrough) would flip these.
    #[test]
    fn workspace_color_1_is_blue_ish() {
        assert_eq!(get_workspace_color(1), "rgba(122, 162, 247, 0.5)");
    }

    #[test]
    fn workspace_color_10_is_last_explicit() {
        assert_eq!(get_workspace_color(10), "rgba(13, 185, 215, 0.5)");
    }

    #[test]
    fn workspace_color_11_falls_through_to_default() {
        let default = "rgba(67, 233, 123, 0.5)";
        assert_eq!(get_workspace_color(11), default);
        assert_eq!(get_workspace_color(100), default);
    }

    // Hyprland uses negative workspace IDs for special workspaces; verify we
    // don't accidentally match a positive arm and that we hit the default.
    #[test]
    fn workspace_color_negative_id_falls_through_to_default() {
        let default = "rgba(67, 233, 123, 0.5)";
        assert_eq!(get_workspace_color(-1), default);
        assert_eq!(get_workspace_color(-99), default);
    }

    // Every explicit arm returns a different color — if a regression turns
    // two of them into the same rgba, this catches it.
    #[test]
    fn workspace_colors_are_all_distinct() {
        let mut colors: Vec<&str> = (1..=10).map(get_workspace_color).collect();
        colors.push(get_workspace_color(0)); // default
        colors.sort();
        let len_before = colors.len();
        colors.dedup();
        assert_eq!(colors.len(), len_before, "expected all distinct colors");
    }
}

// setup_*_updates are infallible now that there is no global sender to
// double-initialize — they only move a receiver into a glib-local drain task.

pub fn setup_workspace_updates(
    mut rx: mpsc::UnboundedReceiver<WorkspaceUpdate>,
    label: gtk4::Label,
    title_widget: gtk4::Label,
) {
    debug!("Setting up workspace updates");

    // Handle combined workspace updates (name + ID) in single frame
    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!(
                "Updating workspace - label: '{}', color for workspace: {}",
                update.name, update.id
            );
            // Update both workspace text and title color atomically
            label.set_text(&update.name);
            update_title_widget_workspace_color(&title_widget, update.id);
        }
    });
}

pub fn setup_title_updates(mut rx: mpsc::UnboundedReceiver<String>, label: gtk4::Label) {
    debug!("Setting up title updates");

    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating title label: {}", update);
            // NOTE: Title widget always remains visible even when empty, unlike battery/bluetooth widgets.
            // This provides consistent visual layout and shows the centered position in the bar.
            label.set_text(&update);
        }
    });
}

pub fn setup_battery_updates(mut rx: mpsc::UnboundedReceiver<String>, label: gtk4::Label) {
    debug!("Setting up battery updates");

    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating battery label: {}", update);

            // Hide widget if no battery data, show if there is data
            // NOTE: Originally tried CSS approach with label.add_css_class("widget-hidden")
            // and .widget-hidden { display: none !important; } but GTK4 CSS specificity
            // issues prevented it from working. GTK's native set_visible() works reliably.
            if update.trim().is_empty() {
                label.set_visible(false);
                debug!("🙈 HIDING battery widget with set_visible(false)");
            } else {
                label.set_visible(true);
                label.set_text(&update);
                debug!("👁️  SHOWING battery widget - data: {}", update);
            }
        }
    });
}

pub fn setup_bluetooth_updates(mut rx: mpsc::UnboundedReceiver<String>, label: gtk4::Label) {
    debug!("Setting up Bluetooth battery updates");

    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating Bluetooth battery label: {}", update);

            // Hide widget if no Bluetooth devices, show if there are devices
            // NOTE: Using GTK's native set_visible() since CSS approach didn't work reliably
            if update.trim().is_empty() {
                label.set_visible(false);
                debug!("🙈 HIDING Bluetooth widget - no devices");
            } else {
                label.set_visible(true);
                label.set_text(&update);
                debug!("👁️  SHOWING Bluetooth widget - data: {}", update);
            }
        }
    });
}

pub fn setup_volume_updates(label: gtk4::Label) -> Result<()> {
    debug!("Setting up volume updates with tokio async channels");

    let (sender, mut receiver) = mpsc::unbounded_channel::<VolumeUpdate>();

    // Start PipeWire monitoring on dedicated thread
    pw::start_pipewire_thread(sender)?;

    // Spawn async task on GTK main thread to handle volume updates
    glib::spawn_future_local(async move {
        debug!("🚀 Starting async volume update loop...");

        while let Some(update) = receiver.recv().await {
            // Use channel volume first (more accurate), fallback to main volume
            if let Some(volume_percent) = update.channel_percent.or(update.volume_percent) {
                let first_char = update.name.chars().next().unwrap_or('A');
                let emoji = if update.is_muted == Some(true) {
                    "🔇"
                } else {
                    "🔊"
                };
                let display_text = format!("{}{}{}", emoji, first_char, volume_percent);
                label.set_text(&display_text);
                debug!("📺 GTK UI updated via ASYNC: {}", display_text);
            } else {
                debug!("📺 Skipping GUI update - no volume data available");
            }
        }

        debug!("⚠️ Volume update loop ended");
    });

    Ok(())
}
