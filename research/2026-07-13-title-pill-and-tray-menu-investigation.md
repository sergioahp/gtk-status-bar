# Title pill centering, tray menu hover flash, and tray nav latency

Date: 2026-07-13

This is a retrospective of three related investigations done in one session,
spanning the title widget and the system tray dropdown. Two produced fixes on
dedicated branches (not yet merged); one (title centering) is already on
master. Written up because the dead ends and the reasoning behind the final
choices are easy to lose once a branch is merged and the chat history scrolls
away.

## 1. Title pill centering (master, 3 commits)

The title pill went through three passes before it centered correctly in all
cases:

- `f68e304` "fix: center title pill contents" - made the title widget's
  `GtkCenterBox` treat the icon+label pair as a single center child. Fixed the
  case where the pill was left-aligned instead of centered, but the icon's
  width still pulled the visual center of the label off to one side whenever
  an icon was present, worst on short titles.
- `c108b77` "fix: center title label independent of icon" - the actual fix for
  that: made the icon the `CenterBox`'s *start* widget and the label alone its
  *center* widget. `GtkCenterLayout` keeps the center child truly centered
  regardless of what's in the start/end slots, so the title text now stays
  balanced whether or not an icon is showing, instead of the icon+label group
  being centered as one unit.
- `6e59b7b` "fix: center title pill on monitor" - a separate bug: the pill
  itself (not just its contents) drifted off the monitor's true center once
  side pills had asymmetric widths, because the bar's outer layout used two
  expanding spacer `Box`es around the title, which only balance when both
  sides are equal width. Replaced with the bar's own `GtkCenterBox`
  (`start_widget`/`center_widget`/`end_widget` = left group / title / right
  group), which centers the title independent of side-group width.

Two independent-looking "off center" bug reports turned out to be two
different layers of the same CenterBox pattern: center the *content* within
its container, then separately center that *container* within the bar.

## 2. Tray menu hover flash (branch `fix/tray-menu-hover-flash`, not merged)

Reported symptom: opening a tray dropdown via keyboard or the `trayctl`
socket (not the mouse) showed a faint white highlight on the *last* item in
the menu, before any navigation happened and with the real mouse cursor
nowhere near the popup.

### Reproduction and root cause

Reproduced with `trayctl keyboard-menu <index>` immediately followed by a
screenshot. Confirmed the highlight was genuinely `.tray-menu-item:hover`
(not `.selected`) by pixel-matching: the highlighted row's RGB
`(52, 56, 70)` is exactly `rgba(255, 255, 255, 0.10)` (the CSS hover rule)
composited over the menu's `rgba(30, 34, 50, 0.96)` background. Reproduced
identically across two different tray icons with different menu lengths
(always the *last* built button), with the real hardware cursor's position
(`hyprctl cursorpos`) checked and nowhere near the popup's actual screen
rectangle - so it was not a real pointer entering the surface.

Root cause: `build_menu_box` constructs a fresh `gtk4::Button` per menu item
every time a dropdown opens. When GTK lays out a new/changed widget tree
under a stationary pointer, it re-picks which widget is "under" the pointer
and can assign `GTK_STATE_FLAG_PRELIGHT` (hover) to whichever widget now
occupies that position - a widget-tree-relative pick, not a real Wayland
`wl_pointer.enter` for the popup surface. Since the buttons are rebuilt every
time, that stale pick consistently lands on the last-built button.

### First fix (commit `eb8c50e`): clear it after the fact

Added `clear_menu_item_hover()`, called via `unset_state_flags(PRELIGHT)` on
every menu-item button right after each `popover.popup()` call (mirrors an
existing, older fix for the same class of stray-PRELIGHT problem on the tray
*icon* buttons in `end_nav`). Verified live: pixel scan of the previously
tainted `(52, 56, 70)` region now reads as plain background.

This worked, but treats the symptom (clear the flag after GTK sets it)
rather than the mechanism.

### Refinement (commit `094e8cf`): stop it from ever being set

Investigated further and found the precise mechanism: `GtkCenterBox`/popover
allocation makes GDK emit a synthetic motion event to re-pick pointer focus
after a layout change, and that synthetic event carries `GDK_CURRENT_TIME`
(timestamp zero) instead of a real device timestamp. CSS `:hover` has no way
to distinguish a synthetic re-pick from genuine pointer motion.

Replaced the CSS `:hover` rule with an explicit `.pointer-hover` class,
toggled by a `GtkEventControllerMotion` added to each menu-item button. The
motion handler checks `controller.current_event_time()` and ignores events
carrying `GDK_CURRENT_TIME`; only genuine, timestamped motion adds the class.
Since nothing spurious can set the state anymore, the defensive
clear-after-popup call sites and the `clear_menu_item_hover` helper were
removed entirely.

Re-verified live with the same reproduction steps (pixel scan across the
full popup region) - clean. This is the version left on the branch.

## 3. Tray menu keyboard-nav latency (branch `fix/tray-menu-nav-latency`, pushed)

Starting point: `show_tray_menu`'s keyboard/socket-driven path wrapped
`popover.popup()` in `glib::timeout_add_local_once(Duration::from_millis(100),
...)`, with a comment about letting "the exclusive helper surface... settle
keyboard focus before the popover maps." This added a fixed, noticeable delay
to every keyboard/socket tray-menu open in production, apparently added
while working on the VM test harness rather than for a real production need.

Asked for: remove the blanket sleep from production code, find and fix the
actual race instead of masking it with a timer, and only keep a settle delay
if the VM/nixos test harness genuinely needs one (scoped to test config, not
shipped code).

### Attempt 1: gate on `GtkWindow::is-active` - failed on real hardware

First implementation moved the `KeyboardGrab` helper window from
`Layer::Bottom` to `Layer::Overlay` (layer-shell only guarantees exclusive
keyboard focus on the top/overlay layers), and replaced the fixed timer with
a `NavCmd::GrabAcquired` event sent from a `window.connect_is_active_notify`
handler, gated the initial dropdown-open on that signal, and added a 3-second
`NavCmd::GrabTimeout` as a bounded failure path (not a "sleep and hope", an
actual abort-cleanly-on-failure). Logic reviewed carefully (stale/duplicate
signal guards via `request_id` + a `grab_acquired` dedup flag, correct
re-snapshotting of the currently-selected icon across the async gap) and
looked sound; local build, 64 tests, clippy, and fmt all passed.

It failed live. Stopped the real systemd-managed bar, ran the build directly
with `RUST_LOG=debug`, drove it with `trayctl keyboard-menu`, and got this on
real Hyprland (not the VM), reproduced twice identically:

```
23:05:01.582 INFO  Tray menu requested exclusive keyboard focus request_id=1
23:05:04.582 WARN  Keyboard grab was not confirmed before timeout request_id=1
23:05:04.583 INFO  Tray menu released keyboard focus request_id=1
```

`GtkWindow::is-active` never fired true for this layer-shell surface, so
every keyboard/socket tray-menu-open hit the 3-second failure path and
aborted - a full regression, worse than the original 100ms tax. Likely
explanation: `is-active` tracking is wired to `xdg_toplevel` activation
state, and layer-shell surfaces use `zwlr_layer_surface_v1`, which has no
such state at all - there may simply be no GTK-level signal for "this
layer-shell surface has compositor-granted keyboard focus."

### Cross-check: how does rofi handle this?

Spawned a research subagent to clone rofi (a mature, widely-used wlr-layer-
shell client with the same "grab exclusive keyboard focus for a launcher UI"
requirement) and check whether it has solved this more robustly. Findings:

- rofi does not use GTK for its Wayland surface at all - raw
  `wayland-client` + the `wlr-layer-shell-unstable-v1` protocol directly,
  GLib only for the event loop.
- It sets `zwlr_layer_surface_v1_set_keyboard_interactivity(1)`, same
  declarative request as our `KeyboardMode::Exclusive`.
- Its post-surface-creation `wl_display_roundtrip()` only drives the
  layer-shell configure/ack-configure handshake (surface sizing) - nothing to
  do with keyboard focus. Confirms the protocol itself has no
  focus-granted signal.
- rofi does not wait for any focus confirmation either. Its
  `wayland_display_set_input_focus` / `revert_input_focus` are empty no-ops
  on the Wayland backend; key events are just processed whenever
  `wl_keyboard_listener` delivers them.
- One partial (and inapplicable-to-us) mitigation: `wl_keyboard.enter`
  carries the array of keys already held down at the moment focus arrives,
  and rofi replays those as presses - this only helps keys still held down
  when focus lands, not a fast tap-and-release sent beforehand.
- No commit or issue in rofi's history discusses this race; it is an
  accepted, unaddressed limitation there too.

This confirmed attempt 1's gate was chasing a signal that structurally does
not exist for this protocol, not a bug specific to our GTK bindings.

### Attempt 2 (final, commit `020cbf2`): do not wait at all

Removed the `is-active` gate and the `GrabAcquired`/`GrabTimeout` machinery
entirely. `show_tray_menu` now calls `popover.popup()` and `box_.grab_focus()`
unconditionally and immediately - no timer, no gate, in either the
keyboard/socket path or the mouse path.

Verified live on real Hyprland using `wtype` to synthesize real keypresses
right after `trayctl keyboard-menu`:

- Realistic timing (small delays between commands, close to what a human or
  a separate script invocation would naturally have): dropdown opened in
  ~15-30ms, injected keys (`j`, `Right`, `q`) were received and processed
  correctly every time.
- Zero-delay synthetic key injected immediately after `trayctl` returned was
  *not* logged at all (silently dropped by the compositor before focus
  landed) - but the very next key, sent 56ms later, worked fine and the
  session recovered normally. A 10ms gap before the first synthetic key was
  already enough to avoid the drop.

So: focus delivery is genuinely asynchronous, there is no GTK/protocol signal
to wait on (matches the rofi finding), but real usage (human reaction time,
or the natural overhead of a separate `trayctl` process invocation) is always
slower than the ~15-40ms handoff window observed on real hardware. The only
residual risk is a synthetic keystroke sent with near-zero delay, which is
silently dropped without corrupting any state or hanging the session - a
better failure mode than either the old fixed 100ms tax or the broken
`is-active` gate's 3-second abort.

Documented this trade-off directly in the code (a comment in
`KeyboardGrab::acquire`, right after `window.present()`), since it is exactly
the kind of non-obvious constraint that would otherwise only live in chat
history.

### VM harness

The nixos VM test drives nav via the same synthetic-input mechanism, so it
can hit the identical race under nested-virtualization timing (plausibly
worse than real hardware's ~15-40ms). Added a 1-second settle
(`machine.sleep(1)`; the test driver's typed API only accepts whole-second
sleeps, an intermediate `0.1` attempt failed for that reason) between
triggering nav and sending the first synthetic key, in the VM test script
only - the shipped application stays at zero added delay. This is the
"scoped to test config, not production code" ask from the original request.

Full VM suite (keyboard navigation, two-stage close, Escape, close-menus,
click-away, Enter activation, stale-open race) passed in 291 seconds.
Committed as `020cbf2` "fix: remove tray navigation popup delay" and pushed
to `origin/fix/tray-menu-nav-latency`. Not merged, no home-manager switch, as
instructed.

## 4. Process notes: driving a second agent over tmux

Sol (Codex CLI, `gpt-5.6-sol` at high reasoning effort) did the implementation
work for the nav-latency fix in its own git worktree/branch, driven by
sending it messages over a shared tmux session and reading its pane, with
Claude Code doing an independent review-and-live-test gate before each VM
verification pass. Two things worth remembering for next time:

- **`tmux send-keys -l` with embedded literal newlines can silently get
  stuck.** A multi-paragraph message sent this way displayed correctly in
  Sol's compose box but never actually submitted - each embedded `\n` byte
  was delivered as a raw keystroke rather than as part of a recognized paste,
  so the TUI just kept inserting newlines instead of treating it as "paste,
  then submit on next real Enter." Two minutes of silence (no "Working"
  indicator, no file changes) confirmed it was stuck rather than "thinking."
  Fix: `tmux load-buffer` + `tmux paste-buffer -p` (the `-p` flag wraps the
  content in a bracketed-paste sequence), which the TUI correctly recognized
  as a single paste, followed by a normal `Enter` keystroke to submit. Worked
  immediately.
- **Event-driven monitoring beats manually polling `tmux capture-pane` in a
  sleep loop.** Switched from a backgrounded `until`-loop poll to the
  `Monitor` tool: a persistent background command (git-diff-hash + keyword
  grep over the pane, both deduped) whose stdout lines become individual
  chat notifications. This meant genuinely waiting for events instead of
  guessing a poll interval, and made it easy to catch meaningful moments
  (diffs appearing, build failures, the "ready for review" ping) without
  spending turns on manual re-checks. For a cleaner signal next time: have
  the other agent (or a `tmux pipe-pane` redirect) write plain, line-oriented
  progress markers to a file, rather than relying on grep over a redrawn
  TUI's scrollback.

## Branches from this session

- `master`: title pill centering (`f68e304`, `c108b77`, `6e59b7b`), already
  in.
- `fix/tray-menu-hover-flash` (pushed, not merged): phantom hover on the last
  menu item when opening a dropdown via keyboard/socket.
- `fix/tray-menu-nav-latency` (pushed, not merged): removes the 100ms
  artificial delay on tray dropdown opens; VM-verified.
