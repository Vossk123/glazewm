//! Window thumbnail capture via `PrintWindow`.
//!
//! Captures the visible content of a window into an RGBA buffer, cropped
//! to the DWM extended frame bounds (excluding the invisible shadow
//! border), detected for blankness, and downscaled to keep thumbnails
//! small.
//!
//! Called from the WM thread right before minimizing a window (while the
//! window is still visible) and from the panel thread for hover refreshes.

/// A captured, downscaled window thumbnail in RGBA8.
#[derive(Clone, Debug)]
pub struct CapturedImage {
  pub width: u32,
  pub height: u32,
  pub rgba: Vec<u8>,
}

/// The crop of a captured window bitmap, in source pixels.
///
/// Excludes the invisible DWM shadow border when the extended frame
/// bounds are available.
struct Crop {
  x: u32,
  y: u32,
  width: u32,
  height: u32,
}

/// Captures the window content, or `None` if it failed or the window
/// rendered as blank (e.g. hardware-accelerated surfaces that
/// `PrintWindow` cannot see, in which case the panel falls back to a
/// placeholder card).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[cfg(target_os = "windows")]
pub fn capture_window(hwnd: isize, max_dim: u32) -> Option<CapturedImage> {
  use tracing::trace;
  use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
      CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
      GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
      BI_RGB, DIB_RGB_COLORS,
    },
    Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
    UI::WindowsAndMessaging::GetWindowRect,
  };

  let raw_hwnd = HWND(hwnd);

  let mut win_rect = windows::Win32::Foundation::RECT::default();

  // SAFETY: All Win32 calls below operate on the given window handle and
  // GDI objects created within this function. Every created GDI object is
  // deleted before returning on every code path.
  unsafe {
    if GetWindowRect(raw_hwnd, &raw mut win_rect).is_err() {
      return None;
    }

    let win_w = win_rect.right - win_rect.left;
    let win_h = win_rect.bottom - win_rect.top;
    if win_w <= 0 || win_h <= 0 || win_w > 16384 || win_h > 16384 {
      return None;
    }

    let crop = crop_bounds(raw_hwnd, &win_rect);

    let screen_dc = GetWindowDC(HWND::default());
    if screen_dc.is_invalid() {
      trace!("Stage capture: GetWindowDC failed.");
      return None;
    }

    let mem_dc = CreateCompatibleDC(screen_dc);
    if mem_dc.is_invalid() {
      ReleaseDC(HWND::default(), screen_dc);
      return None;
    }

    let bitmap_info = BITMAPINFO {
      bmiHeader: BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: win_w,
        // Negative height yields a top-down DIB (row 0 = top row).
        biHeight: -win_h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
      },
      ..Default::default()
    };

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(
      screen_dc,
      &raw const bitmap_info,
      DIB_RGB_COLORS,
      &raw mut bits,
      windows::Win32::Foundation::HANDLE(0),
      0,
    );

    let Ok(bitmap) = bitmap else {
      DeleteDC(mem_dc);
      ReleaseDC(HWND::default(), screen_dc);
      return None;
    };

    if bits.is_null() {
      let _ = DeleteObject(bitmap);
      DeleteDC(mem_dc);
      ReleaseDC(HWND::default(), screen_dc);
      return None;
    }

    let old_object = SelectObject(mem_dc, bitmap);
    // PW_RENDERFULLCONTENT (2) - required for modern (DirectComposition)
    // windows; the windows crate exposes it only as a bare `u32` in
    // `WindowsAndMessaging`, so wrap it for the Xps-typed parameter.
    let printed = PrintWindow(raw_hwnd, mem_dc, PRINT_WINDOW_FLAGS(2));

    let result = if printed.as_bool() {
      capture_from_bits(
        bits.cast::<u8>(),
        win_w as u32,
        win_h as u32,
        &crop,
        max_dim,
      )
    } else {
      None
    };

    SelectObject(mem_dc, old_object);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(mem_dc);
    ReleaseDC(HWND::default(), screen_dc);

    result
  }
}

/// Computes the shadow-free crop rectangle for `raw_hwnd` inside
/// `win_rect` (the window's `GetWindowRect` bounds).
///
/// Uses the DWM extended frame bounds to exclude the invisible shadow
/// border; falls back to the full window rect when unavailable.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[cfg(target_os = "windows")]
fn crop_bounds(
  raw_hwnd: windows::Win32::Foundation::HWND,
  win_rect: &windows::Win32::Foundation::RECT,
) -> Crop {
  use windows::Win32::{
    Foundation::RECT,
    Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
  };

  let win_w = win_rect.right - win_rect.left;
  let win_h = win_rect.bottom - win_rect.top;

  let mut frame_rect = RECT::default();

  // SAFETY: `frame_rect` is a live `RECT` and the byte size passed
  // matches it exactly.
  let has_frame_bounds = unsafe {
    DwmGetWindowAttribute(
      raw_hwnd,
      DWMWA_EXTENDED_FRAME_BOUNDS,
      std::ptr::from_mut::<RECT>(&mut frame_rect).cast(),
      std::mem::size_of::<RECT>() as u32,
    )
    .is_ok()
  } && frame_rect.right > frame_rect.left
    && frame_rect.bottom > frame_rect.top;

  if !has_frame_bounds {
    return Crop {
      x: 0,
      y: 0,
      width: win_w as u32,
      height: win_h as u32,
    };
  }

  let crop_x = (frame_rect.left - win_rect.left).clamp(0, win_w - 1);
  let crop_y = (frame_rect.top - win_rect.top).clamp(0, win_h - 1);

  Crop {
    x: crop_x.max(0) as u32,
    y: crop_y.max(0) as u32,
    width: (frame_rect.right - frame_rect.left)
      .clamp(1, win_w - crop_x)
      .max(1) as u32,
    height: (frame_rect.bottom - frame_rect.top)
      .clamp(1, win_h - crop_y)
      .max(1) as u32,
  }
}

/// Resolves the window's icon handle.
///
/// Tries `WM_GETICON` (big, then small2) before falling back to the
/// class icon slots.
#[cfg(target_os = "windows")]
#[allow(clippy::cast_possible_wrap)]
fn get_window_icon(
  raw_hwnd: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
  use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::WindowsAndMessaging::{
      GetClassLongPtrW, SendMessageTimeoutW, GCLP_HICON, GCLP_HICONSM,
      HICON, ICON_BIG, ICON_SMALL2, SEND_MESSAGE_TIMEOUT_FLAGS,
      WM_GETICON,
    },
  };

  // SAFETY: `SendMessageTimeoutW` writes a single pointer-sized value
  // into the provided out-parameter.
  unsafe {
    let icon = |wparam: usize| -> Option<HICON> {
      let mut result: usize = 0;
      SendMessageTimeoutW(
        raw_hwnd,
        WM_GETICON,
        WPARAM(wparam),
        LPARAM(0),
        SEND_MESSAGE_TIMEOUT_FLAGS(0),
        100,
        Some(&raw mut result),
      );
      (result != 0).then_some(HICON(result as isize))
    };

    icon(ICON_BIG as usize)
      .or_else(|| icon(ICON_SMALL2 as usize))
      .or_else(|| {
        let big = GetClassLongPtrW(raw_hwnd, GCLP_HICON);
        (big != 0).then_some(HICON(big as isize))
      })
      .or_else(|| {
        let small = GetClassLongPtrW(raw_hwnd, GCLP_HICONSM);
        (small != 0).then_some(HICON(small as isize))
      })
  }
}

/// Renders the window's application icon into a `size x size` RGBA
/// image.
///
/// Alpha is recovered by rendering the icon twice - over black and over
/// white - because `DrawIconEx` writes into a GDI DIB without a
/// trustworthy alpha channel.
#[cfg(target_os = "windows")]
#[allow(
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  clippy::cast_possible_wrap
)]
pub fn capture_window_icon(
  hwnd: isize,
  size: u32,
) -> Option<CapturedImage> {
  use windows::Win32::{
    Foundation::{HANDLE, HWND},
    Graphics::Gdi::{
      CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
      GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
      BI_RGB, DIB_RGB_COLORS,
    },
    UI::WindowsAndMessaging::{DrawIconEx, DI_NORMAL},
  };

  // Supersample so the small card icon stays crisp.
  let render = size.saturating_mul(2).max(4);
  let raw_hwnd = HWND(hwnd);

  // SAFETY: All Win32 calls operate on `hwnd` and GDI objects created
  // here; every created object is deleted before returning on all paths.
  unsafe {
    let hicon = get_window_icon(raw_hwnd)?;

    let screen_dc = GetWindowDC(HWND::default());
    if screen_dc.is_invalid() {
      return None;
    }

    let mem_dc = CreateCompatibleDC(screen_dc);
    if mem_dc.is_invalid() {
      ReleaseDC(HWND::default(), screen_dc);
      return None;
    }

    let bitmap_info = BITMAPINFO {
      bmiHeader: BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: render as i32,
        biHeight: -(render as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
      },
      ..Default::default()
    };

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bitmap = match CreateDIBSection(
      screen_dc,
      &raw const bitmap_info,
      DIB_RGB_COLORS,
      &raw mut bits,
      HANDLE(0),
      0,
    ) {
      Ok(bitmap) if !bits.is_null() => bitmap,
      _ => {
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);
        return None;
      }
    };

    let old_object = SelectObject(mem_dc, bitmap);
    let pixel_count = (render * render) as usize;
    let buffer =
      std::slice::from_raw_parts_mut(bits.cast::<u8>(), pixel_count * 4);

    // Pass 1: over black.
    buffer.fill(0);
    let on_black = DrawIconEx(
      mem_dc,
      0,
      0,
      hicon,
      render as i32,
      render as i32,
      0,
      None,
      DI_NORMAL,
    )
    .is_ok();
    let black_pass = buffer.to_vec();

    // Pass 2: over white (opaque 0xFF BGRA everywhere).
    buffer.fill(0xFF);
    let on_white = DrawIconEx(
      mem_dc,
      0,
      0,
      hicon,
      render as i32,
      render as i32,
      0,
      None,
      DI_NORMAL,
    )
    .is_ok();

    let result = if on_black && on_white {
      Some(recover_icon_alpha(&black_pass, buffer, render, size))
    } else {
      None
    };

    SelectObject(mem_dc, old_object);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(mem_dc);
    ReleaseDC(HWND::default(), screen_dc);

    result
  }
}

/// Builds the final `size x size` RGBA image by un-premultiplying the
/// per-pixel alpha recovered from the black-backed and white-backed render
/// passes, box-downscaling from `render` to `size`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn recover_icon_alpha(
  black_pass: &[u8],
  white_pass: &[u8],
  render: u32,
  size: u32,
) -> CapturedImage {
  let mut rgba = vec![0u8; (size * size * 4) as usize];
  let factor = render / size;
  let mut any_opaque = false;

  for out_y in 0..size {
    for out_x in 0..size {
      let mut acc = [0f32; 4];
      let mut samples = 0f32;

      let base_x = out_x * factor;
      let base_y = out_y * factor;
      for y in base_y..(base_y + factor).min(render) {
        for x in base_x..(base_x + factor).min(render) {
          let i = ((y * render + x) * 4) as usize;
          let (b1, g1, r1) = (
            f32::from(black_pass[i]),
            f32::from(black_pass[i + 1]),
            f32::from(black_pass[i + 2]),
          );
          let (b2, g2, r2) = (
            f32::from(white_pass[i]),
            f32::from(white_pass[i + 1]),
            f32::from(white_pass[i + 2]),
          );

          // over_black = a*F ; over_white = a*F + (1-a)*255
          // => a = 1 - (white - black) / 255 per channel.
          let a = 1.0
            - ((r2 - r1).max(0.0) / 255.0)
              .max((g2 - g1).max(0.0) / 255.0)
              .max((b2 - b1).max(0.0) / 255.0);

          let a = a.clamp(0.0, 1.0);
          let safe_a = if a > 0.01 { a } else { 1.0 };
          acc[0] += (r1 / safe_a).clamp(0.0, 255.0);
          acc[1] += (g1 / safe_a).clamp(0.0, 255.0);
          acc[2] += (b1 / safe_a).clamp(0.0, 255.0);
          acc[3] += a * 255.0;
          samples += 1.0;
        }
      }

      if samples == 0.0 {
        continue;
      }

      let o = ((out_y * size + out_x) * 4) as usize;
      rgba[o] = (acc[0] / samples) as u8;
      rgba[o + 1] = (acc[1] / samples) as u8;
      rgba[o + 2] = (acc[2] / samples) as u8;
      rgba[o + 3] = (acc[3] / samples) as u8;
      if rgba[o + 3] > 16 {
        any_opaque = true;
      }
    }
  }

  // Fully transparent result (e.g. a GDI path that ignored alpha) - treat
  // as failure so the card keeps its placeholder.
  if !any_opaque {
    return CapturedImage {
      width: 0,
      height: 0,
      rgba: Vec::new(),
    };
  }

  CapturedImage {
    width: size,
    height: size,
    rgba,
  }
}

/// Converts a top-down BGRA buffer into a cropped, blank-checked,
/// box-filter downscaled RGBA image.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn capture_from_bits(
  bits: *const u8,
  width: u32,
  height: u32,
  crop: &Crop,
  max_dim: u32,
) -> Option<CapturedImage> {
  if crop.x + crop.width > width || crop.y + crop.height > height {
    return None;
  }

  // SAFETY: `bits` points to a valid top-down 32bpp buffer of at least
  // `width * height * 4` bytes, created by `CreateDIBSection` and alive
  // for the duration of this call (enforced by the caller's cleanup
  // order).
  let src = unsafe {
    std::slice::from_raw_parts(bits, (width * height * 4) as usize)
  };

  // Downscale factor for the box filter.
  let scale = (f64::from(crop.width.max(crop.height))
    / f64::from(max_dim.max(1)))
  .ceil()
  .max(1.0) as u32;
  let out_w = (crop.width / scale).max(1);
  let out_h = (crop.height / scale).max(1);

  let mut rgba = vec![0u8; (out_w * out_h * 4) as usize];
  let mut has_content = false;

  for out_y in 0..out_h {
    for out_x in 0..out_w {
      let mut sums = [0u64; 4];
      let mut count = 0u64;

      let base_x = crop.x + out_x * scale;
      let base_y = crop.y + out_y * scale;
      let end_x = (base_x + scale).min(crop.x + crop.width);
      let end_y = (base_y + scale).min(crop.y + crop.height);

      for y in base_y..end_y {
        let row = (y * width) as usize;
        for x in base_x..end_x {
          let i = (row + x as usize) * 4;
          sums[0] += u64::from(src[i]);
          sums[1] += u64::from(src[i + 1]);
          sums[2] += u64::from(src[i + 2]);
          sums[3] += u64::from(src[i + 3]);
          count += 1;
        }
      }

      if count == 0 {
        continue;
      }

      // Source is BGRA (premultiplied by DWM), output is RGBA.
      let blue = (sums[0] / count) as u8;
      let green = (sums[1] / count) as u8;
      let red = (sums[2] / count) as u8;
      let alpha = (sums[3] / count) as u8;

      if red + green + blue > 24 || alpha > 16 {
        has_content = true;
      }

      let o = ((out_y * out_w + out_x) * 4) as usize;
      rgba[o] = red;
      rgba[o + 1] = green;
      rgba[o + 2] = blue;
      rgba[o + 3] = 255;
    }
  }

  // Uniform flat fills are as useless as a black frame: `PrintWindow`
  // on a minimized window (no DWM surface anymore) yields a single
  // solid color, and GPU-composited windows that ignore the
  // print pass render one too. Treat them as blank so the card shows
  // its fallback instead.
  let mut min_rgb = [255u8; 3];
  let mut max_rgb = [0u8; 3];
  for pixel in rgba.as_chunks::<4>().0 {
    for channel in 0..3 {
      min_rgb[channel] = min_rgb[channel].min(pixel[channel]);
      max_rgb[channel] = max_rgb[channel].max(pixel[channel]);
    }
  }
  let uniform = (0..3).all(|channel| {
    i32::from(max_rgb[channel]) - i32::from(min_rgb[channel]) <= 8
  });

  // Windows rendered as fully blank or flat (common for minimized and
  // some GPU-composited surfaces) - signal the caller to use a
  // placeholder card instead.
  if !has_content || uniform {
    return None;
  }

  Some(CapturedImage {
    width: out_w,
    height: out_h,
    rgba,
  })
}

#[cfg(test)]
mod tests {
  use super::capture_from_bits;

  fn capture(
    bgra: &[u8],
    width: u32,
    height: u32,
  ) -> Option<super::CapturedImage> {
    let crop = super::Crop {
      x: 0,
      y: 0,
      width,
      height,
    };
    capture_from_bits(bgra.as_ptr(), width, height, &crop, 64)
  }

  #[test]
  fn flat_fill_counts_as_blank() {
    let width = 8;
    let height = 8;
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    for pixel in bgra.as_chunks_mut::<4>().0 {
      pixel.copy_from_slice(&[160, 160, 160, 255]);
    }
    assert!(capture(&bgra, width, height).is_none());
  }

  #[test]
  fn black_counts_as_blank() {
    let width = 8;
    let height = 8;
    let bgra = vec![0u8; (width * height * 4) as usize];
    assert!(capture(&bgra, width, height).is_none());
  }

  #[test]
  fn varied_content_is_kept() {
    let width = 8;
    let height = 8;
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    for (i, pixel) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
      if i % 2 == 0 {
        pixel.copy_from_slice(&[200, 40, 60, 255]);
      }
    }
    let captured = capture(&bgra, width, height);
    assert!(captured.is_some());
  }
}
