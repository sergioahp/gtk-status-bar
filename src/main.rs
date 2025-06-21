use gio::prelude::*;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};

fn create_workspace_widget() -> gtk::Label {
    let label = gtk::Label::new(Some("Workspace 1"));
    label.add_css_class("workspace-widget");
    label.set_halign(gtk::Align::Center);
    label
}

fn create_title_widget() -> gtk::Label {
    let label = gtk::Label::new(Some("Application Title"));
    label.add_css_class("title-widget");
    label.set_halign(gtk::Align::End);
    label
}

fn create_time_widget() -> gtk::Label {
    let label = gtk::Label::new(Some("12:00"));
    label.add_css_class("time-widget");
    label.set_halign(gtk::Align::End);
    label
}

fn create_bt_widget() -> gtk::Label {
    let label = gtk::Label::new(Some("No BT"));
    label.add_css_class("bt-widget");
    label.set_halign(gtk::Align::End);
    label
}

fn create_experimental_bar() -> gtk::Box {
    let main_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    main_box.set_hexpand(true);
    main_box.set_valign(gtk::Align::Center);

    // Left group - workspace
    let left_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    left_group.add_css_class("left-group");
    left_group.set_valign(gtk::Align::Start);
    left_group.set_hexpand(false);
    left_group.append(&create_workspace_widget());

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
    right_group.append(&create_time_widget());

    // Assemble main box
    main_box.append(&left_group);
    main_box.append(&center_spacer_start);
    main_box.append(&center_group);
    main_box.append(&center_spacer_end);
    main_box.append(&right_group);

    main_box
}

fn activate(application: &gtk::Application) {
    let window = gtk::ApplicationWindow::new(application);
    window.add_css_class("layer-bar");

    // Load CSS
    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_path("style.css");
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&window),
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
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

    let bar = create_experimental_bar();
    window.set_child(Some(&bar));
    window.show()
}

fn main() {
    let application = gtk::Application::new(Some("sh.wmww.gtk-layer-example"), Default::default());

    application.connect_activate(|app| {
        activate(app);
    });

    application.run();
}
