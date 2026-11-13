//! Stage manager card panel — runs the Slint UI on a dedicated thread.
//!
//! The panel is a frameless, transparent, always-on-top window docked to
//! the left or right side of the primary monitor. It lists all taskbar-style
//! windows except the focused one, with a thumbnail per card.
//!
//! Communication with the WM thread is via an mpsc channel (`PanelMsg`) plus
//! an atomic quit flag, polled from a `slint::Timer` inside the panel's
//! event loop. Card clicks act on Win32 directly (restore + focus); the
//! resulting focus event drives the WM-side minimization of the previous
//! window automatically.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use slint::{
  Color, ComponentHandle, Image, LogicalPosition, LogicalSize, Model,
  ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode,
  VecModel,
};
use tracing::{debug, error, warn};
use wm_common::{StageManagerConfig, StageSide, StageSize};
use wm_platform::{Dispatcher, NativeWindow, NativeWindowWindowsExt};
use anyhow::Context;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
  HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
  CreateWindowExW, DefWindowProcW, FindWindowW, GetForegroundWindow,
  GetWindowLongW, GetWindowThreadProcessId, RegisterClassExW,
  SetForegroundWindow, SetWindowLongPtrW, SetWindowLongW, SetWindowPos,
  GWL_EXSTYLE, GWL_HWNDPARENT, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
  SWP_NOACTIVATE, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::{
  capture::{self, CapturedImage},
  PanelMsg, PanelToWm, CAPTURE_MAX_DIM, PANEL_TITLE,
};

slint::include_modules!();

/// Panel poll interval.
const TICK: Duration = Duration::from_millis(100);

/// Off-screen parking spot for the panel window while stage manager is
/// disabled (keeps Slint's event loop alive without showing anything).
const PARK_X: f32 = -32_000.0;
/// Vertical component of `PARK_X`.
const PARK_Y: f32 = -32_000.0;
/// Ticks between structural re-enumerations (~300ms).
const ENUM_TICKS: u32 = 3;
/// Card icon size in logical pixels (rendered supersampled x2).
const ICON_SIZE: u32 = 40;

/// Logical card width for a size preset (px).
fn card_width_for(size: StageSize) -> f32 {
  match size {
    StageSize::Small => 160.0,
    StageSize::Medium => 216.0,
    StageSize::Large => 280.0,
  }
}

/// Mutable state owned by the panel thread.
#[allow(clippy::struct_field_names)]
struct PanelState {
  cfg: StageManagerConfig,
  cards: Rc<VecModel<StageCard>>,
  /// HWNDs aligned with `cards` indices.
  hwnds: Vec<isize>,
  thumbs: HashMap<isize, Image>,
  /// App icons by HWND (kept across refreshes and window swaps).
  icons: HashMap<isize, Image>,
  /// Stable card slots: HWNDs in first-seen order, surviving focus changes.
  order: Vec<isize>,
  prev_enum: Vec<isize>,
  hovered: isize,
  last_hover_capture: Instant,
  enum_tick: u32,
  styled: bool,
  visible: bool,
  last_geom: Option<(i32, i32, i32, i32)>,
  /// HWND of the panel's own native window (set once styled).
  panel_hwnd: isize,
  /// Hidden owner window keeping the panel out of Alt+Tab (0 on failure).
  owner_hwnd: isize,
  /// Last foreground window that was not the panel (foreground guard).
  last_fg: isize,
}

/// Entry point invoked on the dedicated panel thread.
pub fn panel_main(
  config: StageManagerConfig,
  rx: Receiver<PanelMsg>,
  ack_rx: Receiver<()>,
  quit: Arc<AtomicBool>,
  dispatcher: Dispatcher,
  wm_tx: UnboundedSender<PanelToWm>,
) {
  match run_panel(config, rx, ack_rx, quit, dispatcher, wm_tx) {
    Ok(()) => debug!("Stage panel event loop exited."),
    Err(err) => error!("Stage panel failed to start: {err}"),
  }
}

#[allow(clippy::too_many_lines)]
fn run_panel(
  config: StageManagerConfig,
  rx: Receiver<PanelMsg>,
  ack_rx: Receiver<()>,
  quit: Arc<AtomicBool>,
  dispatcher: Dispatcher,
  wm_tx: UnboundedSender<PanelToWm>,
) -> Result<(), slint::PlatformError> {
  let ui = StagePanel::new()?;

  let cards_model = Rc::new(VecModel::<StageCard>::default());
  ui.global::<Stage>().set_cards(ModelRc::from(cards_model.clone()));

  let state = Rc::new(RefCell::new(PanelState {
    cfg: config,
    cards: cards_model,
    hwnds: Vec::new(),
    thumbs: HashMap::new(),
    icons: HashMap::new(),
    order: Vec::new(),
    prev_enum: Vec::new(),
    hovered: -1,
    last_hover_capture: Instant::now() - Duration::from_secs(60),
    enum_tick: 0,
    styled: false,
    visible: true,
    last_geom: None,
    panel_hwnd: 0,
    owner_hwnd: 0,
    last_fg: 0,
  }));

  ui.global::<Stage>().on_card_clicked({
    let state = state.clone();
    move |idx| {
      let hwnd = state
        .borrow()
        .hwnds
        .get(usize::try_from(idx).unwrap_or(usize::MAX))
        .copied();
      let Some(hwnd) = hwnd else { return };

      let window = NativeWindow::from_handle(hwnd);
      if !window.is_valid() {
        return;
      }

      // Ask the WM to sync its focus state, minimize the previous window,
      // restore and focus the target itself. Waiting for the ack keeps
      // the old window off the screen before the target appears (no two
      // windows side by side). A window the WM does not manage never acks,
      // so after the timeout the panel falls back to a native activation.
      if wm_tx.send(PanelToWm::CardActivated { hwnd }).is_err() {
        return;
      }

      if ack_rx.recv_timeout(Duration::from_millis(150)).is_ok() {
        return;
      }

      if window.is_minimized().unwrap_or(false) {
        // A single `SW_RESTORE` shows *and* activates the window. Calling
        // `focus()` additionally races the restore's own activation and
        // leaves the window flashing without a stable switch.
        let _ = window.restore(None);
        return;
      }

      let _ = window.focus();
    }
  });


  ui.global::<Stage>().on_card_hovered({
    let state = state.clone();
    move |value| {
      let mut state = state.borrow_mut();
      match value {
        // Entering card `value`.
        idx if idx >= 0 => {
          state.hovered = state
            .hwnds
            .get(usize::try_from(idx).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or(-1);
          state.last_hover_capture =
            Instant::now() - Duration::from_secs(60);
        }
        // Leaving the encoded card - clear hover only if it is still the
        // hovered one (event order between cards is not guaranteed).
        encoded if encoded <= -2 => {
          let left = state
            .hwnds
            .get(usize::try_from(-encoded - 2).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or(-1);
          if state.hovered == left {
            state.hovered = -1;
          }
        }
        _ => (),
      }
    }
  });

  apply_visuals(&ui, &state.borrow().cfg);
  ui.window().show()?;

  let timer = Timer::default();
  timer.start(TimerMode::Repeated, TICK, {
    let ui = ui.as_weak();
    let state = state.clone();
    let quit = quit.clone();
    move || {
      let Some(ui) = ui.upgrade() else {
        let _ = slint::quit_event_loop();
        return;
      };

      if quit.load(Ordering::Relaxed) {
        let _ = slint::quit_event_loop();
        return;
      }

      while let Ok(msg) = rx.try_recv() {
        match msg {
          PanelMsg::Refresh => refresh(&ui, &state, &dispatcher, true),
          PanelMsg::Snapshot { hwnd, image } => {
            apply_snapshot(&state, hwnd, image);
          }
          PanelMsg::Reload(cfg) => {
            state.borrow_mut().cfg = cfg.clone();
            apply_visuals(&ui, &cfg);
            state.borrow_mut().last_geom = None;
          }
          PanelMsg::SetVisible(visible) => {
            let changed = {
              let mut st = state.borrow_mut();
              if visible != st.visible {
                st.visible = visible;
                if visible {
                  // Force a geometry recalculation to pull the parked
                  // window back into its docking position.
                  st.last_geom = None;
                }
                true
              } else {
                false
              }
            };

            if !changed {
              continue;
            }

            if visible {
              // The window set may have changed while parked - rebuild
              // the cards immediately so the re-show is up to date.
              refresh(&ui, &state, &dispatcher, true);
            } else {
              // Slint exits its event loop when the last window
              // becomes invisible - and the backend cannot be
              // re-initialized in this process. Park the window
              // off-screen instead of hiding it so the panel
              // thread and its window survive a disable/enable
              // cycle for instant re-show.
              ui.window().set_position(slint::LogicalPosition::new(
                PARK_X, PARK_Y,
              ));
            }
          }
          PanelMsg::Shutdown => {
            quit.store(true, Ordering::Relaxed);
          }
        }
      }

      // Bounce an unwanted foreground hand-off (Alt+Tab) back to the
      // previous window even while parked off-screen.
      guard_foreground(&mut state.borrow_mut());

      // While hidden (stage manager disabled) skip all enumeration and
      // capture work - the thread only idles to allow instant re-show.
      if !state.borrow().visible {
        return;
      }

      {
        state.borrow_mut().enum_tick += 1;
      }
      if state.borrow().enum_tick >= ENUM_TICKS {
        state.borrow_mut().enum_tick = 0;
        refresh(&ui, &state, &dispatcher, false);
      }

      refresh_hovered(&state);
      update_geometry(&ui, &state, &dispatcher);
      style_panel_window(&mut state.borrow_mut());
    }
  });

  ui.run()
}

/// Whether the given native window should appear as a card.
fn is_taskbar_window(window: &NativeWindow) -> bool {
  if !window.is_valid() {
    return false;
  }

  let own_pid = std::process::id();
  let mut pid = 0u32;
  // SAFETY: Win32 call with a valid HWND and pointer to a local `u32`.
  unsafe { GetWindowThreadProcessId(window.hwnd(), Some(&mut pid)) };
  if pid == own_pid {
    return false;
  }

  if window.has_owner_window() || window.has_window_style_ex(WS_EX_TOOLWINDOW)
  {
    return false;
  }

  let mut cloaked = 0u32;
  // SAFETY: Win32 call with a valid HWND and pointer to a local `u32`.
  let is_cloaked = unsafe {
    DwmGetWindowAttribute(
      window.hwnd(),
      DWMWA_CLOAKED,
      std::ptr::from_mut::<u32>(&mut cloaked).cast(),
      std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
      && cloaked != 0
  };
  if is_cloaked {
    return false;
  }

  if window.is_desktop_window().unwrap_or(false) {
    return false;
  }

  window
    .title()
    .map(|title| !title.trim().is_empty())
    .unwrap_or(false)
}

/// Re-enumerates taskbar-style windows and rebuilds the model when the set
/// (or its order) changed.
fn refresh(
  ui: &StagePanel,
  state: &Rc<RefCell<PanelState>>,
  dispatcher: &Dispatcher,
  force: bool,
) {
  // Exclude the focused window only while it is actually on screen. With
  // everything minimized the native foreground is a stale leftover, and
  // excluding it would hide a card the user still needs to pick from.
  let focused_id = dispatcher
    .focused_window()
    .ok()
    .filter(|window| !window.is_minimized().unwrap_or(false))
    .map(|window| window.id().0)
    .unwrap_or(0);

  let mut taskbar: Vec<(isize, String, String)> = Vec::new();
  for window in dispatcher.visible_windows().unwrap_or_default() {
    if !is_taskbar_window(&window) {
      continue;
    }

    taskbar.push((
      window.id().0,
      window.title().unwrap_or_default(),
      window.process_name().unwrap_or_default(),
    ));
  }

  // Stable card slots: surviving windows keep their original slot and
  // first-seen windows are appended, so the card order never shuffles with
  // z-order changes. The focused window holds its slot while it is off the
  // card list, reappearing at the same position when minimized.
  {
    let mut state = state.borrow_mut();
    state
      .order
      .retain(|hwnd| taskbar.iter().any(|(seen, _, _)| seen == hwnd));
    for (hwnd, _, _) in &taskbar {
      if !state.order.contains(hwnd) {
        state.order.push(*hwnd);
      }
    }

    // Drop cached captures for windows that are gone.
    state
      .thumbs
      .retain(|hwnd, _| taskbar.iter().any(|(seen, _, _)| seen == hwnd));
    state
      .icons
      .retain(|hwnd, _| taskbar.iter().any(|(seen, _, _)| seen == hwnd));
  }

  let enumerated: Vec<(isize, String, String)> = {
    let state = state.borrow();
    state
      .order
      .iter()
      .filter_map(|hwnd| {
        taskbar
          .iter()
          .find(|(seen, _, _)| seen == hwnd)
          .cloned()
      })
      .filter(|(hwnd, _, _)| *hwnd != focused_id)
      .collect()
  };

  let hwnds: Vec<isize> = enumerated.iter().map(|(hwnd, _, _)| *hwnd).collect();

  let changed = {
    let state = state.borrow();
    hwnds != state.prev_enum
  };
  if !changed && !force {
    return;
  }

  let mut cards = Vec::with_capacity(enumerated.len());
  for (hwnd, title, process) in &enumerated {
    let cached = state.borrow().thumbs.get(hwnd).cloned();
    let thumb = match cached {
      Some(image) => Some(image),
      None => {
        let captured = capture::capture_window(*hwnd, CAPTURE_MAX_DIM);
        captured.map(|image| {
          let image = image_to_slint(image);
          state.borrow_mut().thumbs.insert(*hwnd, image.clone());
          image
        })
      }
    };

    let cached_icon = state.borrow().icons.get(hwnd).cloned();
    let icon = match cached_icon {
      Some(image) => Some(image),
      None => {
        let captured = capture::capture_window_icon(*hwnd, ICON_SIZE)
          .filter(|image| image.width > 0);
        captured.map(|image| {
          let image = image_to_slint(image);
          state.borrow_mut().icons.insert(*hwnd, image.clone());
          image
        })
      }
    };

    cards.push(build_card(title, process, thumb, icon));
  }

  {
    let mut state = state.borrow_mut();
    state.prev_enum = hwnds;
    state.hwnds = enumerated.iter().map(|(hwnd, _, _)| *hwnd).collect();
    state.cards.clear();
    for card in cards {
      state.cards.push(card);
    }
  }

  let count = state.borrow().cards.row_count();
  ui.global::<Stage>().set_status(if count == 0 {
    SharedString::from("台前调度已开启\n没有后台窗口")
  } else {
    SharedString::default()
  });
}

/// Re-captures the hovered window's thumbnail while it stays visible.
fn refresh_hovered(state: &Rc<RefCell<PanelState>>) {
  let (hovered, interval) = {
    let state = state.borrow();
    (state.hovered, state.cfg.hover_refresh_ms)
  };
  if hovered < 0 {
    return;
  }

  {
    let state = state.borrow();
    if state.last_hover_capture.elapsed()
      < Duration::from_millis(u64::from(interval))
    {
      return;
    }
  }

  let window = NativeWindow::from_handle(hovered);
  if !window.is_valid()
    || window.is_minimized().unwrap_or(true)
    || !window.is_visible().unwrap_or(false)
  {
    return;
  }

  state.borrow_mut().last_hover_capture = Instant::now();

  let Some(image) = capture::capture_window(hovered, CAPTURE_MAX_DIM) else {
    return;
  };

  set_card_thumb(state, hovered, image_to_slint(image));
}

/// Applies a snapshot pushed by the WM engine (taken right before a window
/// gets minimized) to the matching card, if present.
fn apply_snapshot(state: &Rc<RefCell<PanelState>>, hwnd: isize, image: CapturedImage) {
  let image = image_to_slint(image);
  state.borrow_mut().thumbs.insert(hwnd, image.clone());

  let exists = state.borrow().hwnds.contains(&hwnd);
  if exists {
    set_card_thumb(state, hwnd, image);
  }
}

/// Sets the thumbnail of the card bound to `hwnd`, re-rendering only that
/// row.
fn set_card_thumb(
  state: &Rc<RefCell<PanelState>>,
  hwnd: isize,
  image: Image,
) {
  let state = state.borrow();
  let Some(pos) = state.hwnds.iter().position(|&candidate| candidate == hwnd)
  else {
    return;
  };

  let Some(mut card) = state.cards.row_data(pos) else {
    return;
  };
  card.thumb = image;
  card.has_thumb = true;
  state.cards.set_row_data(pos, card);
}

/// Builds a card with a thumbnail, or a placeholder based on the title's
/// first char and a process-derived accent color.
fn build_card(
  title: &str,
  process: &str,
  thumb: Option<Image>,
  icon: Option<Image>,
) -> StageCard {
  let initial = title
    .trim()
    .chars()
    .next()
    .map(|c| c.to_uppercase().collect::<String>())
    .unwrap_or_default();

  StageCard {
    title: SharedString::from(title),
    thumb: thumb.clone().unwrap_or_default(),
    initial: SharedString::from(initial),
    accent: accent_color(process),
    has_thumb: thumb.is_some(),
    icon: icon.clone().unwrap_or_default(),
    has_icon: icon.is_some(),
  }
}

/// Deterministic card accent color derived from the process name.
fn accent_color(process: &str) -> Color {
  let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
  for byte in process.as_bytes() {
    hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
  }

  // HSL(hue, 0.5, 0.35) to RGB.
  let hue = (hash % 360) as f32;
  let lightness = 0.35f32;
  let saturation = 0.5f32;
  let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
  let x = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
  let m = lightness - chroma / 2.0;
  let (r, g, b) = match (hue / 60.0) as u32 {
    0 => (chroma, x, 0.0),
    1 => (x, chroma, 0.0),
    2 => (0.0, chroma, x),
    3 => (0.0, x, chroma),
    4 => (x, 0.0, chroma),
    _ => (chroma, 0.0, x),
  };

  Color::from_rgb_u8(
    ((r + m) * 255.0) as u8,
    ((g + m) * 255.0) as u8,
    ((b + m) * 255.0) as u8,
  )
}

fn image_to_slint(image: CapturedImage) -> Image {
  let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
    &image.rgba,
    image.width,
    image.height,
  );
  Image::from_rgba8(buffer)
}

/// Pushes visual properties that don't require geometry recalculation.
fn apply_visuals(ui: &StagePanel, cfg: &StageManagerConfig) {
  let global = ui.global::<Stage>();
  global.set_card_opacity(cfg.opacity as f32);
  global.set_corner(cfg.corner_radius as f32);
}

/// Computes and applies the panel size/position from the primary display.
fn update_geometry(
  ui: &StagePanel,
  state: &Rc<RefCell<PanelState>>,
  dispatcher: &Dispatcher,
) {
  let Ok(display) = dispatcher.primary_display() else {
    return;
  };
  let Ok(bounds) = display.bounds() else {
    return;
  };

  let scale = ui.window().scale_factor() as f32;
  let screen_x = bounds.x() as f32 / scale;
  let screen_y = bounds.y() as f32 / scale;
  let screen_w = bounds.width() as f32 / scale;
  let screen_h = bounds.height() as f32 / scale;

  let (card_width, card_height, spacing, side, corner, count) = {
    let state = state.borrow();
    let card_width = card_width_for(state.cfg.size);
    (
      card_width,
      card_width * 9.0 / 16.0 + 26.0,
      state.cfg.spacing as f32,
      state.cfg.side,
      state.cfg.corner_radius as f32,
      state.cards.row_count() as f32,
    )
  };

  let pad = 12.0f32;
  let panel_w = card_width + 2.0 * pad;
  let raw_h = if count <= 0.0 {
    pad * 2.0 + 60.0
  } else {
    pad * 2.0 + count * card_height + (count - 1.0) * spacing
  };
  let panel_h = raw_h.min(screen_h - 24.0).max(80.0);

  let x = if matches!(side, StageSide::Left) {
    screen_x + 8.0
  } else {
    screen_x + screen_w - panel_w - 8.0
  };
  let y = screen_y + 12.0;

  let geom = (x as i32, y as i32, panel_w as i32, panel_h as i32);
  {
    let state = state.borrow();
    if state.last_geom == Some(geom) {
      return;
    }
  }

  let global = ui.global::<Stage>();
  global.set_card_width(card_width);
  global.set_card_height(card_height);
  global.set_spacing(spacing);
  global.set_corner(corner);

  ui.window().set_size(LogicalSize::new(panel_w, panel_h));
  ui.window().set_position(LogicalPosition::new(x, y));
  state.borrow_mut().last_geom = Some(geom);
}

/// One-shot window styling: hide from the task switcher and keep above.
///
/// Besides `WS_EX_TOOLWINDOW`, a hidden owner window is assigned via
/// `GWL_HWNDPARENT`. On Windows 11 24H2 the Alt+Tab enumeration was
/// observed to still pick tool windows once they have been activated,
/// while the "root owner" rule (a window joins Alt+Tab only when its
/// root-owner chain resolves back to itself) still excludes windows that
/// are owned by another window. The owner trick provides the exclusion.
fn style_panel_window(state: &mut PanelState) {
  if state.styled {
    return;
  }

  // SAFETY: All Win32 calls below take valid handles; the HWND from
  // `FindWindowW` is checked before styles are modified.
  unsafe {
    let title = HSTRING::from(PANEL_TITLE);
    let hwnd = FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr()));
    if hwnd.0 == 0 {
      debug!("Stage panel window not found for styling.");
      return;
    }

    if state.owner_hwnd == 0 {
      match create_owner_window() {
        Ok(owner) => state.owner_hwnd = owner,
        Err(err) => warn!("Stage panel owner window creation failed: {err}"),
      }
    }

    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    SetWindowLongW(
      hwnd,
      GWL_EXSTYLE,
      (ex_style | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as i32,
    );

    if state.owner_hwnd != 0 {
      SetWindowLongPtrW(hwnd, GWL_HWNDPARENT, state.owner_hwnd);
    }

    let _ = SetWindowPos(
      hwnd,
      HWND_TOPMOST,
      0,
      0,
      0,
      0,
      SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );

    state.panel_hwnd = hwnd.0;
    state.styled = true;
  }
}

/// Window procedure for the hidden owner window.
///
/// The owner never renders or receives meaningful messages - it only
/// exists so the Alt+Tab root-owner rule excludes the panel.
///
/// # SAFETY
///
/// Called by the OS with a valid window handle; delegates entirely to
/// `DefWindowProcW`.
unsafe extern "system" fn owner_wnd_proc(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Creates the hidden owner window that keeps the panel out of Alt+Tab.
///
/// The window is a 1x1 `WS_POPUP` parked far off-screen and never shown;
/// it only exists to own the panel window for the Alt+Tab root-owner rule.
fn create_owner_window() -> anyhow::Result<isize> {
  const CLASS_NAME: &str = "GlazeWMStageOwner";

  // SAFETY: Win32 class registration and window creation with a static
  // class name, `DefWindowProcW` as the procedure, and no parent window.
  let owner = unsafe {
    let hmodule =
      GetModuleHandleW(PCWSTR::null()).context("GetModuleHandleW failed")?;

    let hinstance: HINSTANCE = hmodule.into();
    let class_name = HSTRING::from(CLASS_NAME);
    let mut class = WNDCLASSEXW::default();
    class.cbSize =
      u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or_default();
    class.lpfnWndProc = Some(owner_wnd_proc);
    class.hInstance = hinstance;
    class.lpszClassName = PCWSTR(class_name.as_ptr());

    // A non-zero return means the class got registered; zero means it
    // either failed or was already registered by a previous cycle, in
    // which case `CreateWindowExW` below still resolves the class.
    let _ = RegisterClassExW(&class);

    CreateWindowExW(
      WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
      PCWSTR(class_name.as_ptr()),
      PCWSTR(class_name.as_ptr()),
      WS_POPUP,
      PARK_X as i32,
      PARK_Y as i32,
      1,
      1,
      None,
      None,
      hinstance,
      None,
    )
  };

  if owner.0 == 0 {
    anyhow::bail!("CreateWindowExW returned a null HWND");
  }

  Ok(owner.0)
}

/// Reclaims the foreground when the panel window was activated anyway.
///
/// Alt+Tab on some Windows 11 builds can still hand foreground to the
/// panel despite the owner-window exclusion. The panel never needs the
/// foreground, so it is bounced back to the last non-panel foreground
/// window within one timer tick.
fn guard_foreground(state: &mut PanelState) {
  if !state.styled || state.panel_hwnd == 0 {
    return;
  }

  // SAFETY: `GetForegroundWindow` always returns a valid or null HWND;
  // `SetForegroundWindow` tolerates a stale handle by failing.
  unsafe {
    let fg = GetForegroundWindow();
    if fg.0 == state.panel_hwnd {
      if state.last_fg != 0 {
        SetForegroundWindow(HWND(state.last_fg));
      }
    } else if fg.0 != 0 {
      state.last_fg = fg.0;
    }
  }
}
