# Stage Manager (macOS-style single-window mode) — Implementation Design

Fork of GlazeWM v3.10.1. All new behavior lives under the new `stage_manager` config
section and `packages/wm/src/stage_manager/`. Original behavior is untouched when the
feature is off (and the added config section is ignored by the official build).

## Config (`stage_manager:` top-level section in config.yaml)

| key | type | default | meaning |
| --- | --- | --- | --- |
| `enabled` | bool | false | auto-enable at WM startup |
| `side` | `left`/`right` | right | panel dock side (primary monitor only) |
| `size` | `small`/`medium`/`large` | medium | panel/card size preset |
| `opacity` | 0..1 | 0.85 | card background opacity |
| `spacing` | int px | 10 | gap between cards (logical px) |
| `corner_radius` | float px | 12 | card corner radius |
| `hide_on_fullscreen` | bool | true | hide panel while focused window is fullscreen |
| `exempt_same_process` | bool | true | don't minimize windows sharing the focused window's process |
| `exempt_same_title` | bool | true | don't minimize windows with identical title |
| `exempt_fullscreen` | bool | true | never minimize fullscreen windows |
| `hover_refresh_ms` | int | 150 | live thumbnail refresh interval on hover |
| `whitelist` | list of window matches | [] | never minimize matching apps |

Hotkey: new `toggle-stage-manager` command usable anywhere other commands are
(`keybindings` section, `glazewm --command toggle-stage-manager`, IPC).

## Architecture

- One binary. Panel runs as a Slint UI on a dedicated thread inside `glazewm.exe`.
- `StageRuntime` lives in `WmState` (`pub stage`): spawn/kill + `std::sync::mpsc`
  channel `PanelMsg { Refresh, Snapshot{hwnd,w,h,rgba}, Reload(cfg), SetVisible(bool), Shutdown }`.
- Engine hook in `handle_window_focused` (end of the found-window branch):
  on focus change to window B, sweep all other managed windows:
  skip minimized / exempt (fullscreen / same process / same title / whitelist);
  for each victim A (still visible at this instant): `PrintWindow` capture ->
  send `Snapshot` to panel -> `native().minimize()`. Then `Refresh`.
- Panel thread: 100 ms `slint::Timer` drains rx, re-enumerates taskbar-style windows
  every 300 ms or on `Refresh` (visible_windows minus: own pid, owner windows,
  tool windows, cloaked, desktop, untitled, focused window). Card click ->
  `NativeWindow::from_handle`: `restore()` if minimized, then `focus()` —
  the ensuing Focused event makes the engine minimize the previous window,
  capturing its fresh thumbnail first (no two windows share the screen).
- Panel window: frameless, transparent, always-on-top; after show, styled via
  `FindWindowW("GlazeWM Stage")`: `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` + TOPMOST.
  `check_is_manageable` gains an own-process guard so the panel can never be tiled.
- Thumbnails: `PrintWindow(PW_RENDERFULLCONTENT)` + DWM extended-frame crop +
  blank detection + box downscale to <= 640 px. Fresh snapshot taken at minimize
  time (window still visible); hover on a visible card re-captures live.
- Toggle off: engine stops; minimized windows stay as-is (per user decision).
- `stage_manager` config reload -> `PanelMsg::Reload` (visuals only, no restart).

## Geometry (logical px)

card width: small 160 / medium 216 / large 280; card height = w*9/16 + 26 (title bar);
panel width = card + 24 padding; height = 24 + n*card_h + (n-1)*spacing clamped;
x = display edge -/+ 8; y = top + 12. Primary display only.

## Touched upstream files (minimal, additive)

1. `packages/wm-common/src/parsed_config.rs` — `StageManagerConfig` + field.
2. `packages/wm-common/src/app_command.rs` — `ToggleStageManager` variant.
3. `packages/wm/src/main.rs` — `mod stage_manager;` + startup auto-enable.
4. `packages/wm/src/wm.rs` — command arm (after `WmTogglePause`).
5. `packages/wm/src/wm_state.rs` — `pub stage` field + Drop shutdown.
6. `packages/wm/src/events/handle_window_focused.rs` — one hook line.
7. `packages/wm/src/commands/common/reload_config.rs` — one notify line.
8. `packages/wm/src/commands/window/manage_window.rs` — own-process guard.
9. `packages/wm/Cargo.toml` + `build.rs` — slint/windows deps.
