//! Stage manager - macOS-like single-window mode with a side card panel.
//!
//! While enabled, focusing a window minimizes all other non-exempt
//! windows, so the desktop only ever shows one window. A panel (see
//! `panel`) lists the background windows with thumbnails and allows
//! switching back via card clicks.
//!
//! Design doc: `docs/STAGE_MANAGER_DESIGN.md`. Upstream touch points are
//! intentionally minimal: behavior is driven from `on_window_focused()`,
//! `toggle()`, and the `StageRuntime` stored on `WmState`.
//!
//! # Platform-specific
//! - Windows: fully supported (panel thread, thumbnails via
//!   `PrintWindow`).
//! - Other platforms: unsupported and never built - this fork is
//!   Windows-only (macOS ships its own Stage Manager).

use std::{
  sync::{
    atomic::{AtomicBool, AtomicI64, AtomicIsize, Ordering},
    mpsc::Sender,
    Arc,
  },
  thread::JoinHandle,
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{error, info, warn};
use wm_common::{
  DisplayState, StageManagerConfig, WindowMatchConfig, WindowRuleEvent,
  WindowState, WmEvent,
};
use wm_platform::Dispatcher;

use crate::{
  commands::{
    container::set_focused_descendant,
    general::platform_sync,
    window::{run_window_rules, update_window_state},
    workspace::focus_workspace,
  },
  models::{NativeWindowProperties, WindowContainer, WorkspaceTarget},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub mod capture;

#[cfg(target_os = "windows")]
mod panel;

/// Window title of the panel, used to style its HWND after creation.
pub const PANEL_TITLE: &str = "GlazeWM Stage";

/// Max thumbnail dimension (px) for captures pushed to the panel.
pub const CAPTURE_MAX_DIM: u32 = 640;

/// Messages from the WM thread to the panel thread.
pub enum PanelMsg {
  /// Re-enumerate windows and rebuild the card model.
  Refresh,
  /// Fresh thumbnail for a window (captured while still visible).
  Snapshot {
    hwnd: isize,
    image: capture::CapturedImage,
  },
  /// Apply a new configuration (after a config reload).
  Reload(StageManagerConfig),
  /// Show/hide the panel window (e.g. while a fullscreen app has focus).
  SetVisible(bool),
  /// Ask the panel event loop to exit.
  Shutdown,
}

/// Messages from the panel thread to the WM event loop.
///
/// The platform window hook is installed with `WINEVENT_SKIPOWNPROCESS`,
/// so focus changes originating from the panel thread (which lives in the
/// WM process) never surface as `WindowEvent::Focused`. Card clicks
/// therefore report the activation explicitly so the WM can sync its
/// focus state and run the single-window sweep.
#[derive(Debug)]
pub enum PanelToWm {
  /// A card was clicked; the WM should focus-sync the target window and
  /// minimize the rest.
  CardActivated { hwnd: isize },
}

/// Owns the panel thread lifecycle; stored on `WmState.stage`.
///
/// The panel thread outlives enable/disable cycles: Slint's backend cannot
/// be re-initialized in a process once its event loop has quit, so
/// disabling only hides the panel window instead of tearing the thread
/// down. The thread is only joined on WM shutdown.
pub struct StageRuntime {
  active: bool,
  tx: Option<Sender<PanelMsg>>,
  quit_flag: Option<Arc<AtomicBool>>,
  join_handle: Option<JoinHandle<()>>,
  /// Panel -> WM event stream (polled by the main event loop).
  wm_tx: Option<UnboundedSender<PanelToWm>>,
  wm_inbox: Option<UnboundedReceiver<PanelToWm>>,
  /// Released to the panel after it has processed the last card click.
  ack_tx: Option<Sender<()>>,
  /// Target HWND of the last sweep (for duplicate suppression).
  last_sweep_hwnd: AtomicIsize,
  /// Unix-ms timestamp of the last sweep.
  last_sweep_ms: AtomicI64,
  /// Unix-ms timestamp of the last deferred-sweep notice (log throttle).
  last_skip_ms: AtomicI64,
  /// Target of the last sweep, armed to re-assert its foreground once if
  /// a just-minimized window steals it back (0 = never armed).
  sweep_target: AtomicIsize,
  /// One-shot permission for the foreground reclaim re-assertion.
  reclaim_armed: AtomicBool,
}

impl Default for StageRuntime {
  fn default() -> Self {
    let (wm_tx, wm_inbox) = tokio::sync::mpsc::unbounded_channel();

    Self {
      active: false,
      tx: None,
      quit_flag: None,
      join_handle: None,
      wm_tx: Some(wm_tx),
      wm_inbox: Some(wm_inbox),
      ack_tx: None,
      last_sweep_hwnd: AtomicIsize::new(0),
      last_sweep_ms: AtomicI64::new(0),
      last_skip_ms: AtomicI64::new(0),
      sweep_target: AtomicIsize::new(0),
      reclaim_armed: AtomicBool::new(false),
    }
  }
}

impl StageRuntime {
  /// Whether stage manager mode is currently enabled.
  pub fn is_active(&self) -> bool {
    self.active
  }

  /// Ensures the panel thread exists and shows the panel. No-op if already
  /// active.
  pub fn enable(
    &mut self,
    config: &StageManagerConfig,
    dispatcher: &Dispatcher,
  ) {
    #[cfg(target_os = "windows")]
    {
      if self.active {
        return;
      }

      if self.tx.is_none() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let quit_flag = Arc::new(AtomicBool::new(false));
        let quit_thread = Arc::clone(&quit_flag);
        let config = config.clone();
        let dispatcher = dispatcher.clone();
        let Some(wm_tx) = self.wm_tx.clone() else {
          error!("Stage manager inbox missing.");
          return;
        };

        let spawned = std::thread::Builder::new()
          .name("stage-panel".to_string())
          .spawn(move || {
            let result = std::panic::catch_unwind(
              std::panic::AssertUnwindSafe(|| {
                panel::panel_main(
                  config,
                  rx,
                  ack_rx,
                  &quit_thread,
                  dispatcher,
                  wm_tx,
                );
              }),
            );
            if result.is_err() {
              error!("Stage panel thread panicked.");
              quit_thread.store(true, Ordering::Relaxed);
            }
          });

        match spawned {
          Ok(join_handle) => {
            self.tx = Some(tx);
            self.ack_tx = Some(ack_tx);
            self.quit_flag = Some(quit_flag);
            self.join_handle = Some(join_handle);
          }
          Err(err) => {
            error!("Failed to spawn stage panel thread: {err}");
            return;
          }
        }
      } else {
        // Thread already exists from a previous cycle - push the current
        // configuration before re-showing the panel.
        self.send(PanelMsg::Reload(config.clone()));
      }

      self.active = true;
      self.send(PanelMsg::SetVisible(true));
      info!("Stage manager enabled.");
    }
  }

  /// Hides the panel and stops enforcing single-window mode. The panel
  /// thread stays alive so re-enabling is instant (Slint's backend cannot
  /// be re-initialized after its event loop has quit).
  pub fn disable(&mut self) {
    if !self.active {
      return;
    }
    self.active = false;

    self.send(PanelMsg::SetVisible(false));

    info!("Stage manager disabled.");
  }

  /// Sends a message to the panel thread (no-op when no thread exists).
  pub fn send(&self, msg: PanelMsg) {
    if let Some(tx) = &self.tx {
      if tx.send(msg).is_err() {
        warn!("Stage panel channel closed while stage manager is active.");
      }
    }
  }

  /// Releases the panel thread's wait after a card activation. Sent by
  /// `on_card_activated` once the WM has finished its sweep.
  fn release_card_ack(&self) {
    if let Some(ack_tx) = &self.ack_tx {
      let _ = ack_tx.send(());
    }
  }

  /// Whether a sweep targeting `hwnd` should run right now.
  ///
  /// Sweeps for the same target are suppressed for a short window to
  /// break focus ping-pong: while `platform_sync` re-asserts the WM's
  /// focus, the shell's alt-tab commit can restore the window the
  /// sweep just minimized, which re-triggers the sweep, and so on.
  pub fn should_sweep(&self, hwnd: isize) -> bool {
    let now_ms = Self::now_ms();

    let last_hwnd = self.last_sweep_hwnd.load(Ordering::Relaxed);
    let last_ms = self.last_sweep_ms.load(Ordering::Relaxed);

    if last_hwnd == hwnd && now_ms - last_ms < 300 {
      return false;
    }

    self.last_sweep_hwnd.store(hwnd, Ordering::Relaxed);
    self.last_sweep_ms.store(now_ms, Ordering::Relaxed);
    true
  }

  /// Throttles the deferred-sweep log notice to at most one per second.
  pub fn note_deferred_sweep(&self) -> bool {
    let now_ms = Self::now_ms();

    if now_ms - self.last_skip_ms.load(Ordering::Relaxed) < 1000 {
      return false;
    }

    self.last_skip_ms.store(now_ms, Ordering::Relaxed);
    true
  }

  /// Current Unix time in milliseconds (0 when the clock is broken).
  // Millisecond timestamps stay far below `i64::MAX` for any sane clock.
  #[allow(clippy::cast_possible_truncation)]
  fn now_ms() -> i64 {
    SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_or_default(|elapsed| elapsed.as_millis() as i64)
  }

  /// Arms the one-shot foreground-reclaim guard for a sweep of `hwnd`.
  ///
  /// Apps that get minimized by a sweep (Electron windows in
  /// particular) sometimes re-assert their own foreground right away,
  /// cancelling the minimize. The next focus event then re-runs the sweep
  /// against the *stealer*, minimizing the actual target instead. The
  /// guard makes the WM re-assert the sweep target exactly once.
  pub fn arm_foreground_reclaim(&self, hwnd: isize) {
    self.sweep_target.store(hwnd, Ordering::Relaxed);
    self.reclaim_armed.store(true, Ordering::Relaxed);
  }

  /// Consumes the foreground-reclaim guard.
  ///
  /// Returns `true` at most once per sweep, and only within a short grace
  /// period after the sweep ran (outside it, a focus change is genuine
  /// user intent and must not be overridden).
  pub fn take_foreground_reclaim(&self) -> bool {
    if Self::now_ms() - self.last_sweep_ms.load(Ordering::Relaxed) > 750 {
      return false;
    }

    self
      .reclaim_armed
      .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
      .is_ok()
  }

  /// The window the last sweep was run for (0 when never armed).
  pub fn sweep_target(&self) -> isize {
    self.sweep_target.load(Ordering::Relaxed)
  }

  /// Takes the panel -> WM inbox; called once by the main event loop to
  /// poll `PanelToWm` messages inside `tokio::select!`.
  ///
  /// A fresh (inert) channel pair is substituted on the second call so
  /// the receiver side never blocks on a missing inbox.
  pub fn take_inbox(&mut self) -> UnboundedReceiver<PanelToWm> {
    if let Some(inbox) = self.wm_inbox.take() {
      return inbox;
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    self.wm_tx = Some(tx);
    rx
  }

  /// Pushes a new configuration to the panel thread after a config reload.
  pub fn on_config_reload(&self, config: &StageManagerConfig) {
    if self.tx.is_some() {
      self.send(PanelMsg::Reload(config.clone()));
    }
  }

  /// Tears the panel thread down; only called on WM shutdown.
  pub fn shutdown(&mut self) {
    if let Some(quit_flag) = &self.quit_flag {
      quit_flag.store(true, Ordering::Relaxed);
    }
    if let Some(tx) = &self.tx {
      let _ = tx.send(PanelMsg::Shutdown);
    }
    self.tx = None;
    self.quit_flag = None;

    if let Some(join_handle) = self.join_handle.take() {
      let deadline = Instant::now() + Duration::from_millis(1000);
      while !join_handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
      }
      if !join_handle.is_finished() {
        warn!("Stage panel thread did not exit within 1s.");
      }
    }
  }
}

impl Drop for StageRuntime {
  fn drop(&mut self) {
    self.shutdown();
  }
}

/// Toggles stage manager mode on or off.
///
/// When enabling, an initial sweep minimizes all non-exempt windows except
/// the currently focused one. When disabling, minimized windows stay as-is
/// (deliberate decision - no state restoration).
pub fn toggle(state: &mut WmState, config: &UserConfig) {
  if state.stage.is_active() {
    state.stage.disable();
    return;
  }

  state
    .stage
    .enable(&config.value.stage_manager, &state.dispatcher);
  if !state.stage.is_active() {
    return;
  }

  if let Some(window) = state
    .focused_container()
    .and_then(|container| container.as_window_container().ok())
  {
    minimize_others(&window, state, config);
  }
  state.stage.send(PanelMsg::Refresh);
}

/// Hook invoked from `handle_window_focused` after a managed window gains
/// focus. Minimizes all other non-exempt windows.
pub fn on_window_focused(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) {
  if !state.stage.is_active() || state.is_paused {
    return;
  }

  // Focus can land on a minimized window before the OS has finished
  // restoring it (shell alt-tab commit) - or as an echo of the sweep's
  // own minimize. Sweeping such a target would minimize the last visible
  // window and leave the desktop empty, so the activation is deferred to
  // `on_window_unminimized`, which runs once the restore completes.
  if window.native().is_minimized().unwrap_or(false) {
    if state.stage.note_deferred_sweep() {
      info!("Stage sweep deferred: target {window} not visible yet.");
    }
    return;
  }

  // Foreground-reclaim guard: a window the last sweep minimized (some
  // Electron apps do this) stole the foreground back and cancelled its
  // minimize. Without re-asserting here, the sweep below would run
  // against the stealer and minimize the actual target instead.
  let sweep_target = state.stage.sweep_target();
  let hwnd = window.native().id().0;

  if sweep_target != 0
    && hwnd != sweep_target
    && state.stage.take_foreground_reclaim()
  {
    let target = state
      .windows()
      .into_iter()
      .find(|other| other.native().id().0 == sweep_target);

    if let Some(target) = target {
      let target_props = target.native_properties();
      let reassert = !target.native().is_minimized().unwrap_or(true)
        && !is_fullscreen(&target, state)
        && !is_exempt(
          window,
          &target_props,
          &config.value.stage_manager,
          state,
        );

      if reassert {
        info!(
          "Stage sweep: {window} reclaimed the foreground right after \
           the sweep - re-asserting {target}."
        );

        set_focused_descendant(&target.clone().into(), None);
        state.is_focus_synced = true;

        run_sweep(&target, state, config);
        state.stage.send(PanelMsg::Refresh);

        if let Err(err) = target.native().focus() {
          warn!("Stage manager failed to re-assert sweep target: {err}");
        }

        return;
      }
    }
  }

  let fullscreen = is_fullscreen(window, state);
  if config.value.stage_manager.hide_on_fullscreen {
    state.stage.send(PanelMsg::SetVisible(!fullscreen));
  }

  minimize_others(window, state, config);
  state.stage.send(PanelMsg::Refresh);
}

/// Hook invoked from `handle_window_minimize_ended` after a minimized
/// window has been restored.
///
/// Completes sweeps deferred by `on_window_focused`: the shell's alt-tab
/// commit can focus a minimized window before the restore has finished,
/// and the sweep runs here once the window is actually visible.
pub fn on_window_unminimized(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) {
  if !state.stage.is_active() || state.is_paused {
    return;
  }

  // Only the WM's focused window triggers the sweep - background
  // restores (e.g. the restore command without focus) keep the current
  // single-window layout untouched.
  let is_focused = state
    .focused_container()
    .and_then(|container| container.as_window_container().ok())
    .is_some_and(|focused| focused.id() == window.id());

  if is_focused {
    on_window_focused(window, state, config);
  }
}

/// Hook invoked from the main event loop when a panel card is clicked.
///
/// The panel thread lives inside the WM process, and the platform window
/// hook is installed with `WINEVENT_SKIPOWNPROCESS`, so the focus change
/// it triggers never arrives as `WindowEvent::Focused`. This function
/// mirrors the manual-focus path of `handle_window_focused`: it updates
/// the WM's focus state *before* the previous window gets minimized (so
/// the minimize handlers cannot steal focus back), then runs the
/// single-window sweep and releases the panel's ack so it can restore or
/// focus the target without ever showing two windows side by side.
pub fn on_card_activated(
  hwnd: isize,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  let Some(mut target) = state
    .windows()
    .into_iter()
    .find(|window| window.native().id().0 == hwnd)
  else {
    // Window unmanaged (or closed) - no ack, so the panel falls back to
    // restoring/focusing the window natively itself.
    state.stage.send(PanelMsg::Refresh);
    return Ok(());
  };

  state.pending_sync.queue_focused_effect_update();

  // Mirror `handle_window_minimize_ended`: a card click is the intent to
  // restore, so transition the model out of the minimized state upfront.
  // The panel performs no native restore itself - leaving the state stale
  // would let the sync below see a minimized model with a restored
  // window and re-minimize it (flash without switching).
  if target.state() == WindowState::Minimized {
    let target_state = target
      .prev_state()
      .unwrap_or(WindowState::default_from_config(&config.value));

    target = update_window_state(target, target_state, state, config)?;
  }

  if let Some(workspace) = target.workspace() {
    if target.display_state() == DisplayState::Hidden {
      focus_workspace(
        WorkspaceTarget::Name(workspace.config().name),
        state,
        config,
      )?;
    }

    // Update the WM's focus state before anything gets minimized.
    set_focused_descendant(&target.clone().into(), None);

    // Run window rules for focus events.
    run_window_rules(
      target.clone(),
      &WindowRuleEvent::Focus,
      state,
      config,
    )?;

    state.is_focus_synced = true;
    state.pending_sync.queue_workspace_to_reorder(workspace);

    // Broadcast the focus change event.
    state.emit_event(WmEvent::FocusChanged {
      focused_container: target.to_dto()?,
    });
  }

  // Single-window sweep: captures thumbnails while windows are still
  // visible, then minimizes them.
  minimize_others(&target, state, config);

  // Unblock the panel thread - the previous window is minimized by now
  // and the panel performs no native window actions itself.
  state.stage.release_card_ack();

  if !state.is_paused && state.pending_sync.has_changes() {
    // Restores the target via `SetWindowPlacement` (flicker-free), puts
    // it at its tiling slot and raises it in the z-order.
    platform_sync(state, config)?;
  }

  // Activate after the target is restored and positioned - focusing a
  // still-minimized window silently fails.
  if let Err(err) = target.native().focus() {
    warn!("Stage manager failed to focus card target: {err}");
  }

  state.stage.send(PanelMsg::Refresh);

  Ok(())
}

/// Minimizes every managed window except `window`, capturing a fresh
/// thumbnail of each while it is still visible (so cards never show a
/// stale or blank image).
///
/// Wraps the sweep guards (fullscreen exemption, duplicate-sweep
/// debounce) and arms the foreground-reclaim guard before running it.
fn minimize_others(
  window: &WindowContainer,
  state: &WmState,
  config: &UserConfig,
) {
  // A fullscreen focused window owns the screen - leave everything alone.
  if is_fullscreen(window, state) {
    info!("Stage sweep skipped: target {window} is fullscreen.");
    return;
  }

  // Duplicate sweeps for the same target within the debounce window are
  // echoes of the WM's own activation (shell commits, sync-focus);
  // re-running them makes windows ping-pong.
  if !state.stage.should_sweep(window.native().id().0) {
    info!("Stage sweep skipped: duplicate sweep for {window}.");
    return;
  }

  state.stage.arm_foreground_reclaim(window.native().id().0);
  run_sweep(window, state, config);
}

/// The minimize loop of a sweep - runs after `minimize_others` guards
/// pass. Also used by the foreground-reclaim guard to re-assert a sweep
/// target whose minimize was cancelled by the app stealing the foreground
/// back (deliberately bypasses the debounce and does not re-arm).
fn run_sweep(
  window: &WindowContainer,
  state: &WmState,
  config: &UserConfig,
) {
  let cfg = &config.value.stage_manager;

  info!("Stage sweep starting for target {window}.");
  let focused_props = window.native_properties();

  for other in state.windows() {
    if other.id() == window.id() {
      continue;
    }

    let native = other.native();
    if !native.is_valid() || native.is_minimized().unwrap_or(true) {
      continue;
    }
    drop(native);

    if is_exempt(&other, &focused_props, cfg, state) {
      continue;
    }

    #[cfg(target_os = "windows")]
    {
      let hwnd = {
        use wm_platform::NativeWindowWindowsExt;
        other.native().hwnd().0
      };
      // Capture while still visible - the thumbnail must not be taken
      // after minimize (would be black) nor skipped (would go stale).
      let started = std::time::Instant::now();
      let image = capture::capture_window(hwnd, CAPTURE_MAX_DIM);
      if started.elapsed().as_millis() > 50 {
        info!(
          "Stage sweep capture took {} ms for {other}.",
          started.elapsed().as_millis()
        );
      }
      if let Some(image) = image {
        state.stage.send(PanelMsg::Snapshot { hwnd, image });
      }
    }

    if let Err(err) = other.native().minimize() {
      warn!("Stage manager failed to minimize {other}: {err}");
    } else {
      info!("Stage sweep minimized {other}.");
    }
  }
}

/// Whether a window covers a whole monitor (WM fullscreen state or a
/// frame that spans the nearest monitor while not maximized).
///
/// Mirrors the heuristic in `manage_window::window_state_to_create`, but
/// relative to the monitor instead of the max workspace rect.
fn is_fullscreen(window: &WindowContainer, state: &WmState) -> bool {
  if matches!(window.state(), WindowState::Fullscreen(_)) {
    return true;
  }

  let props = window.native_properties();
  if props.is_maximized || props.is_minimized {
    return false;
  }

  state
    .nearest_monitor(&window.native())
    .and_then(|monitor| monitor.to_rect().ok())
    .is_some_and(|rect| props.frame.inset(1).contains_rect(&rect))
}

/// Whether a window is protected from single-window minimization.
///
/// Exemptions (each independently configurable): fullscreen windows,
/// windows sharing the focused window's process, windows sharing its exact
/// title, and anything matching the `whitelist` rules.
fn is_exempt(
  other: &WindowContainer,
  focused_props: &NativeWindowProperties,
  cfg: &StageManagerConfig,
  state: &WmState,
) -> bool {
  if cfg.exempt_fullscreen && is_fullscreen(other, state) {
    return true;
  }

  let props = other.native_properties();

  if cfg.exempt_same_process
    && !props.process_name.is_empty()
    && props.process_name == focused_props.process_name
  {
    return true;
  }

  if cfg.exempt_same_title
    && !props.title.trim().is_empty()
    && props.title == focused_props.title
  {
    return true;
  }

  cfg
    .whitelist
    .iter()
    .any(|match_config| matches_whitelist(&props, match_config))
}

/// Whether a window matches a whitelist entry (mirrors the logic in
/// `UserConfig::pending_window_rules`).
fn matches_whitelist(
  props: &NativeWindowProperties,
  match_config: &WindowMatchConfig,
) -> bool {
  let process_ok = match_config
    .window_process
    .as_ref()
    .is_none_or(|match_type| match_type.is_match(&props.process_name));

  #[cfg(target_os = "windows")]
  let class_ok = match_config
    .window_class
    .as_ref()
    .is_none_or(|match_type| match_type.is_match(&props.class_name));
  #[cfg(not(target_os = "windows"))]
  let class_ok = true;

  let title_ok = match_config
    .window_title
    .as_ref()
    .is_none_or(|match_type| match_type.is_match(&props.title));

  process_ok && class_ok && title_ok
}
