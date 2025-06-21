# 🚀 GTK Status Bar

A modern, transparent status bar for Wayland compositors built directly in Rust with GTK4 and layer-shell protocol. Designed for Hyprland with real-time workspace tracking and async event handling.

## ✨ Features

- **🎯 Direct GTK4 implementation** - No middleware like eww, built directly on GTK
- **⚡ Async workspace monitoring** - Real-time Hyprland workspace updates using tokio
- **🐧 Modern Linux desktop** - Native Wayland layer-shell protocol support
- **🌟 Transparency support** - Clean, minimal aesthetic with transparent background
- **🔒 Thread-safe architecture** - Proper async/sync bridge between Hyprland events and GTK main thread
- **🔥 Blazingly fast** - Zero-cost abstractions and efficient Rust performance

## 📦 Components

- 🖥️ Live workspace display with custom name support
- ⏰ Real-time clock updates
- 📱 Bluetooth device status placeholder
- 🧩 Extensible widget architecture

## 🛠️ Technology Stack

- **🦀 Rust** - Memory-safe systems programming
- **🎨 GTK4** - Modern Linux desktop UI toolkit
- **🌊 Layer Shell** - Wayland compositor integration
- **⚡ Tokio** - Async runtime for event handling
- **🎮 Hyprland-rs** - Native Hyprland API bindings

## 🚀 Usage

This is a personal status bar tailored to my specific workflow and aesthetic preferences. You're welcome to:

- 🍴 Fork this project for your own customizations
- 📖 Use it as a reference for building GTK layer-shell applications
- 🔄 Adapt the async patterns for other Wayland/GTK projects
- 🎓 Study the implementation for learning purposes

**📝 Note:** This project is highly specific to my use case and desktop setup. While you're encouraged to fork and modify it, I likely won't accept contributions as the design decisions are very personal and opinionated.

## 🔨 Building

### With Nix Flakes (Recommended)

```bash
nix develop
cargo build --release
```

### Traditional Cargo

```bash
cargo build --release
```

Requires GTK4, layer-shell protocol support, and a Wayland compositor (tested with Hyprland).