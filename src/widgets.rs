// Widget layer: builds the GTK4 bar tree and owns the consumer side of every
// subsystem channel. Each setup_*_updates pairs an mpsc::UnboundedReceiver
// drained on the GTK main thread (glib::spawn_future_local) with a tokio::spawn
// that runs the producer. The producer comes from one of the subsystem modules
// (hypr, dbus, pw); this module never knows what's inside the channel, only
// that strings/structs come out and labels go in.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use chrono::Local;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::bus::{BATTERY_SENDER, BLUETOOTH_SENDER, TITLE_SENDER, VolumeUpdate, WORKSPACE_SENDER};
use crate::tray::{TrayAction, TrayCommand, TrayItem, TrayUpdate};
use crate::{dbus, hypr, pw, tray};

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
    let label = gtk4::Label::new(None);  // Start with no text, will be hidden until devices found
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

pub fn create_tray_widget() -> gtk4::Box {
    debug!("Creating system tray widget");
    let tray = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    tray.add_css_class("tray-widget");
    tray.set_visible(false);
    tray
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

pub fn create_right_group() -> (gtk4::Box, gtk4::Box, gtk4::Label, gtk4::Label, gtk4::Label, gtk4::Label) {
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

    let tray_widget = create_tray_widget();
    right_group.append(&tray_widget);

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

    (right_container, tray_widget, bt_widget, volume_widget, battery_widget, time_widget)
}

pub fn create_experimental_bar() -> (gtk4::Box, gtk4::Box, gtk4::Label, gtk4::Label, gtk4::Label, gtk4::Label, gtk4::Label, gtk4::Label) {
    debug!("Creating experimental bar");

    let main_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    main_box.set_hexpand(true);
    main_box.set_valign(gtk4::Align::Center);

    let (left_group, workspace_widget) = create_left_group();
    let (center_spacer_start, title_widget, center_spacer_end) = create_center_group();
    let (right_group, tray_widget, bt_widget, volume_widget, battery_widget, time_widget) = create_right_group();

    main_box.append(&left_group);
    main_box.append(&center_spacer_start);
    main_box.append(&title_widget);
    main_box.append(&center_spacer_end);
    main_box.append(&right_group);

    (main_box, tray_widget, bt_widget, volume_widget, battery_widget, time_widget, workspace_widget, title_widget)
}

fn argb_to_rgba(width: i32, height: i32, argb: &[u8]) -> Option<Vec<u8>> {
    if width <= 0 || height <= 0 {
        warn!(width, height, "Ignoring tray pixmap with invalid dimensions");
        return None;
    }

    let expected_len = match (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    {
        Some(expected_len) => expected_len,
        None => {
            warn!(width, height, "Tray pixmap dimensions overflowed");
            return None;
        }
    };
    if argb.len() != expected_len {
        warn!(width, height, actual = argb.len(), expected_len, "Ignoring malformed tray pixmap");
        return None;
    }

    let mut rgba = Vec::with_capacity(argb.len());
    for pixel in argb.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
    }
    Some(rgba)
}

// A themed icon that has_icon() reports as present can still be impossible to
// draw: when the matching file needs a gdk-pixbuf loader that is not installed
// (e.g. an SVG-only icon with no librsvg loader) GTK resolves it to nothing and
// silently paints "image-missing". Reproduce the decode here so the failure is
// observable in the logs and callers can fall back instead of showing a broken
// glyph. Returns None when the icon is drawable, or Some(reason) when it is not.
fn named_icon_render_error(icon_theme: &gtk4::IconTheme, name: &str, scale: i32) -> Option<String> {
    let paintable = icon_theme.lookup_icon(
        name,
        &[],
        20,
        scale.max(1),
        gtk4::TextDirection::None,
        gtk4::IconLookupFlags::empty(),
    );
    let Some(path) = paintable.file().and_then(|file| file.path()) else {
        return Some("theme entry resolved to no drawable file".to_string());
    };
    match gdk::Texture::from_file(&gtk4::gio::File::for_path(&path)) {
        Ok(_texture) => None,
        Err(error) => Some(format!("{} failed to decode: {error}", path.display())),
    }
}

fn update_tray_image(image: &gtk4::Image, item: &TrayItem) {
    let display = image.display();
    let icon_theme = gtk4::IconTheme::for_display(&display);

    // Some trays (fcitx5, KDE/appindicator apps) ship their icons in a private
    // directory and advertise it through IconThemePath rather than installing into
    // a standard XDG theme. Register that directory so has_icon/set_icon_name can
    // find the named icon; without this those items fall through to image-missing.
    if !item.icon_theme_path.is_empty() {
        let theme_path = Path::new(&item.icon_theme_path);
        let already_searched = icon_theme
            .search_path()
            .iter()
            .any(|existing| existing == theme_path);
        if !already_searched {
            debug!(path = item.icon_theme_path, "Adding tray icon theme path");
            icon_theme.add_search_path(theme_path);
        }
    }

    // An absolute path points straight at an icon file on disk.
    if !item.icon_name.is_empty()
        && Path::new(&item.icon_name).is_absolute()
        && Path::new(&item.icon_name).is_file()
    {
        image.set_from_file(Some(&item.icon_name));
        image.set_pixel_size(20);
        return;
    }
    // A named icon is resolved through the theme. set_icon_name (rather than
    // hand-loading a pixbuf) lets GTK recolor symbolic icons to the widget's CSS
    // color, which is what keeps fctix's input-keyboard-symbolic visible. Verify
    // the icon can actually be drawn first: has_icon() only checks the theme
    // index, so a missing pixbuf loader would otherwise fail silently.
    if !item.icon_name.is_empty() && icon_theme.has_icon(&item.icon_name) {
        match named_icon_render_error(&icon_theme, &item.icon_name, image.scale_factor()) {
            None => {
                image.set_icon_name(Some(&item.icon_name));
                image.set_pixel_size(20);
                return;
            }
            Some(reason) => warn!(
                item = item.key,
                icon = item.icon_name,
                reason,
                "Named tray icon is in the theme but cannot be drawn (missing gdk-pixbuf loader?); falling back"
            ),
        }
    }

    match &item.icon_pixmap {
        Some((width, height, argb)) => match argb_to_rgba(*width, *height, argb) {
            Some(rgba) => {
                let bytes = glib::Bytes::from_owned(rgba);
                let texture = gdk::MemoryTexture::new(
                    *width,
                    *height,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    (*width as usize) * 4,
                );
                image.set_paintable(Some(&texture));
                image.set_pixel_size(20);
            }
            None => image.set_icon_name(Some("image-missing")),
        },
        None => {
            warn!(item = item.key, icon_name = item.icon_name, "Tray item has no usable icon");
            image.set_icon_name(Some("image-missing"));
            image.set_pixel_size(20);
        }
    }
}

fn update_tray_button(button: &gtk4::Button, image: &gtk4::Image, item: &TrayItem) {
    for class in ["active", "passive", "needs-attention"] {
        button.remove_css_class(class);
    }
    let status_class = match item.status.as_str() {
        status if status.eq_ignore_ascii_case("NeedsAttention") => "needs-attention",
        status if status.eq_ignore_ascii_case("Passive") => "passive",
        _ => "active",
    };
    button.add_css_class(status_class);
    button.set_tooltip_text(Some(if item.title.is_empty() {
        &item.key
    } else {
        &item.title
    }));
    update_tray_image(image, item);
}

fn create_tray_button(
    item: &TrayItem,
    commands: &mpsc::UnboundedSender<TrayCommand>,
) -> (gtk4::Button, gtk4::Image) {
    let button = gtk4::Button::new();
    button.add_css_class("tray-item");
    button.set_focusable(false);

    let image = gtk4::Image::new();
    button.set_child(Some(&image));
    update_tray_button(&button, &image, item);

    let gesture = gtk4::GestureClick::new();
    gesture.set_button(0);
    let key = item.key.clone();
    let item_is_menu = item.item_is_menu;
    let commands = commands.clone();
    let button_for_click = button.clone();
    gesture.connect_released(move |gesture, _press_count, x, y| {
        let action = match gesture.current_button() {
            1 if item_is_menu => TrayAction::ContextMenu,
            1 => TrayAction::Activate,
            2 => TrayAction::SecondaryActivate,
            3 => TrayAction::ContextMenu,
            button => {
                debug!(button, item = key, "Ignoring unsupported tray mouse button");
                return;
            }
        };
        let (x, y) = match button_for_click.root() {
            Some(root) => match root.downcast::<gtk4::Window>() {
                Ok(root_window) => {
                    let point = gtk4::graphene::Point::new(x as f32, y as f32);
                    match button_for_click.compute_point(&root_window, &point) {
                        Some(point) => (point.x().round() as i32, point.y().round() as i32),
                        None => {
                            debug!(item = key, "Could not translate tray click to bar coordinates");
                            (x.round() as i32, y.round() as i32)
                        }
                    }
                }
                Err(_root) => {
                    debug!(item = key, "Tray root is not a GTK window");
                    (x.round() as i32, y.round() as i32)
                }
            },
            None => {
                debug!(item = key, "Tray button has no root during click");
                (x.round() as i32, y.round() as i32)
            }
        };
        let command = TrayCommand {
            key: key.clone(),
            action,
            x,
            y,
        };
        if let Err(error) = commands.send(command) {
            warn!(item = key, %error, "Could not send tray click to D-Bus backend");
        }
    });
    button.add_controller(gesture);

    (button, image)
}

pub fn setup_tray_updates(container: gtk4::Box) {
    debug!("Setting up system tray updates");
    let (update_tx, mut update_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        if let Err(error) = tray::run_tray(update_tx, command_rx).await {
            warn!(%error, "System tray backend stopped");
        }
    });

    glib::spawn_future_local(async move {
        let mut items: HashMap<String, (gtk4::Button, gtk4::Image)> = HashMap::new();
        while let Some(update) = update_rx.recv().await {
            match update {
                TrayUpdate::Upsert(item) => match items.get(&item.key) {
                    Some((button, image)) => update_tray_button(button, image, &item),
                    None => {
                        let (button, image) = create_tray_button(&item, &command_tx);
                        container.append(&button);
                        items.insert(item.key.clone(), (button, image));
                        container.set_visible(true);
                        info!(item = item.key, "Added system tray item");
                    }
                },
                TrayUpdate::Remove(key) => match items.remove(&key) {
                    Some((button, _image)) => {
                        container.remove(&button);
                        container.set_visible(!items.is_empty());
                        info!(item = key, "Removed system tray item");
                    }
                    None => debug!(item = key, "Ignoring removal of unknown tray item"),
                },
            }
        }
        debug!("System tray UI update channel closed");
    });
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

fn update_title_widget_workspace_color(title_widget: &gtk4::Label, workspace_id: hyprland::shared::WorkspaceId) {
    // Get workspace color based on ID
    let color = get_workspace_color(workspace_id);

    // Apply color directly via CSS provider for immediate update
    let css_provider = gtk4::CssProvider::new();
    let css = format!(
        ".title-widget {{ background-color: {}; }}",
        color
    );

    css_provider.load_from_data(&css);

    let style_context = title_widget.style_context();
    style_context.add_provider(&css_provider, gtk4::STYLE_PROVIDER_PRIORITY_USER + 1);

    debug!("Updated title widget color to: {} for workspace: {}", color, workspace_id);
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

    #[test]
    fn tray_pixmap_argb_is_converted_to_rgba() {
        assert_eq!(
            argb_to_rgba(2, 1, &[255, 1, 2, 3, 128, 4, 5, 6]),
            Some(vec![1, 2, 3, 255, 4, 5, 6, 128])
        );
    }

    #[test]
    fn tray_pixmap_rejects_invalid_dimensions_and_length() {
        assert_eq!(argb_to_rgba(0, 1, &[]), None);
        assert_eq!(argb_to_rgba(1, 1, &[0, 1, 2]), None);
    }
}

pub fn setup_workspace_updates(label: gtk4::Label, title_widget: gtk4::Label) -> Result<()> {
    debug!("Setting up workspace updates");

    // Set up combined workspace updates
    let (tx, mut rx) = mpsc::unbounded_channel();
    if WORKSPACE_SENDER.set(tx).is_err() {
        return Err(anyhow::anyhow!("Failed to set global workspace sender"));
    }

    tokio::spawn(hypr::run_workspace_listener_supervised());

    // Handle combined workspace updates (name + ID) in single frame
    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating workspace - label: '{}', color for workspace: {}", update.name, update.id);
            // Update both workspace text and title color atomically
            label.set_text(&update.name);
            update_title_widget_workspace_color(&title_widget, update.id);
        }
    });

    Ok(())
}

pub fn setup_title_updates(label: gtk4::Label) -> Result<()> {
    debug!("Setting up title updates");

    let (tx, mut rx) = mpsc::unbounded_channel();

    if TITLE_SENDER.set(tx).is_err() {
        return Err(anyhow::anyhow!("Failed to set global title sender"));
    }

    tokio::spawn(hypr::run_title_listener_supervised());

    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating title label: {}", update);
            // NOTE: Title widget always remains visible even when empty, unlike battery/bluetooth widgets.
            // This provides consistent visual layout and shows the centered position in the bar.
            label.set_text(&update);
        }
    });

    Ok(())
}

pub fn setup_battery_updates(label: gtk4::Label) -> Result<()> {
    debug!("Setting up battery updates");

    let (tx, mut rx) = mpsc::unbounded_channel();

    if BATTERY_SENDER.set(tx).is_err() {
        return Err(anyhow::anyhow!("Failed to set global battery sender"));
    }

    tokio::spawn(dbus::run_dbus_monitor_supervised());

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

    Ok(())
}

pub fn setup_bluetooth_updates(label: gtk4::Label) -> Result<()> {
    debug!("Setting up Bluetooth battery updates");

    let (tx, mut rx) = mpsc::unbounded_channel();

    if BLUETOOTH_SENDER.set(tx).is_err() {
        return Err(anyhow::anyhow!("Failed to set global Bluetooth sender"));
    }

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

    Ok(())
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
                let emoji = if update.is_muted == Some(true) { "🔇" } else { "🔊" };
                let display_text = format!("{}{}{}",
                    emoji,
                    first_char,
                    volume_percent
                );
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
