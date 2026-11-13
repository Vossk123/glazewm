//! Window thumbnail capture via `PrintWindow`.
//!
//! Captures the visible content of a window into an RGBA buffer, cropped to
//! the DWM extended frame bounds (excluding the invisible shadow border),
//! detected for blankness, and downscaled to keep thumbnails small.
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

/// Captures the window content, or `None` if it failed or the window
/// rendered as blank (e.g. hardware-accelerated surfaces that `PrintWindow`
/// cannot see, in which case the panel falls back to a placeholder card).
#[cfg(target_os = "windows")]
pub fn capture_window(hwnd: isize, max_dim: u32) -> Option<CapturedImage> {
  use tracing::trace;
  use windows::Win32::Foundation::{HWND, RECT};
  use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
  };
  use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC,
    CreateDIBSection, DeleteDC, DeleteObject, DIB_RGB_COLORS, GetWindowDC,
    ReleaseDC, SelectObject,
  };
  use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
  use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

  let raw_hwnd = HWND(hwnd);

  let mut win_rect = RECT::default();
  let mut frame_rect = RECT::default();

  // SAFETY: All Win32 calls below operate on the given window handle and
  // GDI objects created within this function. Every created GDI object is
  // deleted before returning on every code path.
  unsafe {
    if GetWindowRect(raw_hwnd, &mut win_rect).is_err() {
      return None;
    }

    let win_w = win_rect.right - win_rect.left;
    let win_h = win_rect.bottom - win_rect.top;
    if win_w <= 0 || win_h <= 0 || win_w > 16384 || win_h > 16384 {
      return None;
    }

    // Crop the invisible shadow border via the DWM extended frame bounds.
    let has_frame_bounds = DwmGetWindowAttribute(
      raw_hwnd,
      DWMWA_EXTENDED_FRAME_BOUNDS,
      std::ptr::from_mut::<RECT>(&mut frame_rect).cast(),
      std::mem::size_of::<RECT>() as u32,
    )
    .is_ok()
      && frame_rect.right > frame_rect.left
      && frame_rect.bottom > frame_rect.top;

    let (crop_x, crop_y) = if has_frame_bounds {
      (
        (frame_rect.left - win_rect.left).clamp(0, win_w - 1),
        (frame_rect.top - win_rect.top).clamp(0, win_h - 1),
      )
    } else {
      (0, 0)
    };

    let crop_w = if has_frame_bounds {
      (frame_rect.right - frame_rect.left).clamp(1, win_w - crop_x)
    } else {
      win_w
    };
    let crop_h = if has_frame_bounds {
      (frame_rect.bottom - frame_rect.top).clamp(1, win_h - crop_y)
    } else {
      win_h
    };

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

    let mut bitmap_info = BITMAPINFO {
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
      &mut bitmap_info,
      DIB_RGB_COLORS,
      &mut bits,
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
        crop_x.max(0) as u32,
        crop_y.max(0) as u32,
        crop_w.max(1) as u32,
        crop_h.max(1) as u32,
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

/// Non-Windows stub so that the module compiles on all platforms.
#[cfg(not(target_os = "windows"))]
pub fn capture_window(_hwnd: isize, _max_dim: u32) -> Option<CapturedImage> {
  None
}

/// Renders the window's application icon into a `size x size` RGBA image.
///
/// Icon lookup order: `WM_GETICON` big, `WM_GETICON` small2, then the class
/// icons. Alpha is recovered by rendering the icon twice - over black and
/// over white - because `DrawIconEx` writes into a GDI DIB without a
/// trustworthy alpha channel.
#[cfg(target_os = "windows")]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn capture_window_icon(hwnd: isize, size: u32) -> Option<CapturedImage> {
  use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
  use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC,
    CreateDIBSection, DeleteDC, DeleteObject, DIB_RGB_COLORS, GetWindowDC,
    ReleaseDC, SelectObject,
  };
  use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetClassLongPtrW, SendMessageTimeoutW, DI_NORMAL, GCLP_HICON,
    GCLP_HICONSM, HICON, ICON_BIG, ICON_SMALL2, SEND_MESSAGE_TIMEOUT_FLAGS,
    WM_GETICON,
  };

  // Supersample so the small card icon stays crisp.
  let render = size.saturating_mul(2).max(4);
  let raw_hwnd = HWND(hwnd);

  // SAFETY: All Win32 calls operate on `hwnd` and GDI objects created here;
  // every created object is deleted before returning on all paths.
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
        Some(&mut result),
      );
      (result != 0).then(|| HICON(result as isize))
    };

    let hicon = icon(ICON_BIG as usize)
      .or_else(|| icon(ICON_SMALL2 as usize))
      .or_else(|| {
        let big = GetClassLongPtrW(raw_hwnd, GCLP_HICON);
        (big != 0).then(|| HICON(big as isize))
      })
      .or_else(|| {
        let small = GetClassLongPtrW(raw_hwnd, GCLP_HICONSM);
        (small != 0).then(|| HICON(small as isize))
      })?;

    let screen_dc = GetWindowDC(HWND::default());
    if screen_dc.is_invalid() {
      return None;
    }

    let mem_dc = CreateCompatibleDC(screen_dc);
    if mem_dc.is_invalid() {
      ReleaseDC(HWND::default(), screen_dc);
      return None;
    }

    let mut bitmap_info = BITMAPINFO {
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
      &mut bitmap_info,
      DIB_RGB_COLORS,
      &mut bits,
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
    let buffer = std::slice::from_raw_parts_mut(bits.cast::<u8>(), pixel_count * 4);

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

/// Non-Windows stub so that the module compiles on all platforms.
#[cfg(not(target_os = "windows"))]
pub fn capture_window_icon(_hwnd: isize, _size: u32) -> Option<CapturedImage> {
  None
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
          let a = 1.0 - ((r2 - r1).max(0.0) / 255.0)
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

/// Converts a top-down BGRA buffer into a cropped, blank-checked, box-filter
/// downscaled RGBA image.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn capture_from_bits(
  bits: *const u8,
  width: u32,
  height: u32,
  crop_x: u32,
  crop_y: u32,
  crop_w: u32,
  crop_h: u32,
  max_dim: u32,
) -> Option<CapturedImage> {
  if crop_x + crop_w > width || crop_y + crop_h > height {
    return None;
  }

  // SAFETY: `bits` points to a valid top-down 32bpp buffer of at least
  // `width * height * 4` bytes, created by `CreateDIBSection` and alive for
  // the duration of this call (enforced by the caller's cleanup order).
  let src = unsafe {
    std::slice::from_raw_parts(bits, (width * height * 4) as usize)
  };

  // Downscale factor for the box filter.
  let scale =
    ((crop_w.max(crop_h) as f64) / f64::from(max_dim.max(1))).ceil().max(1.0)
      as u32;
  let out_w = (crop_w / scale).max(1);
  let out_h = (crop_h / scale).max(1);

  let mut rgba = vec![0u8; (out_w * out_h * 4) as usize];
  let mut has_content = false;

  for out_y in 0..out_h {
    for out_x in 0..out_w {
      let mut sums = [0u64; 4];
      let mut count = 0u64;

      let base_x = crop_x + out_x * scale;
      let base_y = crop_y + out_y * scale;
      let end_x = (base_x + scale).min(crop_x + crop_w);
      let end_y = (base_y + scale).min(crop_y + crop_h);

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
      let b = (sums[0] / count) as u8;
      let g = (sums[1] / count) as u8;
      let r = (sums[2] / count) as u8;
      let a = (sums[3] / count) as u8;

      if r + g + b > 24 || a > 16 {
        has_content = true;
      }

      let o = ((out_y * out_w + out_x) * 4) as usize;
      rgba[o] = r;
      rgba[o + 1] = g;
      rgba[o + 2] = b;
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
  for pixel in rgba.chunks_exact(4) {
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

  fn capture(bgra: &[u8], width: u32, height: u32) -> Option<super::CapturedImage> {
    capture_from_bits(bgra.as_ptr(), width, height, 0, 0, width, height, 64)
  }

  #[test]
  fn flat_fill_counts_as_blank() {
    let width = 8;
    let height = 8;
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    for pixel in bgra.chunks_exact_mut(4) {
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
    for (i, pixel) in bgra.chunks_exact_mut(4).enumerate() {
      if i % 2 == 0 {
        pixel.copy_from_slice(&[200, 40, 60, 255]);
      }
    }
    let captured = capture(&bgra, width, height);
    assert!(captured.is_some());
  }
}
