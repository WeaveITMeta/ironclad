//! Native screen capture via Win32 PrintWindow.
//!
//! Same path as the backend `windows_screenshot_foreground` tool. We
//! capture the foreground window, downscale to 1568px long edge,
//! encode as JPEG q=0.85, base64-encode. The resulting string is the
//! exact format Anthropic accepts as image content.
//!
//! Returns `None` on non-Windows targets so the rest of the desktop
//! crate stays cross-platform-buildable.

#[cfg(target_os = "windows")]
mod windows_impl {
    use anyhow::{Context, Result};
    use base64::Engine;
    use image::{ImageBuffer, Rgba};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
        ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};

    const MAX_LONG_EDGE: u32 = 1568;
    const JPEG_QUALITY: u8 = 85;

    pub fn capture_foreground() -> Result<String> {
        let r = catch_unwind(AssertUnwindSafe(|| unsafe {
            capture_inner()
        }));
        match r {
            Ok(v) => v,
            Err(_) => anyhow::bail!("PrintWindow panicked in Win32 FFI"),
        }
    }

    unsafe fn capture_inner() -> Result<String> { unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            anyhow::bail!("no foreground window");
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        GetWindowRect(hwnd, &mut rect).context("GetWindowRect")?;
        let src_w = (rect.right - rect.left).max(1) as i32;
        let src_h = (rect.bottom - rect.top).max(1) as i32;

        let null_hwnd = HWND(std::ptr::null_mut());
        let screen_dc = GetDC(null_hwnd);
        if screen_dc.0.is_null() {
            anyhow::bail!("GetDC failed");
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, src_w, src_h);
        let prev = SelectObject(mem_dc, HGDIOBJ(bitmap.0));

        let _ = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(0x2));

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: src_w,
                biHeight: -src_h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let pixel_count = (src_w as usize) * (src_h as usize);
        let mut buf = vec![0u8; pixel_count * 4];
        let scanlines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            src_h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem_dc, prev);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(mem_dc);
        ReleaseDC(null_hwnd, screen_dc);

        if scanlines == 0 {
            anyhow::bail!("GetDIBits returned 0 scanlines");
        }

        // BGRA → RGBA
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        for chunk in buf.chunks_exact(4) {
            rgba.push(chunk[2]);
            rgba.push(chunk[1]);
            rgba.push(chunk[0]);
            rgba.push(0xFF);
        }
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_raw(src_w as u32, src_h as u32, rgba)
                .context("from_raw")?;

        // Downscale.
        let long_edge = src_w.max(src_h) as u32;
        let img = if long_edge > MAX_LONG_EDGE {
            let scale = MAX_LONG_EDGE as f32 / long_edge as f32;
            let dst_w = ((src_w as f32) * scale).round() as u32;
            let dst_h = ((src_h as f32) * scale).round() as u32;
            image::imageops::resize(&img, dst_w, dst_h, image::imageops::FilterType::Triangle)
        } else {
            img
        };

        // JPEG encode.
        let mut jpeg_bytes: Vec<u8> = Vec::new();
        {
            use image::codecs::jpeg::JpegEncoder;
            let mut encoder = JpegEncoder::new_with_quality(&mut jpeg_bytes, JPEG_QUALITY);
            // Drop alpha for JPEG.
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            encoder
                .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
                .context("JPEG encode")?;
        }

        Ok(base64::engine::general_purpose::STANDARD.encode(&jpeg_bytes))
    }}
}

#[cfg(target_os = "windows")]
pub use windows_impl::capture_foreground;

#[cfg(not(target_os = "windows"))]
pub fn capture_foreground() -> anyhow::Result<String> {
    anyhow::bail!("screen capture only implemented on Windows")
}
