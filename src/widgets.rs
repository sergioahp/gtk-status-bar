// Widget layer: builds the GTK4 bar tree and owns the consumer side of every
// subsystem channel. Each setup_*_updates takes a receiver from Bus::new and
// drains it onto its label on the GTK main thread (glib::spawn_future_local).
// The producers are spawned separately by activate() with Bus clones, AFTER
// all consumers here are wired — so a producer's first send can never race an
// unwired channel. This module never knows what's inside the channel, only
// that strings/structs come out and labels go in. The volume path is the
// exception: pw's producer is a std::thread, so setup_volume_updates still
// owns both the channel and the thread spawn.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::Local;
use gtk4::gdk;
use gtk4::gio::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tray_ipc::{IpcRequest, IpcResponse, IpcTrayItem, IpcUiRequest};

use crate::bus::{TitleUpdate, VolumeUpdate, WorkspaceUpdate};
use crate::clock::Clock;
use crate::pw;
use crate::tray::{TrayAction, TrayCommand, TrayItem, TrayMenu, TrayMenuItem, TrayUi, TrayUpdate};

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

#[derive(Clone)]
pub struct TitleWidget {
    root: gtk4::CenterBox,
    icon: gtk4::Image,
    label: gtk4::Label,
}

pub fn create_title_widget() -> TitleWidget {
    debug!("Creating title widget");

    let root = gtk4::CenterBox::new();
    root.add_css_class("title-widget");
    root.set_halign(gtk4::Align::End);
    root.set_valign(gtk4::Align::Start);

    let icon = gtk4::Image::new();
    icon.add_css_class("title-icon");
    icon.set_valign(gtk4::Align::Center);
    icon.set_visible(false);
    root.set_start_widget(Some(&icon));

    let label = gtk4::Label::new(Some("Application Title"));
    label.add_css_class("title-label");
    label.set_valign(gtk4::Align::Center);
    // The producer already crops long titles by character count, but wide
    // glyphs can still exceed the remaining monitor width when right-side
    // pills are added. Ellipsizing gives GTK permission to shrink the label's
    // minimum width instead of expanding the layer surface past the output.
    label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    label.set_single_line_mode(true);
    // Label is the CenterBox's own center widget (icon is the start widget)
    // so GtkCenterLayout keeps the title text truly centered in the pill
    // regardless of whether the icon is showing — packing both into one
    // "center" child instead visually centers the icon+label group, which
    // pulls short titles off-center once an icon appears.
    root.set_center_widget(Some(&label));

    TitleWidget { root, icon, label }
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
    Clock::new()
        .on_second(move |now| {
            let Some(label) = label_weak.upgrade() else {
                return;
            };

            let text = now.format("%l:%M %p").to_string();
            debug!("Updating time label: {text}");
            label.set_text(&text);
        })
        .start();
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

pub fn create_network_widget() -> gtk4::Label {
    debug!("Creating network widget");
    let label = gtk4::Label::new(Some("🌐 ?"));
    label.add_css_class("network-widget");
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

pub fn create_right_group() -> (
    gtk4::Box,
    gtk4::Box,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
) {
    debug!("Creating right group");

    let right_container = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    right_container.add_css_class("right-container");
    right_container.set_hexpand(false);
    right_container.set_valign(gtk4::Align::Start);

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

    let network_widget = create_network_widget();
    right_group.append(&network_widget);

    let battery_widget = create_battery_widget();
    right_group.append(&battery_widget);

    let time_widget = create_time_widget();
    right_group.append(&time_widget);

    right_container.append(&right_spacer);
    right_container.append(&right_group);

    (
        right_container,
        tray_widget,
        bt_widget,
        volume_widget,
        network_widget,
        battery_widget,
        time_widget,
    )
}

pub fn create_experimental_bar() -> (
    gtk4::CenterBox,
    gtk4::Box,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    gtk4::Label,
    TitleWidget,
) {
    debug!("Creating experimental bar");

    let main_box = gtk4::CenterBox::new();
    main_box.set_hexpand(true);
    main_box.set_valign(gtk4::Align::Start);

    let (left_group, workspace_widget) = create_left_group();
    let title_widget = create_title_widget();
    let (
        right_group,
        tray_widget,
        bt_widget,
        volume_widget,
        network_widget,
        battery_widget,
        time_widget,
    ) = create_right_group();

    // GtkCenterLayout keeps the title at the monitor midpoint independently
    // of the side groups' widths. Equal expanding spacers cannot guarantee
    // that once the dynamic right group grows wider than its 20em container.
    main_box.set_start_widget(Some(&left_group));
    main_box.set_center_widget(Some(&title_widget.root));
    main_box.set_end_widget(Some(&right_group));

    // Pin the height once the font is resolvable, so dynamic content (title
    // length, tray removal) can't resize the bar and shift windows below it.
    let bar_weak = main_box.downgrade();
    glib::idle_add_local_once(move || {
        if let Some(bar) = bar_weak.upgrade() {
            pin_bar_height_to_font(&bar);
        }
    });

    (
        main_box,
        tray_widget,
        bt_widget,
        volume_widget,
        network_widget,
        battery_widget,
        time_widget,
        workspace_widget,
        title_widget,
    )
}

// Multiplier applied to the measured tall-character height when pinning the bar
// height. A single line box would hug the text too tightly, so we pad it out to
// ~1.4 character cells for comfortable breathing room while still tracking the
// font (and thus DPI / user font size) rather than a fixed pixel count.
const BAR_HEIGHT_CHAR_MULTIPLIER: f64 = 1.4;

// Pin the bar's height to a font-derived "tall character" measurement so the
// surface never resizes when a widget's content changes (e.g. a title growing
// or shrinking, or the tray being removed). Without this the layer-shell window
// tracks its tallest child, so dropping/adding a widget or wrapping text would
// move every window below the bar. We measure on the realized widget so the
// font (and thus its metrics) is actually resolvable, mirroring how the tray
// sizes its icon to a tall glyph rather than a fixed pixel count.
fn pin_bar_height_to_font(bar: &gtk4::CenterBox) {
    let ctx = bar.pango_context();
    let layout = gtk4::pango::Layout::new(&ctx);
    if let Some(font) = ctx.font_description() {
        layout.set_font_description(Some(&font));
    }
    // "Mgj0" carries ascenders, descenders and a digit so the measured height
    // approximates the full text line box rather than just a cap-height glyph.
    layout.set_text("Mgj0");
    let (_width, height) = layout.pixel_size();
    if height > 0 {
        let pinned = (height as f64 * BAR_HEIGHT_CHAR_MULTIPLIER).round() as i32;
        debug!(
            char_height = height,
            multiplier = BAR_HEIGHT_CHAR_MULTIPLIER,
            pinned,
            "Pinning bar height to font-derived line height"
        );
        bar.set_size_request(-1, pinned);
    } else {
        warn!("Could not resolve font metrics to pin bar height; leaving it content-sized");
    }
}

fn argb_to_rgba(width: i32, height: i32, argb: &[u8]) -> Option<Vec<u8>> {
    if width <= 0 || height <= 0 {
        warn!(
            width,
            height, "Ignoring tray pixmap with invalid dimensions"
        );
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
        warn!(
            width,
            height,
            actual = argb.len(),
            expected_len,
            "Ignoring malformed tray pixmap"
        );
        return None;
    }

    let mut rgba = Vec::with_capacity(argb.len());
    for pixel in argb.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
    }
    Some(rgba)
}

// Fallback only used when the widget's font is not yet resolvable (e.g. before
// the widget is realized). Once available we size the icon to a tall character
// of the actual font so it tracks the text rather than a fixed pixel count —
// that keeps it from forcing the bar taller than the text on any machine,
// regardless of DPI or configured font size.
const TRAY_ICON_SIZE_FALLBACK: i32 = 16;

// Height of a tall character for the given image's font, in device pixels.
// We measure the pixel height of a representative glyph ("0") so the icon
// occupies about one character cell, matching the text widgets and scaling
// across machines regardless of DPI or configured font size. This keeps the
// icon from forcing the bar taller than the surrounding text.
fn tray_icon_pixel_size(image: &gtk4::Image) -> i32 {
    let ctx = image.pango_context();
    let layout = gtk4::pango::Layout::new(&ctx);
    if let Some(font) = ctx.font_description() {
        layout.set_font_description(Some(&font));
    }
    layout.set_text("0");
    let (_width, height) = layout.pixel_size();
    if height > 0 {
        height.clamp(1, 64)
    } else {
        TRAY_ICON_SIZE_FALLBACK
    }
}

// A themed icon that has_icon() reports as present can still be impossible to
// draw: when the matching file needs a gdk-pixbuf loader that is not installed
// (e.g. an SVG-only icon with no librsvg loader) GTK resolves it to nothing and
// silently paints "image-missing". Reproduce the decode here so the failure is
// observable in the logs and callers can fall back instead of showing a broken
// glyph. Returns None when the icon is drawable, or Some(reason) when it is not.
fn named_icon_render_error(
    icon_theme: &gtk4::IconTheme,
    name: &str,
    size: i32,
    scale: i32,
) -> Option<String> {
    let paintable = icon_theme.lookup_icon(
        name,
        &[],
        size,
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
    let icon_size = tray_icon_pixel_size(image);
    image.set_pixel_size(icon_size);

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
        match named_icon_render_error(
            &icon_theme,
            &item.icon_name,
            icon_size,
            image.scale_factor(),
        ) {
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
            warn!(
                item = item.key,
                icon_name = item.icon_name,
                "Tray item has no usable icon"
            );
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
    request_id: u64,
    popovers: Vec<gtk4::Popover>,
    entries: Vec<OpenTrayMenuEntry>,
    selected: Option<usize>,
}

struct OpenTrayMenuEntry {
    button: gtk4::Button,
    id: i32,
    ancestors: Vec<gtk4::Popover>,
    submenu: Option<gtk4::Popover>,
}

struct KeyboardGrab {
    application: glib::object::WeakRef<gtk4::Application>,
    active_request: Cell<Option<u64>>,
}

struct KeyboardGrabLease {
    state: Rc<KeyboardGrab>,
    request_id: u64,
    window: gtk4::ApplicationWindow,
}

impl KeyboardGrab {
    fn acquire(
        self: &Rc<Self>,
        request_id: u64,
        nav_tx: &mpsc::UnboundedSender<NavCmd>,
    ) -> Option<KeyboardGrabLease> {
        let Some(application) = self.application.upgrade() else {
            warn!(
                request_id,
                "Cannot grab the keyboard after the GTK application was dropped"
            );
            return None;
        };
        let window = gtk4::ApplicationWindow::builder()
            .application(&application)
            .default_width(1)
            .default_height(1)
            .build();
        window.init_layer_shell();
        window.set_layer(Layer::Bottom);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_exclusive_zone(0);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.add_controller(nav_key_controller(nav_tx, "keyboard-grab"));
        window.present();
        self.active_request.set(Some(request_id));
        info!(request_id, "Tray menu acquired exclusive keyboard focus");
        Some(KeyboardGrabLease {
            state: self.clone(),
            request_id,
            window,
        })
    }

    fn release(&self, request_id: u64) {
        if self.active_request.get() != Some(request_id) {
            return;
        }
        self.active_request.set(None);
        info!(request_id, "Tray menu released keyboard focus");
    }
}

impl Drop for KeyboardGrabLease {
    fn drop(&mut self) {
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.window.close();
        self.state.release(self.request_id);
    }
}

// One directional intent from the keyboard while the tray grab is active. The
// key controllers stay dumb: they translate a keypress into one of these and
// send it to the tray task, which owns the whole navigation state machine (so
// socket verbs and native keys can never disagree about where we are).
#[derive(Debug)]
enum NavCmd {
    Left,     // h / Left
    Right,    // l / Right
    Down,     // j / Down
    Up,       // k / Up
    First,    // gg / Home
    Last,     // G / End
    Activate, // Enter
    Escape,   // q / Esc (two-step: deeper -> level 0, level 0 -> release)
    UserClosed { request_id: u64 },
    MenuTimeout { request_id: u64 },
    MouseAction(TrayCommand),
}

// The live keyboard-navigation session. Present only while the grab is held.
// `key` identifies the pre-selected tray icon (level 0); `in_menu` is false at
// level 0 (scrolling icons, dropdown open but no entry selected) and true once
// we have descended into the open dropdown. Dropping `lease` releases the
// keyboard.
struct NavActive {
    // Held only for its Drop, which releases the grab; never read directly.
    #[allow(dead_code)]
    lease: KeyboardGrabLease,
    key: String,
    menu_request_id: u64,
    in_menu: bool,
}

struct MenuBuildContext<'a> {
    command_tx: &'a mpsc::UnboundedSender<TrayCommand>,
    menu: &'a TrayMenu,
    top_popover: &'a glib::object::WeakRef<gtk4::Popover>,
    popovers: &'a mut Vec<gtk4::Popover>,
    entries: &'a mut Vec<OpenTrayMenuEntry>,
}

fn ipc_item(index: usize, item: &TrayItem) -> IpcTrayItem {
    IpcTrayItem {
        index,
        key: item.key.clone(),
        title: item.title.clone(),
        status: item.status.clone(),
        item_is_menu: item.item_is_menu,
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

fn next_menu_request(sequence: &Cell<u64>) -> u64 {
    let next = sequence.get().wrapping_add(1);
    sequence.set(next);
    next
}

fn select_menu_index(menu: &mut OpenTrayMenu, selected: usize) -> i32 {
    if let Some(previous) = menu.selected {
        menu.entries[previous].button.remove_css_class("selected");
    }
    let selected_entry = &menu.entries[selected];
    for popover in menu.popovers.iter().skip(1) {
        if !selected_entry
            .ancestors
            .iter()
            .any(|ancestor| ancestor == popover)
        {
            popover.popdown();
        }
    }
    for popover in &selected_entry.ancestors {
        popover.popup();
    }
    selected_entry.button.add_css_class("selected");
    selected_entry.button.grab_focus();
    let id = selected_entry.id;
    menu.selected = Some(selected);
    id
}

fn entries_in_scope(menu: &OpenTrayMenu) -> Vec<usize> {
    let scope = menu
        .selected
        .map(|selected| menu.entries[selected].ancestors.as_slice())
        .unwrap_or_default();
    menu.entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| (entry.ancestors.as_slice() == scope).then_some(index))
        .collect()
}

fn select_menu_entry(
    open_menu: &Rc<RefCell<Option<OpenTrayMenu>>>,
    direction: isize,
) -> std::result::Result<i32, String> {
    let mut open_menu = open_menu.borrow_mut();
    let menu = open_menu
        .as_mut()
        .ok_or_else(|| "tray item has no open menu".to_string())?;
    if menu.entries.is_empty() {
        return Err("open tray menu has no enabled entries".to_string());
    }
    let candidates = entries_in_scope(menu);
    if candidates.is_empty() {
        // entries_in_scope can be empty while `entries` is not: e.g. the current
        // scope is the top level but every enabled entry lives inside a submenu.
        // Guard before indexing/rem_euclid so a keypress can't panic the bar.
        return Err("open tray menu has no enabled entries in the current scope".to_string());
    }
    let len = candidates.len() as isize;
    let candidate = match menu.selected.and_then(|selected| {
        candidates
            .iter()
            .position(|candidate| *candidate == selected)
    }) {
        Some(selected) => (selected as isize + direction).rem_euclid(len) as usize,
        None if direction < 0 => candidates.len() - 1,
        None => 0,
    };
    Ok(select_menu_index(menu, candidates[candidate]))
}

fn jump_menu_entry(
    open_menu: &Rc<RefCell<Option<OpenTrayMenu>>>,
    last: bool,
) -> std::result::Result<i32, String> {
    let mut open_menu = open_menu.borrow_mut();
    let menu = open_menu
        .as_mut()
        .ok_or_else(|| "tray item has no open menu".to_string())?;
    let candidates = entries_in_scope(menu);
    let selected = if last {
        candidates.last().copied()
    } else {
        candidates.first().copied()
    }
    .ok_or_else(|| "open tray menu has no enabled entries".to_string())?;
    Ok(select_menu_index(menu, selected))
}

fn enter_submenu(
    open_menu: &Rc<RefCell<Option<OpenTrayMenu>>>,
) -> std::result::Result<bool, String> {
    let mut open_menu = open_menu.borrow_mut();
    let menu = open_menu
        .as_mut()
        .ok_or_else(|| "tray item has no open menu".to_string())?;
    let Some(selected) = menu.selected else {
        return Ok(false);
    };
    let Some(submenu) = menu.entries[selected].submenu.clone() else {
        return Ok(false);
    };
    let child = menu
        .entries
        .iter()
        .position(|candidate| candidate.ancestors.last() == Some(&submenu))
        .ok_or_else(|| "open tray submenu has no enabled entries".to_string())?;
    submenu.popup();
    select_menu_index(menu, child);
    Ok(true)
}

fn leave_submenu(
    open_menu: &Rc<RefCell<Option<OpenTrayMenu>>>,
) -> std::result::Result<bool, String> {
    let mut open_menu = open_menu.borrow_mut();
    let menu = open_menu
        .as_mut()
        .ok_or_else(|| "tray item has no open menu".to_string())?;
    let Some(selected) = menu.selected else {
        return Ok(false);
    };
    let Some(submenu) = menu.entries[selected].ancestors.last().cloned() else {
        return Ok(false);
    };
    let parent = menu
        .entries
        .iter()
        .position(|candidate| candidate.submenu.as_ref() == Some(&submenu))
        .ok_or_else(|| "open tray submenu has no parent entry".to_string())?;
    submenu.popdown();
    select_menu_index(menu, parent);
    Ok(true)
}

fn menu_entry_id(
    open_menu: &Rc<RefCell<Option<OpenTrayMenu>>>,
    requested: Option<i32>,
) -> std::result::Result<i32, String> {
    let open_menu = open_menu.borrow();
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
    menu_requests: &Cell<u64>,
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
            next_menu_request(menu_requests);
            for entry in entries.values() {
                close_tray_menu(&entry.open_menu);
            }
            return IpcResponse::success(Vec::new());
        }
        IpcRequest::Activate { target } => (target.clone(), None),
        IpcRequest::SecondaryActivate { target } => {
            (target.clone(), Some(TrayAction::SecondaryActivate))
        }
        IpcRequest::ContextMenu { target } | IpcRequest::KeyboardMenu { target } => {
            (target.clone(), None)
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
        IpcRequest::Activate { .. } if item.item_is_menu => TrayAction::ContextMenu {
            request_id: next_menu_request(menu_requests),
            keyboard_grab: false,
        },
        IpcRequest::Activate { .. } => TrayAction::Activate,
        IpcRequest::MenuNext { .. } => match select_menu_entry(&entry.open_menu, 1) {
            Ok(_id) => return IpcResponse::success(vec![selected]),
            Err(error) => return IpcResponse::error(error),
        },
        IpcRequest::MenuPrevious { .. } => match select_menu_entry(&entry.open_menu, -1) {
            Ok(_id) => return IpcResponse::success(vec![selected]),
            Err(error) => return IpcResponse::error(error),
        },
        IpcRequest::MenuActivate { .. } => {
            match enter_submenu(&entry.open_menu) {
                Ok(true) => return IpcResponse::success(vec![selected]),
                Ok(false) => {}
                Err(error) => return IpcResponse::error(error),
            }
            match menu_entry_id(&entry.open_menu, None) {
                Ok(id) => TrayAction::MenuEvent(id),
                Err(error) => return IpcResponse::error(error),
            }
        }
        IpcRequest::MenuClick { entry: id, .. } => {
            match menu_entry_id(&entry.open_menu, Some(id)) {
                Ok(id) => TrayAction::MenuEvent(id),
                Err(error) => return IpcResponse::error(error),
            }
        }
        IpcRequest::ContextMenu { .. } => TrayAction::ContextMenu {
            request_id: next_menu_request(menu_requests),
            keyboard_grab: false,
        },
        IpcRequest::KeyboardMenu { .. } => TrayAction::ContextMenu {
            request_id: next_menu_request(menu_requests),
            keyboard_grab: true,
        },
        IpcRequest::SecondaryActivate { .. } => {
            let Some(action) = action else {
                return IpcResponse::error("tray action was not resolved");
            };
            action
        }
        IpcRequest::List | IpcRequest::CloseMenus => {
            return IpcResponse::error("request was handled before target resolution");
        }
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

fn ipc_request_targets_nav(
    request: &IpcRequest,
    nav: &Option<NavActive>,
    order: &[String],
    entries: &HashMap<String, TrayEntry>,
) -> bool {
    let Some(active) = nav.as_ref() else {
        return false;
    };
    let target = match request {
        IpcRequest::Activate { target }
        | IpcRequest::SecondaryActivate { target }
        | IpcRequest::ContextMenu { target }
        | IpcRequest::KeyboardMenu { target }
        | IpcRequest::MenuNext { target }
        | IpcRequest::MenuPrevious { target }
        | IpcRequest::MenuActivate { target }
        | IpcRequest::MenuClick { target, .. } => target,
        IpcRequest::List | IpcRequest::CloseMenus => return false,
    };
    resolve_ipc_target(target, order, entries)
        .is_ok_and(|(_, entry)| entry.state.borrow().key == active.key)
}

fn create_tray_entry(
    item: TrayItem,
    menu_requests: &Rc<Cell<u64>>,
    nav_tx: &mpsc::UnboundedSender<NavCmd>,
) -> TrayEntry {
    let button = gtk4::Button::new();
    button.add_css_class("tray-item");
    button.set_focusable(false);

    let image = gtk4::Image::new();
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
            x,
            y,
            "Tray button pressed"
        );
        gesture_pressed.set_state(gtk4::EventSequenceState::Claimed);
    });
    let nav_tx = nav_tx.clone();
    let menu_requests = menu_requests.clone();
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
            x,
            y,
            item_is_menu,
            "Tray button released"
        );
        let action = match gesture.current_button() {
            1 if item_is_menu => TrayAction::ContextMenu {
                request_id: next_menu_request(&menu_requests),
                keyboard_grab: false,
            },
            1 => TrayAction::Activate,
            2 => TrayAction::SecondaryActivate,
            3 => TrayAction::ContextMenu {
                request_id: next_menu_request(&menu_requests),
                keyboard_grab: false,
            },
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
                            debug!(
                                item = key,
                                "Could not translate tray click to bar coordinates"
                            );
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
        if let Err(error) = nav_tx.send(NavCmd::MouseAction(command)) {
            warn!(item = key, %error, "Could not queue tray click for D-Bus backend");
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

pub fn setup_tray_updates(
    TrayUi {
        mut updates,
        commands,
        mut menus,
    }: TrayUi,
    mut ipc_requests: mpsc::UnboundedReceiver<IpcUiRequest>,
    container: gtk4::Box,
    window: &gtk4::ApplicationWindow,
) {
    debug!("Setting up system tray updates");

    let menu_requests = Rc::new(Cell::new(0));
    let application = window
        .application()
        .expect("the status bar window must belong to its GTK application");
    let keyboard_grab = Rc::new(KeyboardGrab {
        application: application.downgrade(),
        active_request: Cell::new(None),
    });
    // Key controllers on the open dropdowns forward NavCmd here; the task owns
    // the navigation state so socket verbs and native keys share one truth.
    let (nav_tx, mut nav_rx) = mpsc::unbounded_channel::<NavCmd>();

    glib::spawn_future_local(async move {
        let mut entries: HashMap<String, TrayEntry> = HashMap::new();
        let mut order = Vec::new();
        let mut nav: Option<NavActive> = None;
        let mut ipc_available = true;
        loop {
            tokio::select! {
                update = updates.recv() => {
                    let Some(update) = update else { break };
                    match update {
                        TrayUpdate::Upsert(item) => match entries.get(&item.key) {
                            Some(entry) => {
                                update_tray_button(&entry.button, &entry.image, &item);
                                *entry.state.borrow_mut() = item;
                            }
                            None => {
                                let key = item.key.clone();
                                let entry = create_tray_entry(
                                    item,
                                    &menu_requests,
                                    &nav_tx,
                                );
                                container.append(&entry.button);
                                entries.insert(key.clone(), entry);
                                order.push(key.clone());
                                container.set_visible(true);
                                info!(item = key, "Added system tray item");
                            }
                        },
                        TrayUpdate::Remove(key) => match entries.remove(&key) {
                            Some(entry) => {
                                let removed_index = order.iter().position(|item_key| item_key == &key);
                                let removed_active_item = nav
                                    .as_ref()
                                    .is_some_and(|active| active.key == key);
                                // Unparent any open menu first: popovers are only
                                // attached to the button, not laid-out children,
                                // and GTK complains when a widget is finalized
                                // with one still attached.
                                close_tray_menu(&entry.open_menu);
                                container.remove(&entry.button);
                                order.retain(|item_key| item_key != &key);
                                container.set_visible(!entries.is_empty());
                                info!(item = key, "Removed system tray item");

                                if removed_active_item {
                                    let Some(index) = removed_index
                                        .map(|index| index.min(order.len().saturating_sub(1)))
                                        .filter(|_| !order.is_empty())
                                    else {
                                        end_nav(&mut nav, &container, &entries, &menu_requests);
                                        continue;
                                    };
                                    if let Err(error) = start_nav(
                                        &mut nav,
                                        index,
                                        &keyboard_grab,
                                        &nav_tx,
                                        &container,
                                        &entries,
                                        &order,
                                        &commands,
                                        &menu_requests,
                                    ) {
                                        warn!(%error, "Ending tray navigation after item removal");
                                        end_nav(&mut nav, &container, &entries, &menu_requests);
                                    }
                                }
                            }
                            None => debug!(item = key, "Ignoring removal of unknown tray item"),
                        },
                    }
                }
                menu = menus.recv() => {
                    // The supervised backend holds menu_tx for the process
                    // lifetime, so None means teardown, same as the update arm
                    // (a `continue` here would busy-spin the select loop).
                    let Some(menu) = menu else { break };
                    if menu.request_id != menu_requests.get() {
                        debug!(
                            item = menu.key,
                            request_id = menu.request_id,
                            current_request_id = menu_requests.get(),
                            "Ignoring canceled or superseded tray menu"
                        );
                        continue;
                    }
                    match entries.get(&menu.key) {
                        Some(entry) => show_tray_menu(entry, &menu, &commands, &nav_tx),
                        None => debug!(item = menu.key, "Ignoring menu for unknown tray item"),
                    }
                }
                nav_cmd = nav_rx.recv() => {
                    // nav_tx lives for the whole task, so recv() only ends at
                    // teardown (same semantics as the arms above).
                    let Some(nav_cmd) = nav_cmd else { break };
                    handle_nav(
                        nav_cmd,
                        &mut nav,
                        &nav_tx,
                        &order,
                        &entries,
                        &container,
                        &commands,
                        &menu_requests,
                    );
                }
                request = ipc_requests.recv(), if ipc_available => {
                    let Some(request) = request else {
                        ipc_available = false;
                        debug!("Tray IPC UI channel closed; tray updates remain active");
                        continue;
                    };
                    if request.response.is_closed() {
                        debug!("Skipping tray IPC request after its client timed out");
                        continue;
                    }
                    let response = match request.request {
                        // keyboard-menu starts (or retargets) a nav session at
                        // level 0 rather than opening one throwaway grab menu.
                        IpcRequest::KeyboardMenu { target } => {
                            match resolve_ipc_target(&target, &order, &entries) {
                                Ok((index, entry)) => {
                                    let selected = ipc_item(index, &entry.state.borrow());
                                    start_nav(
                                        &mut nav,
                                        index,
                                        &keyboard_grab,
                                        &nav_tx,
                                        &container,
                                        &entries,
                                        &order,
                                        &commands,
                                        &menu_requests,
                                    )
                                    .map_or_else(
                                        IpcResponse::error,
                                        |()| IpcResponse::success(vec![selected]),
                                    )
                                }
                                Err(error) => IpcResponse::error(error),
                            }
                        }
                        // close-menus also releases the grab, so it is a
                        // reliable kill-switch for a stuck nav session.
                        IpcRequest::CloseMenus => {
                            end_nav(&mut nav, &container, &entries, &menu_requests);
                            handle_ipc_request(
                                IpcRequest::CloseMenus,
                                &order,
                                &entries,
                                &commands,
                                &menu_requests,
                            )
                        }
                        other => {
                            let targets_nav = ipc_request_targets_nav(
                                &other,
                                &nav,
                                &order,
                                &entries,
                            );
                            let ends_nav = matches!(
                                &other,
                                IpcRequest::Activate { .. }
                                    | IpcRequest::SecondaryActivate { .. }
                                    | IpcRequest::ContextMenu { .. }
                                    | IpcRequest::MenuClick { .. }
                            );
                            let activates_menu =
                                matches!(&other, IpcRequest::MenuActivate { .. });
                            let enters_menu = matches!(
                                &other,
                                IpcRequest::MenuNext { .. } | IpcRequest::MenuPrevious { .. }
                            );
                            let response = handle_ipc_request(
                                other,
                                &order,
                                &entries,
                                &commands,
                                &menu_requests,
                            );
                            if response.ok && ends_nav {
                                end_nav(&mut nav, &container, &entries, &menu_requests);
                            } else if response.ok && targets_nav && enters_menu {
                                set_nav_in_menu(&mut nav, &container, true);
                            } else if response.ok && targets_nav && activates_menu {
                                let menu_still_open = nav
                                    .as_ref()
                                    .and_then(|active| entries.get(&active.key))
                                    .is_some_and(|entry| entry.open_menu.borrow().is_some());
                                if menu_still_open {
                                    set_nav_in_menu(&mut nav, &container, true);
                                } else {
                                    end_nav(&mut nav, &container, &entries, &menu_requests);
                                }
                            }
                            response
                        }
                    };
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

// Translate keys into NavCmd and forward them to the tray task. The controller
// holds no navigation state of its own (the task owns it); it only knows the
// `gg` two-key jump, which needs to remember a lone `g` between presses.
fn nav_key_controller(
    nav_tx: &mpsc::UnboundedSender<NavCmd>,
    item_key: &str,
) -> gtk4::EventControllerKey {
    let controller = gtk4::EventControllerKey::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let nav_tx = nav_tx.clone();
    let item_key = item_key.to_string();
    let pending_g = Cell::new(false);
    controller.connect_key_pressed(move |_controller, key, _code, _modifiers| {
        if key == gdk::Key::g {
            // First `g` arms the jump; the second fires it.
            if pending_g.replace(true) {
                pending_g.set(false);
                let _ = nav_tx.send(NavCmd::First);
            }
            return glib::Propagation::Stop;
        }
        pending_g.set(false);

        let cmd = match key {
            gdk::Key::h | gdk::Key::Left => NavCmd::Left,
            gdk::Key::l | gdk::Key::Right => NavCmd::Right,
            gdk::Key::j | gdk::Key::Down => NavCmd::Down,
            gdk::Key::k | gdk::Key::Up => NavCmd::Up,
            gdk::Key::G | gdk::Key::End => NavCmd::Last,
            gdk::Key::Home => NavCmd::First,
            gdk::Key::Return | gdk::Key::KP_Enter => NavCmd::Activate,
            gdk::Key::Escape | gdk::Key::q => NavCmd::Escape,
            _ => return glib::Propagation::Proceed,
        };
        debug!(item = item_key, ?cmd, "Tray nav key");
        if nav_tx.send(cmd).is_err() {
            debug!(item = item_key, "Tray nav channel closed; dropping key");
        }
        glib::Propagation::Stop
    });
    controller
}

// Highlight exactly one tray icon as the level-0 pre-selection. Clearing first
// keeps the "at most one" invariant even as items are added or removed.
fn update_nav_highlight(entries: &HashMap<String, TrayEntry>, order: &[String], index: usize) {
    for entry in entries.values() {
        entry.button.remove_css_class("nav-selected");
    }
    if let Some(entry) = order.get(index).and_then(|key| entries.get(key)) {
        entry.button.add_css_class("nav-selected");
    }
}

fn set_nav_in_menu(nav: &mut Option<NavActive>, container: &gtk4::Box, in_menu: bool) {
    let Some(active) = nav.as_mut() else {
        return;
    };
    active.in_menu = in_menu;
    if in_menu {
        container.remove_css_class("nav-level-zero");
    } else {
        container.add_css_class("nav-level-zero");
    }
}

fn move_tray_index(index: usize, len: usize, cmd: &NavCmd) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match cmd {
        NavCmd::Left => Some((index + len - 1) % len),
        NavCmd::Right => Some((index + 1) % len),
        NavCmd::First => Some(0),
        NavCmd::Last => Some(len - 1),
        _ => None,
    }
}

// Drop back to level 0 within an open dropdown: forget the selected entry (and
// its highlight) and collapse any open submenus so only the top level shows.
fn clear_menu_selection(open_menu: &Rc<RefCell<Option<OpenTrayMenu>>>) {
    let mut open_menu = open_menu.borrow_mut();
    let Some(menu) = open_menu.as_mut() else {
        return;
    };
    if let Some(selected) = menu.selected.take() {
        if let Some(entry) = menu.entries.get(selected) {
            entry.button.remove_css_class("selected");
        }
    }
    for popover in menu.popovers.iter().skip(1) {
        popover.popdown();
    }
}

// Ask the backend to (re)open one icon's dropdown as a keyboard-grab menu. The
// grab itself belongs to the nav session; this only flags the menu so
// show_tray_menu wires up the nav controller and pops up focused.
fn open_icon_menu(
    entry: &TrayEntry,
    commands: &mpsc::UnboundedSender<TrayCommand>,
    menu_requests: &Rc<Cell<u64>>,
    nav_tx: &mpsc::UnboundedSender<NavCmd>,
) -> std::result::Result<u64, String> {
    let (x, y) = tray_button_coordinates(&entry.button);
    let item = entry.state.borrow();
    let request_id = next_menu_request(menu_requests);
    let command = TrayCommand {
        key: item.key.clone(),
        action: TrayAction::ContextMenu {
            request_id,
            keyboard_grab: true,
        },
        x,
        y,
        menu_path: item.menu_path.clone(),
    };
    commands
        .send(command)
        .map_err(|error| format!("could not open tray menu for keyboard nav: {error}"))?;
    let nav_tx = nav_tx.clone();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        let _ = nav_tx.send(NavCmd::MenuTimeout { request_id });
    });
    Ok(request_id)
}

// Start (or retarget) a keyboard-navigation session at `index`, entering at
// level 0 with that icon's dropdown auto-opened. The first call acquires the
// grab; later calls just move the pre-selection.
#[allow(clippy::too_many_arguments)]
fn start_nav(
    nav: &mut Option<NavActive>,
    index: usize,
    keyboard_grab: &Rc<KeyboardGrab>,
    nav_tx: &mpsc::UnboundedSender<NavCmd>,
    container: &gtk4::Box,
    entries: &HashMap<String, TrayEntry>,
    order: &[String],
    commands: &mpsc::UnboundedSender<TrayCommand>,
    menu_requests: &Rc<Cell<u64>>,
) -> std::result::Result<(), String> {
    let key = order
        .get(index)
        .ok_or_else(|| format!("tray item index {index} is no longer available"))?
        .clone();
    let entry = entries
        .get(&key)
        .ok_or_else(|| format!("tray item {key:?} is no longer available"))?;

    match nav.as_mut() {
        Some(active) => {
            active.key = key.clone();
        }
        None => {
            let request_id = next_menu_request(menu_requests);
            let Some(lease) = keyboard_grab.acquire(request_id, nav_tx) else {
                return Err("could not acquire the keyboard grab for tray navigation".to_string());
            };
            container.add_css_class("nav-focus");
            *nav = Some(NavActive {
                lease,
                key: key.clone(),
                menu_request_id: request_id,
                in_menu: false,
            });
        }
    }
    set_nav_in_menu(nav, container, false);
    // Only one dropdown open at a time.
    for entry in entries.values() {
        close_tray_menu(&entry.open_menu);
    }
    update_nav_highlight(entries, order, index);
    let request_id = open_icon_menu(entry, commands, menu_requests, nav_tx)?;
    if let Some(active) = nav.as_mut() {
        active.menu_request_id = request_id;
    }
    Ok(())
}

// Tear the session down: release the grab (dropping the lease restores focus to
// whatever held it before), close the dropdown, and clear all focus visuals.
fn end_nav(
    nav: &mut Option<NavActive>,
    container: &gtk4::Box,
    entries: &HashMap<String, TrayEntry>,
    menu_requests: &Cell<u64>,
) {
    let Some(active) = nav.take() else {
        return;
    };
    if menu_requests.get() == active.menu_request_id {
        next_menu_request(menu_requests);
    }
    for entry in entries.values() {
        close_tray_menu(&entry.open_menu);
        entry.button.remove_css_class("nav-selected");
        // Unmapping the exclusive helper can leave GTK's hover state on the
        // icon under a stationary pointer. Clear that transient tint with the
        // keyboard state; normal pointer motion will establish hover again.
        entry.button.unset_state_flags(gtk4::StateFlags::PRELIGHT);
    }
    container.remove_css_class("nav-focus");
    container.remove_css_class("nav-level-zero");
    drop(active);
    debug!("Tray keyboard navigation ended");
}

// The navigation state machine, driven by one NavCmd at a time. Level 0 scrolls
// across tray icons (auto-opening dropdowns); deeper levels move within the open
// dropdown. Escape is two-step: a deeper level drops to level 0, level 0
// releases the grab.
#[allow(clippy::too_many_arguments)]
fn handle_nav(
    cmd: NavCmd,
    nav: &mut Option<NavActive>,
    nav_tx: &mpsc::UnboundedSender<NavCmd>,
    order: &[String],
    entries: &HashMap<String, TrayEntry>,
    container: &gtk4::Box,
    commands: &mpsc::UnboundedSender<TrayCommand>,
    menu_requests: &Rc<Cell<u64>>,
) {
    let cmd = match cmd {
        NavCmd::MouseAction(command) => {
            end_nav(nav, container, entries, menu_requests);
            if let Err(error) = commands.send(command) {
                warn!(%error, "Could not send tray click to D-Bus backend");
            }
            return;
        }
        other => other,
    };

    // Snapshot the session; a late key after release finds nothing to drive.
    let (key, in_menu, menu_request_id) = match nav.as_ref() {
        Some(active) => (active.key.clone(), active.in_menu, active.menu_request_id),
        None => return,
    };
    let Some(index) = order.iter().position(|candidate| candidate == &key) else {
        warn!(
            item = key,
            "Ending tray navigation after its selected item disappeared"
        );
        end_nav(nav, container, entries, menu_requests);
        return;
    };

    match &cmd {
        NavCmd::UserClosed { request_id } if *request_id == menu_request_id => {
            end_nav(nav, container, entries, menu_requests);
            return;
        }
        NavCmd::UserClosed { request_id } => {
            debug!(
                request_id,
                current_request_id = menu_request_id,
                "Ignoring stale tray menu close"
            );
            return;
        }
        NavCmd::MenuTimeout { request_id } if *request_id == menu_request_id => {
            let menu_loaded = entries
                .get(&key)
                .is_some_and(|entry| entry.open_menu.borrow().is_some());
            if !menu_loaded {
                warn!(
                    item = key,
                    request_id, "Tray menu did not load before timeout"
                );
                end_nav(nav, container, entries, menu_requests);
            }
            return;
        }
        NavCmd::MenuTimeout { .. } => return,
        NavCmd::Escape => {
            if in_menu {
                if let Some(entry) = order.get(index).and_then(|key| entries.get(key)) {
                    clear_menu_selection(&entry.open_menu);
                }
                set_nav_in_menu(nav, container, false);
            } else {
                end_nav(nav, container, entries, menu_requests);
            }
            return;
        }
        _ => {}
    }

    if !in_menu {
        // LEVEL 0: scroll across icons; the landed icon auto-opens.
        let len = order.len();
        if len == 0 {
            return;
        }
        match cmd {
            NavCmd::Left | NavCmd::Right | NavCmd::First | NavCmd::Last => {
                let Some(new_index) = move_tray_index(index, len, &cmd) else {
                    return;
                };
                if new_index == index {
                    return;
                }
                if let Some(entry) = order.get(index).and_then(|key| entries.get(key)) {
                    close_tray_menu(&entry.open_menu);
                }
                if let Some(active) = nav.as_mut() {
                    active.key = order[new_index].clone();
                }
                update_nav_highlight(entries, order, new_index);
                if let Some(entry) = order.get(new_index).and_then(|key| entries.get(key)) {
                    match open_icon_menu(entry, commands, menu_requests, nav_tx) {
                        Ok(request_id) => {
                            if let Some(active) = nav.as_mut() {
                                active.menu_request_id = request_id;
                            }
                        }
                        Err(error) => {
                            warn!(%error, "Ending tray navigation after icon switch failed");
                            end_nav(nav, container, entries, menu_requests);
                        }
                    }
                }
            }
            NavCmd::Down | NavCmd::Up | NavCmd::Activate => {
                // Either vertical direction enters the dropdown. Down/Enter
                // select its first entry; Up selects its last entry.
                if let Some(entry) = order.get(index).and_then(|key| entries.get(key)) {
                    let direction = if matches!(cmd, NavCmd::Up) { -1 } else { 1 };
                    match select_menu_entry(&entry.open_menu, direction) {
                        Ok(_) => {
                            set_nav_in_menu(nav, container, true);
                        }
                        Err(error) => debug!(%error, "Cannot descend into tray dropdown"),
                    }
                }
            }
            _ => {}
        }
        return;
    }

    // IN A DROPDOWN (level >= 1).
    let Some(entry) = order.get(index).and_then(|key| entries.get(key)) else {
        return;
    };
    let open_menu = &entry.open_menu;
    match cmd {
        NavCmd::Down => {
            if let Err(error) = select_menu_entry(open_menu, 1) {
                debug!(%error, "Could not move down in tray dropdown");
            }
        }
        NavCmd::Up => {
            if let Err(error) = select_menu_entry(open_menu, -1) {
                debug!(%error, "Could not move up in tray dropdown");
            }
        }
        NavCmd::First => {
            if let Err(error) = jump_menu_entry(open_menu, false) {
                debug!(%error, "Could not jump to first tray dropdown entry");
            }
        }
        NavCmd::Last => {
            if let Err(error) = jump_menu_entry(open_menu, true) {
                debug!(%error, "Could not jump to last tray dropdown entry");
            }
        }
        NavCmd::Right => {
            if let Err(error) = enter_submenu(open_menu) {
                debug!(%error, "Could not enter tray submenu");
            }
        }
        NavCmd::Left => match leave_submenu(open_menu) {
            Ok(true) => {} // stepped up one submenu level
            Ok(false) => {
                // Already at the dropdown's top level: return to level 0.
                clear_menu_selection(open_menu);
                set_nav_in_menu(nav, container, false);
            }
            Err(error) => debug!(%error, "Could not leave tray submenu"),
        },
        NavCmd::Activate => match enter_submenu(open_menu) {
            Ok(true) => {}
            Ok(false) => {
                let button = open_menu
                    .borrow()
                    .as_ref()
                    .and_then(|menu| menu.selected.map(|i| menu.entries[i].button.clone()));
                match button {
                    // Fires MenuEvent and closes the menu; the resulting
                    // UserClosed then ends the session.
                    Some(button) => button.emit_clicked(),
                    None => debug!("Tray dropdown has no selected entry to activate"),
                }
            }
            Err(error) => debug!(%error, "Could not activate tray dropdown entry"),
        },
        _ => {}
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
    nav_tx: &mpsc::UnboundedSender<NavCmd>,
) {
    // A previous menu may still be attached (or even open) — replace it.
    close_tray_menu(&entry.open_menu);

    let popover = gtk4::Popover::new();
    popover.set_parent(&entry.button);
    popover.set_position(gtk4::PositionType::Bottom);
    popover.set_has_arrow(false);
    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    box_.add_css_class("tray-menu");
    box_.set_focusable(true);
    debug!(
        item = menu.key,
        labels = ?menu.items.iter().map(|i| &i.label).collect::<Vec<_>>(),
        "Tray menu entry labels"
    );
    let popover_weak = popover.downgrade();
    let mut popovers = vec![popover.clone()];
    let mut menu_entries = Vec::new();
    let mut build = MenuBuildContext {
        command_tx,
        menu,
        top_popover: &popover_weak,
        popovers: &mut popovers,
        entries: &mut menu_entries,
    };
    build_menu_box(&box_, &menu.items, &mut build, &[]);
    popover.set_child(Some(&box_));

    // Each submenu is its own native popover and can receive keyboard focus.
    // Install the controller on all of them so navigation does not fall back to
    // GTK's default handling when a submenu opens.
    let is_nav = menu.keyboard_grab;
    if is_nav {
        for menu_popover in &popovers {
            menu_popover.add_controller(nav_key_controller(nav_tx, &menu.key));
        }
    }

    *entry.open_menu.borrow_mut() = Some(OpenTrayMenu {
        request_id: menu.request_id,
        popovers,
        entries: menu_entries,
        selected: None,
    });

    // `keyboard_grab` here means "this dropdown is part of a nav session" — the
    // grab itself is owned by the session, not the menu.
    let open_menu_weak = Rc::downgrade(&entry.open_menu);
    let nav_tx_closed = nav_tx.clone();
    popover.connect_closed(move |_popover| {
        let Some(open_menu) = open_menu_weak.upgrade() else {
            return;
        };
        let Some(closed) = open_menu.borrow_mut().take() else {
            // close_tray_menu already tore this menu down (item removal, reopen,
            // or a nav icon-switch): a programmatic close, not a user dismissal,
            // so there is nothing to unparent and no session to end.
            return;
        };
        if is_nav {
            // We took a live menu here, so the user dismissed it (a click
            // outside the popover): end the whole nav session.
            let _ = nav_tx_closed.send(NavCmd::UserClosed {
                request_id: closed.request_id,
            });
        }
        // `closed` fires during event dispatch; unparenting mid-dispatch breaks
        // GTK's active-state accounting, so defer it to an idle callback.
        glib::idle_add_local_once(move || {
            for popover in &closed.popovers {
                popover.unparent();
            }
            drop(closed);
        });
    });

    debug!(
        item = menu.key,
        entries = menu.items.len(),
        keyboard_grab = menu.keyboard_grab,
        "Presenting tray menu"
    );
    if is_nav {
        // Defer the popup so the exclusive helper surface (grabbed by the nav
        // session) has settled keyboard focus before the popover maps. We land
        // at level 0: the dropdown is open but no entry is selected yet.
        let item_key = menu.key.clone();
        let request_id = menu.request_id;
        let popover_weak = popover.downgrade();
        let box_weak = box_.downgrade();
        let open_menu_weak = Rc::downgrade(&entry.open_menu);
        glib::timeout_add_local_once(Duration::from_millis(100), move || {
            let Some(open_menu) = open_menu_weak.upgrade() else {
                return;
            };
            let still_current = open_menu
                .borrow()
                .as_ref()
                .is_some_and(|menu| menu.request_id == request_id);
            if !still_current {
                return;
            }
            let (Some(popover), Some(box_)) = (popover_weak.upgrade(), box_weak.upgrade()) else {
                return;
            };
            popover.popup();
            box_.grab_focus();
            debug!(item = item_key, "Tray dropdown opened at level 0");
        });
    } else {
        popover.popup();
        box_.grab_focus();
    }
}

// Recursively fill `box_` with the dbusmenu entries. `top_popover` is the top
// popover so any leaf entry activation can close the whole menu, including
// submenus (whose own activation bubbles up to the same popover). Every popover
// built here is also pushed into `popovers` so the caller can unparent the full
// set when the menu goes away.
fn build_menu_box(
    box_: &gtk4::Box,
    items: &[TrayMenuItem],
    build: &mut MenuBuildContext<'_>,
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
        let pointer_motion = gtk4::EventControllerMotion::new();
        let entry_weak = entry.downgrade();
        pointer_motion.connect_motion(move |controller, _x, _y| {
            // Popover allocation asks GDK to emit a synthetic motion event so
            // GTK can repick pointer focus after the popup moves. Its timestamp
            // is GDK_CURRENT_TIME (zero); exposing :hover for that event is what
            // produces the one-frame last-row flash. Only timestamped device
            // motion earns the visual hover class.
            if controller.current_event_time() == gdk::CURRENT_TIME {
                return;
            }
            if let Some(entry) = entry_weak.upgrade() {
                entry.add_css_class("pointer-hover");
            }
        });
        let entry_weak = entry.downgrade();
        pointer_motion.connect_leave(move |_controller| {
            if let Some(entry) = entry_weak.upgrade() {
                entry.remove_css_class("pointer-hover");
            }
        });
        entry.add_controller(pointer_motion);
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
                build.entries.push(OpenTrayMenuEntry {
                    button: entry.clone(),
                    id: item.id,
                    ancestors: ancestors.to_vec(),
                    submenu: None,
                });
            }
            let command_tx = build.command_tx.clone();
            let key = build.menu.key.clone();
            let menu_path = build.menu.menu_path.clone();
            let top_popover = build.top_popover.clone();
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
            if item.enabled {
                build.entries.push(OpenTrayMenuEntry {
                    button: entry.clone(),
                    id: item.id,
                    ancestors: ancestors.to_vec(),
                    submenu: Some(sub_popover.clone()),
                });
            }
            build_menu_box(&sub_box, &item.children, build, &child_ancestors);
            sub_popover.set_child(Some(&sub_box));
            build.popovers.push(sub_popover.clone());
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

pub fn configure_layer_shell(
    window: &gtk4::ApplicationWindow,
    monitor_connector: Option<&str>,
) -> Result<()> {
    debug!("Configuring layer shell");

    window.init_layer_shell();
    if let Some(requested) = monitor_connector {
        let display = gtk4::prelude::WidgetExt::display(window);
        let monitors = display.monitors();
        let mut available = Vec::new();
        let mut selected = None;

        for index in 0..monitors.n_items() {
            let Some(object) = monitors.item(index) else {
                warn!(index, "GDK monitor list omitted an advertised item");
                continue;
            };
            let Ok(monitor) = object.downcast::<gdk::Monitor>() else {
                warn!(index, "GDK monitor list contained a non-monitor object");
                continue;
            };
            let Some(connector) = monitor.connector() else {
                warn!(index, "GDK monitor has no connector name");
                continue;
            };

            if connector == requested {
                selected = Some(monitor);
                break;
            }
            available.push(connector.to_string());
        }

        let Some(monitor) = selected else {
            available.sort();
            let available = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            bail!(
                "monitor connector {requested:?} was not found; available connectors: {available}"
            );
        };

        window.set_monitor(Some(&monitor));
        info!(monitor = requested, "Selected layer-shell monitor");
    }
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

    info!("Layer shell configured successfully");
    Ok(())
}

fn update_title_widget_workspace_color(
    title_widget: &TitleWidget,
    workspace_id: hyprland::shared::WorkspaceId,
) {
    // Get workspace color based on ID
    let color = get_workspace_color(workspace_id);

    // Apply color directly via CSS provider for immediate update
    let css_provider = gtk4::CssProvider::new();
    let css = format!(".title-widget {{ background-color: {}; }}", color);

    css_provider.load_from_data(&css);

    let style_context = title_widget.root.style_context();
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

    #[test]
    fn tray_level_zero_navigation_wraps() {
        assert_eq!(move_tray_index(0, 3, &NavCmd::Left), Some(2));
        assert_eq!(move_tray_index(2, 3, &NavCmd::Right), Some(0));
    }

    #[test]
    fn tray_level_zero_navigation_jumps_to_ends() {
        assert_eq!(move_tray_index(1, 3, &NavCmd::First), Some(0));
        assert_eq!(move_tray_index(1, 3, &NavCmd::Last), Some(2));
    }

    #[test]
    fn tray_level_zero_navigation_rejects_non_horizontal_commands() {
        assert_eq!(move_tray_index(0, 3, &NavCmd::Down), None);
        assert_eq!(move_tray_index(0, 0, &NavCmd::Right), None);
    }
}

// setup_*_updates are infallible now that there is no global sender to
// double-initialize — they only move a receiver into a glib-local drain task.

pub fn setup_workspace_updates(
    mut rx: mpsc::UnboundedReceiver<WorkspaceUpdate>,
    label: gtk4::Label,
    title_widget: TitleWidget,
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

fn desktop_app_class_match_score(app: &gtk4::gio::DesktopAppInfo, class: &str) -> u8 {
    if app
        .startup_wm_class()
        .is_some_and(|wm_class| wm_class.eq_ignore_ascii_case(class))
    {
        return 4;
    }

    if let Some(id) = app.id() {
        let desktop_id = id.strip_suffix(".desktop").unwrap_or(&id);
        if desktop_id.eq_ignore_ascii_case(class) {
            return 3;
        }
        if desktop_id
            .rsplit('.')
            .next()
            .is_some_and(|leaf| leaf.eq_ignore_ascii_case(class))
        {
            return 2;
        }
    }

    u8::from(app.name().eq_ignore_ascii_case(class))
}

fn desktop_icon_for_class(class: &str) -> Option<gtk4::gio::Icon> {
    gtk4::gio::AppInfo::all()
        .into_iter()
        .filter_map(|app| app.downcast::<gtk4::gio::DesktopAppInfo>().ok())
        .filter_map(|app| {
            let score = desktop_app_class_match_score(&app, class);
            (score > 0).then_some((score, app))
        })
        .max_by_key(|(score, _app)| *score)
        .and_then(|(_score, app)| app.icon())
}

fn update_title_icon(image: &gtk4::Image, class: &str) {
    let class = class.trim();
    if class.is_empty() {
        image.set_visible(false);
        return;
    }

    image.set_pixel_size(tray_icon_pixel_size(image));
    if let Some(icon) = desktop_icon_for_class(class) {
        image.set_from_gicon(&icon);
        image.set_visible(true);
        debug!(
            class,
            "Resolved title icon from desktop application metadata"
        );
        return;
    }

    let icon_theme = gtk4::IconTheme::for_display(&image.display());
    let lowercase = class.to_lowercase();
    let leaf = lowercase.rsplit('.').next().unwrap_or(&lowercase);
    for candidate in [class, lowercase.as_str(), leaf] {
        if icon_theme.has_icon(candidate) {
            image.set_icon_name(Some(candidate));
            image.set_visible(true);
            debug!(
                class,
                icon = candidate,
                "Resolved title icon directly from theme"
            );
            return;
        }
    }

    image.set_icon_name(Some("application-x-executable-symbolic"));
    image.set_visible(true);
    debug!(class, "Using generic fallback for title icon");
}

pub fn setup_title_updates(
    mut rx: mpsc::UnboundedReceiver<TitleUpdate>,
    title_widget: TitleWidget,
) {
    debug!("Setting up title updates");

    glib::spawn_future_local(async move {
        let mut current_class = String::new();
        while let Some(update) = rx.recv().await {
            debug!(
                title = update.title,
                class = update.class,
                "Updating title widget"
            );
            // NOTE: Title widget always remains visible even when empty, unlike battery/bluetooth widgets.
            // This provides consistent visual layout and shows the centered position in the bar.
            title_widget.label.set_text(&update.title);
            if update.class != current_class {
                update_title_icon(&title_widget.icon, &update.class);
                current_class = update.class;
            }
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

pub fn setup_network_updates(mut rx: mpsc::UnboundedReceiver<String>, label: gtk4::Label) {
    debug!("Setting up network updates");

    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            debug!("Updating network label: {}", update);
            label.set_text(&update);
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
