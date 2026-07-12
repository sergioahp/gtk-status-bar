// Widget layer: builds the GTK4 bar tree and owns the consumer side of every
// subsystem channel. Each setup_*_updates pairs an mpsc::UnboundedReceiver
// drained on the GTK main thread (glib::spawn_future_local) with a tokio::spawn
// that runs the producer. The producer comes from one of the subsystem modules
// (hypr, dbus, pw); this module never knows what's inside the channel, only
// that strings/structs come out and labels go in.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use anyhow::Result;
use chrono::Local;
use gtk4::gdk;
use gtk4::gio::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::bus::{BATTERY_SENDER, BLUETOOTH_SENDER, TITLE_SENDER, VolumeUpdate, WORKSPACE_SENDER};
use crate::ipc::{IpcRequest, IpcResponse, IpcTrayItem};
use crate::tray::{TrayAction, TrayCommand, TrayItem, TrayMenu, TrayMenuItem, TrayUpdate};
use crate::{dbus, hypr, ipc, pw, tray};

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

const TRAY_ICON_SIZE: i32 = 20;

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
        TRAY_ICON_SIZE,
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
    let icon_path = Path::new(&item.icon_name);
    if !item.icon_name.is_empty() && icon_path.is_absolute() && icon_path.is_file() {
        image.set_from_file(Some(icon_path));
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
            }
            None => image.set_icon_name(Some("image-missing")),
        },
        None => {
            warn!(item = item.key, icon_name = item.icon_name, "Tray item has no usable icon");
            image.set_icon_name(Some("image-missing"));
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

// One live tray icon: the button in the bar, the image inside it, the latest
// item state shared with the click handler (so upserts retarget clicks without
// rebuilding the widget — apps commonly set Menu/ItemIsMenu only after they
// register), and the popovers of the currently open dropdown menu (tracked so
// they can be unparented when the menu closes or the item goes away).
struct TrayEntry {
    button: gtk4::Button,
    image: gtk4::Image,
    state: Rc<RefCell<TrayItem>>,
    open_menu: Rc<RefCell<Option<OpenTrayMenu>>>,
}

struct OpenTrayMenu {
    popovers: Vec<gtk4::Popover>,
    entries: Vec<OpenTrayMenuEntry>,
    selected: Option<usize>,
}

struct OpenTrayMenuEntry {
    button: gtk4::Button,
    id: i32,
    ancestors: Vec<gtk4::Popover>,
}

fn ipc_item(index: usize, item: &TrayItem) -> IpcTrayItem {
    IpcTrayItem {
        index,
        key: item.key.clone(),
        title: item.title.clone(),
        status: item.status.clone(),
    }
}

fn resolve_ipc_target<'a>(
    target: &str,
    order: &'a [String],
    entries: &'a HashMap<String, TrayEntry>,
) -> std::result::Result<(usize, &'a TrayEntry), String> {
    if let Some(entry) = entries.get(target) {
        let index = order
            .iter()
            .position(|key| key == target)
            .ok_or_else(|| format!("tray item {target:?} is missing from the display order"))?;
        return Ok((index, entry));
    }

    let mut matches = order.iter().enumerate().filter_map(|(index, key)| {
        let entry = entries.get(key)?;
        (entry.state.borrow().title == target).then_some((index, entry))
    });
    if let Some(found) = matches.next() {
        if matches.next().is_some() {
            return Err(format!(
                "more than one tray item is titled {target:?}; use its index or key"
            ));
        }
        return Ok(found);
    }

    let index = target
        .parse::<usize>()
        .map_err(|_| format!("no tray item matches {target:?}"))?;
    let key = order
        .get(index)
        .ok_or_else(|| format!("tray item index {index} is out of range"))?;
    let entry = entries
        .get(key)
        .ok_or_else(|| format!("tray item index {index} is no longer available"))?;
    Ok((index, entry))
}

fn tray_button_coordinates(button: &gtk4::Button) -> (i32, i32) {
    let x = f64::from(button.width()) / 2.0;
    let y = f64::from(button.height()) / 2.0;
    let Some(root) = button.root() else {
        return (x.round() as i32, y.round() as i32);
    };
    let Ok(window) = root.downcast::<gtk4::Window>() else {
        return (x.round() as i32, y.round() as i32);
    };
    let point = gtk4::graphene::Point::new(x as f32, y as f32);
    button
        .compute_point(&window, &point)
        .map(|point| (point.x().round() as i32, point.y().round() as i32))
        .unwrap_or((x.round() as i32, y.round() as i32))
}

fn select_menu_entry(entry: &TrayEntry, direction: isize) -> std::result::Result<i32, String> {
    let mut open_menu = entry.open_menu.borrow_mut();
    let menu = open_menu
        .as_mut()
        .ok_or_else(|| "tray item has no open menu".to_string())?;
    if menu.entries.is_empty() {
        return Err("open tray menu has no enabled entries".to_string());
    }
    if let Some(selected) = menu.selected {
        menu.entries[selected].button.remove_css_class("selected");
    }
    let len = menu.entries.len() as isize;
    let selected = match menu.selected {
        Some(selected) => (selected as isize + direction).rem_euclid(len) as usize,
        None if direction < 0 => menu.entries.len() - 1,
        None => 0,
    };
    let selected_entry = &menu.entries[selected];
    for popover in menu.popovers.iter().skip(1) {
        popover.popdown();
    }
    for popover in &selected_entry.ancestors {
        popover.popup();
    }
    selected_entry.button.add_css_class("selected");
    selected_entry.button.grab_focus();
    let id = selected_entry.id;
    menu.selected = Some(selected);
    Ok(id)
}

fn menu_entry_id(entry: &TrayEntry, requested: Option<i32>) -> std::result::Result<i32, String> {
    let open_menu = entry.open_menu.borrow();
    let menu = open_menu
        .as_ref()
        .ok_or_else(|| "tray item has no open menu".to_string())?;
    match requested {
        Some(id) if menu.entries.iter().any(|entry| entry.id == id) => Ok(id),
        Some(id) => Err(format!("open tray menu has no enabled entry {id}")),
        None => menu
            .selected
            .map(|selected| menu.entries[selected].id)
            .ok_or_else(|| "open tray menu has no selected entry".to_string()),
    }
}

fn handle_ipc_request(
    request: IpcRequest,
    order: &[String],
    entries: &HashMap<String, TrayEntry>,
    commands: &mpsc::UnboundedSender<TrayCommand>,
) -> IpcResponse {
    let (target, action) = match &request {
        IpcRequest::List => {
            let items = order
                .iter()
                .enumerate()
                .filter_map(|(index, key)| {
                    entries
                        .get(key)
                        .map(|entry| ipc_item(index, &entry.state.borrow()))
                })
                .collect();
            return IpcResponse::success(items);
        }
        IpcRequest::CloseMenus => {
            for entry in entries.values() {
                close_tray_menu(&entry.open_menu);
            }
            return IpcResponse::success(Vec::new());
        }
        IpcRequest::Activate { target } => (target.clone(), None),
        IpcRequest::SecondaryActivate { target } => {
            (target.clone(), Some(TrayAction::SecondaryActivate))
        }
        IpcRequest::ContextMenu { target } => {
            (target.clone(), Some(TrayAction::ContextMenu))
        }
        IpcRequest::MenuNext { target } => (target.clone(), None),
        IpcRequest::MenuPrevious { target } => (target.clone(), None),
        IpcRequest::MenuActivate { target } => (target.clone(), None),
        IpcRequest::MenuClick { target, entry } => {
            (target.clone(), Some(TrayAction::MenuEvent(*entry)))
        }
    };
    let (index, entry) = match resolve_ipc_target(&target, order, entries) {
        Ok(found) => found,
        Err(error) => return IpcResponse::error(error),
    };
    let item = entry.state.borrow();
    let selected = ipc_item(index, &item);
    let action = match request {
        IpcRequest::Activate { .. } if item.item_is_menu => TrayAction::ContextMenu,
        IpcRequest::Activate { .. } => TrayAction::Activate,
        IpcRequest::MenuNext { .. } => match select_menu_entry(entry, 1) {
            Ok(_id) => return IpcResponse::success(vec![selected]),
            Err(error) => return IpcResponse::error(error),
        },
        IpcRequest::MenuPrevious { .. } => match select_menu_entry(entry, -1) {
            Ok(_id) => return IpcResponse::success(vec![selected]),
            Err(error) => return IpcResponse::error(error),
        },
        IpcRequest::MenuActivate { .. } => match menu_entry_id(entry, None) {
            Ok(id) => TrayAction::MenuEvent(id),
            Err(error) => return IpcResponse::error(error),
        },
        IpcRequest::MenuClick { entry: id, .. } => match menu_entry_id(entry, Some(id)) {
            Ok(id) => TrayAction::MenuEvent(id),
            Err(error) => return IpcResponse::error(error),
        },
        _ => action.expect("non-menu IPC actions have a tray action"),
    };
    let (x, y) = tray_button_coordinates(&entry.button);
    let command = TrayCommand {
        key: item.key.clone(),
        action,
        x,
        y,
        menu_path: item.menu_path.clone(),
    };
    if let Err(error) = commands.send(command) {
        return IpcResponse::error(format!("tray backend is not available: {error}"));
    }
    if matches!(action, TrayAction::MenuEvent(_)) {
        drop(item);
        close_tray_menu(&entry.open_menu);
    }
    IpcResponse::success(vec![selected])
}

fn create_tray_entry(item: TrayItem, commands: &mpsc::UnboundedSender<TrayCommand>) -> TrayEntry {
    let button = gtk4::Button::new();
    button.add_css_class("tray-item");
    button.set_focusable(false);

    let image = gtk4::Image::new();
    image.set_pixel_size(TRAY_ICON_SIZE);
    button.set_child(Some(&image));
    update_tray_button(&button, &image, &item);

    let state = Rc::new(RefCell::new(item));

    let gesture = gtk4::GestureClick::new();
    gesture.set_button(0);
    // Run in the capture phase and claim the sequence so the button's own
    // internal GestureClick does not swallow the release — without this our
    // `released` handler never fires and clicks do nothing.
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let state_for_press = state.clone();
    gesture.connect_pressed(move |gesture_pressed, _press_count, x, y| {
        let item = state_for_press.borrow();
        debug!(
            item = item.key,
            button = gesture_pressed.current_button(),
            x, y,
            "Tray button pressed"
        );
        gesture_pressed.set_state(gtk4::EventSequenceState::Claimed);
    });
    let commands = commands.clone();
    let state_for_release = state.clone();
    // Weak: the gesture lives on the button, so capturing the button strongly
    // here would cycle (button -> gesture -> closure -> button) and leak it
    // once the item is removed from the bar.
    let button_weak = button.downgrade();
    gesture.connect_released(move |gesture, _press_count, x, y| {
        let (key, item_is_menu, menu_path) = {
            let item = state_for_release.borrow();
            (item.key.clone(), item.item_is_menu, item.menu_path.clone())
        };
        debug!(
            item = key,
            button = gesture.current_button(),
            x, y, item_is_menu,
            "Tray button released"
        );
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
        let Some(button) = button_weak.upgrade() else {
            debug!(item = key, "Tray button was dropped before click handling");
            return;
        };
        let (x, y) = match button.root() {
            Some(root) => match root.downcast::<gtk4::Window>() {
                Ok(root_window) => {
                    let point = gtk4::graphene::Point::new(x as f32, y as f32);
                    match button.compute_point(&root_window, &point) {
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
            menu_path,
        };
        if let Err(error) = commands.send(command) {
            warn!(item = key, %error, "Could not send tray click to D-Bus backend");
        }
    });
    button.add_controller(gesture);

    TrayEntry {
        button,
        image,
        state,
        open_menu: Rc::new(RefCell::new(None)),
    }
}

pub fn setup_tray_updates(container: gtk4::Box) {
    debug!("Setting up system tray updates");
    let (update_tx, mut update_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (menu_tx, mut menu_rx) = mpsc::unbounded_channel();
    let (ipc_tx, mut ipc_rx) = mpsc::unbounded_channel();

    tokio::spawn(tray::run_tray_supervised(update_tx, command_rx, menu_tx));
    tokio::spawn(async move {
        if let Err(error) = ipc::run_server(ipc_tx).await {
            warn!(%error, "Tray IPC server stopped");
        }
    });

    glib::spawn_future_local(async move {
        let mut entries: HashMap<String, TrayEntry> = HashMap::new();
        let mut order = Vec::new();
        let mut ipc_available = true;
        loop {
            tokio::select! {
                update = update_rx.recv() => {
                    let Some(update) = update else { break };
                    match update {
                        TrayUpdate::Upsert(item) => match entries.get(&item.key) {
                            Some(entry) => {
                                update_tray_button(&entry.button, &entry.image, &item);
                                *entry.state.borrow_mut() = item;
                            }
                            None => {
                                let key = item.key.clone();
                                let entry = create_tray_entry(item, &command_tx);
                                container.append(&entry.button);
                                entries.insert(key.clone(), entry);
                                order.push(key.clone());
                                container.set_visible(true);
                                info!(item = key, "Added system tray item");
                            }
                        },
                        TrayUpdate::Remove(key) => match entries.remove(&key) {
                            Some(entry) => {
                                // Unparent any open menu first: popovers are only
                                // attached to the button, not laid-out children,
                                // and GTK complains when a widget is finalized
                                // with one still attached.
                                close_tray_menu(&entry.open_menu);
                                container.remove(&entry.button);
                                order.retain(|item_key| item_key != &key);
                                container.set_visible(!entries.is_empty());
                                info!(item = key, "Removed system tray item");
                            }
                            None => debug!(item = key, "Ignoring removal of unknown tray item"),
                        },
                    }
                }
                menu = menu_rx.recv() => {
                    // The supervised backend holds menu_tx for the process
                    // lifetime, so None means teardown, same as the update arm
                    // (a `continue` here would busy-spin the select loop).
                    let Some(menu) = menu else { break };
                    match entries.get(&menu.key) {
                        Some(entry) => show_tray_menu(entry, &menu, &command_tx),
                        None => debug!(item = menu.key, "Ignoring menu for unknown tray item"),
                    }
                }
                request = ipc_rx.recv(), if ipc_available => {
                    let Some(request) = request else {
                        ipc_available = false;
                        debug!("Tray IPC UI channel closed; tray updates remain active");
                        continue;
                    };
                    if request.response.is_closed() {
                        debug!("Skipping tray IPC request after its client timed out");
                        continue;
                    }
                    let response = handle_ipc_request(
                        request.request,
                        &order,
                        &entries,
                        &command_tx,
                    );
                    if request.response.send(response).is_err() {
                        debug!("Tray IPC client disconnected before receiving its response");
                    }
                }
            }
        }
        debug!("System tray UI update channel closed");
    });
}

// Unparent every popover of an entry's open menu, if any. GTK4 popovers are
// not laid-out children: set_parent only attaches them, and skipping the
// explicit unparent leaks them (with a GTK warning) once the parent button is
// finalized. Runs from the tray select loop, never from event dispatch, so the
// widget tree can be mutated directly here.
fn close_tray_menu(open_menu: &Rc<RefCell<Option<OpenTrayMenu>>>) {
    let Some(menu) = open_menu.borrow_mut().take() else {
        return;
    };
    for popover in menu.popovers {
        popover.popdown();
        popover.unparent();
    }
}

// Render a tray item's dbusmenu as a native GTK popover. We build the menu from
// concrete `Button` widgets (instead of `PopoverMenu::from_model`) because the
// model-based popover constructs its widgets lazily and cannot measure its size
// at `popup()` time, which makes it present at zero width/height on a
// layer-shell surface. Concrete widgets measure immediately. Activating an entry
// forwards the entry id back to the backend, which calls dbusmenu's `Event`.
fn show_tray_menu(
    entry: &TrayEntry,
    menu: &TrayMenu,
    command_tx: &mpsc::UnboundedSender<TrayCommand>,
) {
    // A previous menu may still be attached (or even open) — replace it.
    close_tray_menu(&entry.open_menu);

    let popover = gtk4::Popover::new();
    popover.set_parent(&entry.button);
    popover.set_position(gtk4::PositionType::Bottom);
    popover.set_has_arrow(false);

    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    box_.add_css_class("tray-menu");
    debug!(
        item = menu.key,
        labels = ?menu.items.iter().map(|i| &i.label).collect::<Vec<_>>(),
        "Tray menu entry labels"
    );
    let popover_weak = popover.downgrade();
    let mut popovers = vec![popover.clone()];
    let mut menu_entries = Vec::new();
    build_menu_box(
        &box_,
        &menu.items,
        command_tx,
        menu,
        &popover_weak,
        &mut popovers,
        &mut menu_entries,
        &[],
    );
    popover.set_child(Some(&box_));

    *entry.open_menu.borrow_mut() = Some(OpenTrayMenu {
        popovers,
        entries: menu_entries,
        selected: None,
    });
    let open_menu_weak = Rc::downgrade(&entry.open_menu);
    popover.connect_closed(move |_popover| {
        let Some(open_menu) = open_menu_weak.upgrade() else { return };
        let Some(closed) = open_menu.borrow_mut().take() else {
            // close_tray_menu already tore this menu down (item removal or a
            // reopen); nothing left to do.
            return;
        };
        // `closed` fires during event dispatch (e.g. the click-outside that
        // dismissed the menu), and unparenting mid-dispatch breaks GTK's
        // active-state accounting — defer the teardown to an idle callback.
        glib::idle_add_local_once(move || {
            for popover in &closed.popovers {
                popover.unparent();
            }
        });
    });

    debug!(
        item = menu.key,
        entries = menu.items.len(),
        "Presenting tray menu"
    );
    popover.popup();
}

// Recursively fill `box_` with the dbusmenu entries. `top_popover` is the top
// popover so any leaf entry activation can close the whole menu, including
// submenus (whose own activation bubbles up to the same popover). Every popover
// built here is also pushed into `popovers` so the caller can unparent the full
// set when the menu goes away.
fn build_menu_box(
    box_: &gtk4::Box,
    items: &[TrayMenuItem],
    command_tx: &mpsc::UnboundedSender<TrayCommand>,
    menu: &TrayMenu,
    top_popover: &glib::object::WeakRef<gtk4::Popover>,
    popovers: &mut Vec<gtk4::Popover>,
    menu_entries: &mut Vec<OpenTrayMenuEntry>,
    ancestors: &[gtk4::Popover],
) {
    for item in items {
        if !item.visible {
            continue;
        }
        if item.is_separator {
            box_.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
            continue;
        }

        let entry = gtk4::Button::new();
        entry.set_has_frame(false);
        entry.add_css_class("tray-menu-item");
        entry.set_hexpand(true);
        if !item.enabled {
            entry.set_sensitive(false);
        }

        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        content.set_margin_start(8);
        content.set_margin_end(8);
        content.set_margin_top(4);
        content.set_margin_bottom(4);

        let leading = match (&item.toggle_type, item.toggle_state) {
            (Some(toggle_type), Some(true)) if toggle_type == "checkmark" => {
                Some(gtk4::Image::from_icon_name("object-select-symbolic"))
            }
            (Some(toggle_type), Some(true)) if toggle_type == "radio" => {
                Some(gtk4::Image::from_icon_name("media-record-symbolic"))
            }
            _ => item
                .icon_name
                .as_ref()
                .filter(|name| !name.is_empty())
                .map(|name| gtk4::Image::from_icon_name(name)),
        };
        if let Some(icon) = leading {
            content.append(&icon);
        }
        let label = gtk4::Label::new(item.label.as_deref());
        label.set_halign(gtk4::Align::Start);
        label.set_hexpand(true);
        content.append(&label);
        entry.set_child(Some(&content));

        if item.children.is_empty() {
            if item.enabled {
                menu_entries.push(OpenTrayMenuEntry {
                    button: entry.clone(),
                    id: item.id,
                    ancestors: ancestors.to_vec(),
                });
            }
            let command_tx = command_tx.clone();
            let key = menu.key.clone();
            let menu_path = menu.menu_path.clone();
            let top_popover = top_popover.clone();
            let id = item.id;
            entry.connect_clicked(move |_button| {
                // `clicked` fires mid release-event dispatch; popping the menu
                // down right here unmaps the widgets under the pointer and
                // breaks GTK's active-state accounting ("Broken accounting of
                // active state" warnings on the entry icons). Defer it.
                let top_popover = top_popover.clone();
                glib::idle_add_local_once(move || {
                    if let Some(popover) = top_popover.upgrade() {
                        popover.popdown();
                    }
                });
                let command = TrayCommand {
                    key: key.clone(),
                    action: TrayAction::MenuEvent(id),
                    x: 0,
                    y: 0,
                    menu_path: menu_path.clone(),
                };
                if let Err(error) = command_tx.send(command) {
                    warn!(item = key, %error, "Could not send tray menu event to backend");
                }
            });
        } else {
            let sub_popover = gtk4::Popover::new();
            sub_popover.set_parent(&entry);
            sub_popover.set_position(gtk4::PositionType::Right);
            let sub_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            sub_box.add_css_class("tray-menu");
            let mut child_ancestors = ancestors.to_vec();
            child_ancestors.push(sub_popover.clone());
            build_menu_box(
                &sub_box,
                &item.children,
                command_tx,
                menu,
                top_popover,
                popovers,
                menu_entries,
                &child_ancestors,
            );
            sub_popover.set_child(Some(&sub_box));
            popovers.push(sub_popover.clone());
            // Weak for the same cycle reason as the tray button gesture: the
            // closure lives on `entry`, which the popover is attached to.
            let sub_popover_weak = sub_popover.downgrade();
            entry.connect_clicked(move |_button| {
                // Same deferral as leaf entries, for the same accounting reason.
                let sub_popover_weak = sub_popover_weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(popover) = sub_popover_weak.upgrade() {
                        popover.popup();
                    }
                });
            });
        }

        box_.append(&entry);
    }
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
