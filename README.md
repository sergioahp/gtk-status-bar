# 🚀 GTK Status Bar

A modern, transparent status bar for Wayland compositors built directly in Rust with GTK4 and layer-shell protocol. Designed for Hyprland with real-time workspace tracking, PipeWire audio monitoring, and async event handling.

## 📸 Screenshot

![GTK Status Bar](assets/bar.png)

## ✨ Features

- **🎯 Direct GTK4 implementation** - No middleware like eww, built directly on GTK
- **⚡ No polling design** - Event-driven architecture for blazingly fast performance
- **🎵 Default audio device focus** - PipeWire integration that tracks only the system's default sink
- **🎨 Workspace color coding** - Title widget background changes color based on current workspace
- **📱 Multiple Bluetooth devices** - Shows connected mice, speakers, earbuds with battery info via D-Bus monitoring (TODO: verify multiple device support)
- **🔋 Smart widget visibility** - Battery/Bluetooth widgets hide when no data, title always visible for centering
- **🐧 Native Wayland support** - Layer-shell protocol with Hyprland integration, future DE support planned
- **🌟 Snappy, colorful, transparent** - Clean aesthetic with responsive visual feedback
- **🔒 Thread-safe architecture** - Proper async/sync bridge between system events and GTK main thread
- **🎨 CSS customization** - External CSS file support for complete visual customization
- **📡 D-Bus integration** - Direct system service communication for real-time updates
- **🧰 System tray host** - StatusNotifierItem support for Electron, OBS, Fcitx, and other tray applications
- **🔧 Resilient error handling** - Uses anyhow for graceful degradation and continuous operation
- **📝 Extensive logging** - Comprehensive tracing throughout the application for debugging

## 📦 Components

- 🖥️ Live workspace display with custom name support
- ⏰ Real-time clock with 12-hour format
- 🎵 PipeWire volume monitoring with compact display format
- 📱 Bluetooth device status with battery levels
- 🔋 System battery status with automatic hiding
- 🧰 Clickable system tray with icon theme, file icon, and ARGB pixmap support
- 🧩 Extensible widget architecture with centered layout

System tray controls follow the StatusNotifierItem convention: left click activates an application, middle click performs its secondary action, and right click opens its context menu. Menu-only items open their menu on left click as well.

## 🛠️ Technology Stack

- **🦀 Rust** - Memory-safe systems programming with anyhow error handling
- **🎨 GTK4** - Modern Linux desktop UI toolkit with CSS styling
- **🌊 Layer Shell** - Wayland compositor integration
- **⚡ Tokio** - Async runtime with sophisticated thread management
- **🎮 Hyprland-rs** - Native Hyprland API bindings
- **🎵 PipeWire** - Modern Linux audio system integration
- **🚌 D-Bus** - System service communication for Bluetooth and battery
- **📝 Tracing** - Comprehensive structured logging system

## 🚀 Usage

This is a personal status bar tailored to my specific workflow and aesthetic preferences. You're welcome to:

- 🍴 Fork this project for your own customizations
- 📖 Use it as a reference for building GTK layer-shell applications
- 🔄 Adapt the async patterns for other Wayland/GTK projects
- 🎓 Study the implementation for learning purposes

**📝 Note:** This project is highly specific to my use case and desktop setup. While you're encouraged to fork and modify it, I likely won't accept contributions as the design decisions are very personal and opinionated.

## 🔨 Building & Running

### With Nix Flakes (Recommended)

```bash
# Run directly
nix run

# Or build and run manually
nix develop
cargo build --release
./target/release/gtk-status-bar
```

### Traditional Cargo

```bash
cargo build --release
./target/release/gtk-status-bar
```

Requires GTK4, layer-shell protocol support, and a Wayland compositor (tested with Hyprland).

## 📄 License

MIT License - see [LICENSE](LICENSE) for details.
