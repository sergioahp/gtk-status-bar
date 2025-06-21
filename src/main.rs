use gio::prelude::*;
use gtk::prelude::*;
use gtk::glib;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use chrono::Local;
use tokio::sync::mpsc;
use hyprland::shared::HyprDataActive;
use hyprland::event_listener::AsyncEventListener;
use hyprland::async_closure;
use std::sync::OnceLock;

// Global workspace update sender
static WORKSPACE_SENDER: OnceLock<mpsc::UnboundedSender<String>> = OnceLock::new();

fn create_workspace_widget() -> gtk::Label {
    let label = gtk::Label::new(Some("Workspace ?"));
    label.add_css_class("workspace-widget");
    label.set_halign(gtk::Align::Center);
    label
}

fn format_workspace_name_from_string(name: &str, id: hyprland::shared::WorkspaceId) -> String {
    if name.is_empty() {
        format!("Workspace {}", id)
    } else {
        format!("Workspace {}", name)
    }
}

fn format_workspace_name_from_type(name: &hyprland::shared::WorkspaceType, id: hyprland::shared::WorkspaceId) -> String {
    match name {
        hyprland::shared::WorkspaceType::Regular(name) => {
            if name.is_empty() {
                format!("Workspace {}", id)
            } else {
                format!("Workspace {}", name)
            }
        }
        hyprland::shared::WorkspaceType::Special(name_opt) => {
            match name_opt {
                Some(name) if !name.is_empty() => format!("Special: {}", name),
                _ => format!("Special {}", id),
            }
        }
    }
}

async fn hyprland_event_listener() -> hyprland::Result<()> {
    // Get initial workspace state
    if let Some(sender) = WORKSPACE_SENDER.get() {
        match hyprland::data::Workspace::get_active_async().await {
            Ok(workspace) => {
                let display_name = format_workspace_name_from_string(&workspace.name, workspace.id);
                let _ = sender.send(display_name);
            }
            Err(_) => {
                let _ = sender.send("Workspace ?".to_string());
            }
        }
    }
    
    // Set up event listener
    let mut event_listener = AsyncEventListener::new();
    
    event_listener.add_workspace_changed_handler(async_closure! {
        |workspace_data| {
            if let Some(sender) = WORKSPACE_SENDER.get() {
                let display_name = format_workspace_name_from_type(&workspace_data.name, workspace_data.id);
                let _ = sender.send(display_name);
            }
        }
    });
    
    // Start listening for events
    event_listener.start_listener_async().await?;
    
    Ok(())
}

fn setup_workspace_updates(label: gtk::Label) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    
    // Set the global sender
    if WORKSPACE_SENDER.set(tx).is_err() {
        eprintln!("Failed to set global workspace sender");
        return;
    }
    
    // Start the hyprland event listener
    tokio::spawn(async move {
        if let Err(e) = hyprland_event_listener().await {
            eprintln!("Hyprland event listener error: {}", e);
        }
    });
    
    // Bridge to GTK main thread
    glib::spawn_future_local(async move {
        while let Some(update) = rx.recv().await {
            label.set_text(&update);
        }
    });
}

fn create_title_widget() -> gtk::Label {
    let label = gtk::Label::new(Some("Application Title"));
    label.add_css_class("title-widget");
    label.set_halign(gtk::Align::End);
    label
}

fn create_time_widget() -> gtk::Label {
    let label = gtk::Label::new(Some(&get_current_time()));
    label.add_css_class("time-widget");
    label.set_halign(gtk::Align::End);
    label
}

fn get_current_time() -> String {
    Local::now().format("%H:%M").to_string()
}

fn update_time_widget(label: gtk::Label) {
    let label_weak = label.downgrade();
    glib::timeout_add_seconds_local(1, move || {
        if let Some(label) = label_weak.upgrade() {
            let time_str = get_current_time();
            label.set_text(&time_str);
            glib::ControlFlow::Continue
        } else {
            glib::ControlFlow::Break
        }
    });
}

fn create_bt_widget() -> gtk::Label {
    let label = gtk::Label::new(Some("No BT"));
    label.add_css_class("bt-widget");
    label.set_halign(gtk::Align::End);
    label
}

fn create_experimental_bar() -> (gtk::Box, gtk::Label, gtk::Label) {
    let main_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    main_box.set_hexpand(true);
    main_box.set_valign(gtk::Align::Center);

    // Left group - workspace
    let left_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    left_group.add_css_class("left-group");
    left_group.set_valign(gtk::Align::Start);
    left_group.set_hexpand(false);
    
    let workspace_widget = create_workspace_widget();
    left_group.append(&workspace_widget);

    // Center group - title with spacers
    let center_spacer_start = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    center_spacer_start.set_hexpand(true);

    let center_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    center_group.add_css_class("center-group");
    center_group.set_valign(gtk::Align::Center);
    center_group.set_hexpand(false);
    center_group.append(&create_title_widget());

    let center_spacer_end = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    center_spacer_end.set_hexpand(true);

    // Right group - systray, bt, time
    let right_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    right_group.add_css_class("right-group");
    right_group.set_hexpand(false);
    right_group.set_valign(gtk::Align::End);
    right_group.append(&create_bt_widget());
    
    let time_widget = create_time_widget();
    right_group.append(&time_widget);

    // Assemble main box
    main_box.append(&left_group);
    main_box.append(&center_spacer_start);
    main_box.append(&center_group);
    main_box.append(&center_spacer_end);
    main_box.append(&right_group);

    (main_box, time_widget, workspace_widget)
}

fn activate(application: &gtk::Application) {
    let window = gtk::ApplicationWindow::new(application);
    window.add_css_class("layer-bar");

    // Load CSS
    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_path("style.css");
    // Provide our CSS at USER priority so it overrides theme and application providers
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&window),
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );
    window.init_layer_shell();
    window.set_layer(Layer::Bottom);
    window.auto_exclusive_zone_enable();

    // Set anchors for top bar
    let anchors = [
        (Edge::Left, true),
        (Edge::Right, true),
        (Edge::Top, true),
        (Edge::Bottom, false),
    ];

    for (anchor, state) in anchors {
        window.set_anchor(anchor, state);
    }

    // Set height to 2% of screen (similar to eww config)
    window.set_default_height(30);

    let (bar, time_widget, workspace_widget) = create_experimental_bar();
    window.set_child(Some(&bar));
    window.show();

    // Start time update timer
    update_time_widget(time_widget);
    
    // Start workspace updates
    setup_workspace_updates(workspace_widget);
}

fn main() {
    // Initialize tokio runtime for async tasks
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    
    let application = gtk::Application::new(Some("sh.wmww.gtk-layer-example"), Default::default());

    application.connect_activate(|app| {
        activate(app);
    });

    application.run();
}
