//! Capture a screenshot of a specific window (defaults to foreground) and
//! hand the pixels back to Claude as a vision-content image block. This
//! is the third leg of "what is McKale looking at right now" — title
//! parsing and recent_logs answer the first two cheaply, but when the
//! model genuinely needs to read pixels on screen (a PDF, a chart, a
//! design mock, a paused video), this is the tool.
//!
//! Strategy:
//!   1. Resolve target HWND (GetForegroundWindow, or by title_contains).
//!   2. Capture via PrintWindow(PW_RENDERFULLCONTENT) into a memory DC —
//!      works for layered windows like Chrome and most UWP apps.
//!   3. GetDIBits to pull BGRA pixels, swizzle to RGBA.
//!   4. Encode PNG via `image` crate, base64-encode, attach to
//!      `ToolOutput::images` so the agent loop emits an Image content
//!      block alongside the tool_result block.
//!
//! The text result intentionally stays tiny — just title/process/dims —
//! so the model doesn't waste tokens parsing the JSON before looking at
//! the actual image. All Win32 calls are wrapped in catch_unwind to keep
//! GDI surprises from taking down the gateway.

use std::time::Instant;

use async_trait::async_trait;
use base64::Engine;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

pub struct WindowsScreenshotForegroundTool;

#[async_trait]
impl Tool for WindowsScreenshotForegroundTool {
    fn name(&self) -> &str {
        "windows_screenshot_foreground"
    }

    fn description(&self) -> &str {
        "Capture a PNG screenshot of a window and return it as a vision \
         image so you can read what's on screen. With no arguments, \
         captures the current foreground window. Pass `title_contains` \
         and/or `process` to target a specific window instead. Use this \
         when the user asks 'what am I looking at', 'what's in this \
         document', 'read this for me', or when title parsing isn't \
         enough. Read-only, no approval."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title_contains": {
                    "type": "string",
                    "description": "Case-insensitive substring of the window title. Omit to capture the foreground window."
                },
                "process": {
                    "type": "string",
                    "description": "Process name without .exe, case-insensitive. Omit to capture the foreground window."
                }
            }
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let title = params
            .get("title_contains")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        let process = params
            .get("process")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());

        #[cfg(target_os = "windows")]
        {
            let title_clone = title.clone();
            let process_clone = process.clone();
            let shot = tokio::task::spawn_blocking(move || {
                capture_window(title_clone.as_deref(), process_clone.as_deref())
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.png);
            let result = serde_json::json!({
                "status": "captured",
                "hwnd": shot.hwnd as u64,
                "title": shot.title,
                "process": shot.process,
                "width": shot.width,
                "height": shot.height,
                "bytes": shot.png.len(),
            });
            return Ok(ToolOutput::success(result, start.elapsed()).with_images(vec![b64]));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (title, process, start);
            Err(ToolError::NotAuthorized(
                "windows_screenshot_foreground only works on Windows".to_string(),
            ))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

#[cfg(target_os = "windows")]
struct CapturedWindow {
    hwnd: isize,
    title: String,
    process: String,
    width: u32,
    height: u32,
    png: Vec<u8>,
}

#[cfg(target_os = "windows")]
fn capture_window(
    title_contains: Option<&str>,
    process: Option<&str>,
) -> Result<CapturedWindow, String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
        GetDIBits, HBITMAP, HGDIOBJ, ReleaseDC, SelectObject,
    };
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId,
    };

    let r = catch_unwind(AssertUnwindSafe(|| unsafe {
        // 1. Resolve HWND.
        let hwnd: HWND = if title_contains.is_some() || process.is_some() {
            let (raw, _) = crate::tools::builtin::windows_desktop::find_window_handle(
                process,
                title_contains,
            )
            .ok_or_else(|| "no matching window".to_string())?;
            HWND(raw)
        } else {
            let h = GetForegroundWindow();
            if h.0 == 0 {
                return Err("no foreground window".to_string());
            }
            h
        };

        // 2. Resolve title + process for caller context.
        let len = GetWindowTextLengthW(hwnd);
        let title = if len > 0 {
            let mut buf = vec![0u16; (len as usize) + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            String::new()
        };
        let mut pid: u32 = 0;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process_name = if pid != 0 {
            match OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                false,
                pid,
            ) {
                Ok(handle) => {
                    let mut name = [0u16; 260];
                    let n = GetModuleBaseNameW(handle, None, &mut name);
                    let _ = windows::Win32::Foundation::CloseHandle(handle);
                    if n > 0 {
                        let s = String::from_utf16_lossy(&name[..n as usize]);
                        s.trim_end_matches(".exe").to_string()
                    } else {
                        String::new()
                    }
                }
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };

        // 3. Get window rect (full window, not just client area, so the
        //    title bar is included — useful for "which doc am I in"
        //    questions where the tab title is in the chrome).
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return Err("GetWindowRect failed".to_string());
        }
        let width = (rect.right - rect.left).max(1) as i32;
        let height = (rect.bottom - rect.top).max(1) as i32;

        // 4. Build memory DC + bitmap and PrintWindow into it. Using
        //    PW_RENDERFULLCONTENT (0x2) so Chrome/Electron/UWP layered
        //    surfaces actually paint pixels instead of going black.
        let screen_dc = GetDC(HWND(0));
        if screen_dc.0 == 0 {
            return Err("GetDC failed".to_string());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.0 == 0 {
            ReleaseDC(HWND(0), screen_dc);
            return Err("CreateCompatibleDC failed".to_string());
        }
        let bitmap: HBITMAP = CreateCompatibleBitmap(screen_dc, width, height);
        if bitmap.0 == 0 {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND(0), screen_dc);
            return Err("CreateCompatibleBitmap failed".to_string());
        }
        let prev = SelectObject(mem_dc, HGDIOBJ(bitmap.0));

        let pw_render_fullcontent = PRINT_WINDOW_FLAGS(0x2);
        let ok = PrintWindow(hwnd, mem_dc, pw_render_fullcontent).as_bool();
        if !ok {
            // Some apps refuse PW_RENDERFULLCONTENT — retry without it.
            let _ = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(0)).as_bool();
        }

        // 5. Pull BGRA pixels via GetDIBits.
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height = top-down DIB so row 0 is the top row,
                // which is what `image` expects.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let pixel_count = (width as usize) * (height as usize);
        let mut buf = vec![0u8; pixel_count * 4];
        let scanlines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            height as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        // Cleanup GDI before any early returns.
        SelectObject(mem_dc, prev);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND(0), screen_dc);

        if scanlines == 0 {
            return Err("GetDIBits returned 0 scanlines".to_string());
        }

        // 6. BGRA → RGBA swizzle, drop alpha (some apps leave it 0).
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        for chunk in buf.chunks_exact(4) {
            rgba.push(chunk[2]); // R
            rgba.push(chunk[1]); // G
            rgba.push(chunk[0]); // B
            rgba.push(0xFF);
        }

        // 7. Encode PNG.
        let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
            .ok_or_else(|| "RgbaImage::from_raw size mismatch".to_string())?;
        let mut png_bytes: Vec<u8> = Vec::new();
        {
            use image::codecs::png::PngEncoder;
            use image::ImageEncoder;
            PngEncoder::new(&mut png_bytes)
                .write_image(
                    img.as_raw(),
                    width as u32,
                    height as u32,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| format!("PNG encode: {e}"))?;
        }

        Ok(CapturedWindow {
            hwnd: hwnd.0,
            title,
            process: process_name,
            width: width as u32,
            height: height as u32,
            png: png_bytes,
        })
    }));
    match r {
        Ok(v) => v,
        Err(_) => Err("capture_window panicked in Win32 FFI".to_string()),
    }
}
