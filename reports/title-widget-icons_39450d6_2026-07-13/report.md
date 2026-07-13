# Title widget application icon evidence

Date: 2026-07-13

Source revision: `39450d61fd4999db7840a080bb1ae5f607023ae6`

Evidence harness revision: `0de732e7b42343ddbb3c064438019f32b7ff5d07`

## Result

- PASS: the focused application's icon is rendered on the left side of the
  title text.
- PASS: the icon and text share one uninterrupted title-pill background,
  padding, and workspace color.
- PASS: Hyprland window classes are carried with title updates and resolved
  through desktop application metadata. The VM resolved both `kitty` and
  `claude-desktop`.
- PASS: title-only changes for the same application reuse the current icon.
- PASS: an empty active-window update hides the icon while retaining the title
  widget's established layout.
- PASS: the bar stayed 25 pixels tall for the initial, long-title, tray, menu,
  and tray-removal states.
- PASS: the complete NixOS/Hyprland VM regression test finished successfully in
  289.62 seconds.

## Visual evidence

| File | Demonstrates |
| --- | --- |
| `03-kitty-title-closeup.png` | Kitty icon on the left, inside the same blue pill as `admin@machine: ~`. |
| `11-title-short-closeup.png` | The icon remains inside the pill when the title changes to `hello`. |
| `03-kitty-title.png` | Full-monitor runtime context for the initial Kitty title. |
| `11-title-short.png` | Full-monitor runtime context for a short dynamic title. |
| `15-focus-left.png` | Title follows focus to the window named `Focus Left`. |
| `16-focus-right.png` | Title follows focus to the window named `Focus Right`. |

The close-ups are independent crops of their corresponding full-monitor
captures. No screenshots were combined.

## Runtime evidence

The full journal records the class flowing from Hyprland into the widget and the
desktop metadata match:

```text
Active window changed title="kitty" class="kitty"
Updating title widget title="kitty" class="kitty"
Resolved title icon from desktop application metadata class="kitty"
Active window changed title="Claude" class="claude-desktop"
Updating title widget title="Claude" class="claude-desktop"
Resolved title icon from desktop application metadata class="claude-desktop"
```

`bar-geometry.txt` records:

```text
initial=(0, 0, 1920, 25)
title=(0, 0, 1920, 25)
tray=(0, 0, 1920, 25)
menu=(0, 0, 1920, 25)
removed=(0, 0, 1920, 25)
```

`gtk-status-bar-status.txt` records the packaged bar running under the user
service. `gtk-status-bar-journal-full.log` contains the complete structured
runtime log.

## Verification commands

```sh
nix develop -c cargo fmt
nix develop -c cargo test
nix develop -c cargo clippy --workspace --all-targets
nix build .#checks.x86_64-linux.gtk-status-bar-vm -L \
  --out-link result-title-icons_2026-07-13
```

The local unit suite passed 58 tests. The pinned release package passed 58
application tests, 3 tray IPC tests, and 2 trayctl tests before the VM test ran.
Clippy completed with the repository's pre-existing warnings in `dbus.rs`,
`pw.rs`, and the existing test-module placement in `widgets.rs`.

