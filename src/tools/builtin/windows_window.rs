//! Native window snapping + monitor enumeration. Pairs with
//! `windows_desktop.rs` to give JARVIS the tools needed to lay out
//! mission workspaces (Chrome left, VS Code right, etc.) across one or
//! more monitors.
//!
//! Snapping is implemented via `SetWindowPos` after computing the half
//! of the target monitor's work area to occupy. We do NOT synthesize
//! Win+Left keystrokes — that only works for the foreground window and
//! we want to snap windows that aren't currently focused. Direct
//! positioning is more reliable.

use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

// =============================================================================
// list monitors
// =============================================================================

pub struct WindowsListMonitorsTool;

#[async_trait]
impl Tool for WindowsListMonitorsTool {
    fn name(&self) -> &str {
        "windows_list_monitors"
    }

    fn description(&self) -> &str {
        "Enumerate physical monitors. Returns each monitor's bounds, work-area \
         (screen minus taskbar), DPI scale, and a `primary` flag. JARVIS uses \
         x-coordinate ordering to know which display is 'left' vs 'right' for \
         window snap targeting. Read-only, no approval."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        #[cfg(target_os = "windows")]
        {
            let monitors = tokio::task::spawn_blocking(enumerate_monitors)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
                .map_err(ToolError::ExecutionFailed)?;
            // Index left-to-right by work-area x so JARVIS can say "monitor 0
            // is the leftmost" without having to interpret raw coords.
            let mut sorted = monitors;
            sorted.sort_by_key(|m| m.bounds.0);
            let payload: Vec<serde_json::Value> = sorted
                .into_iter()
                .enumerate()
                .map(|(i, m)| {
                    serde_json::json!({
                        "index": i,
                        "primary": m.primary,
                        "bounds": {
                            "x": m.bounds.0, "y": m.bounds.1,
                            "width": m.bounds.2, "height": m.bounds.3,
                        },
                        "work_area": {
                            "x": m.work_area.0, "y": m.work_area.1,
                            "width": m.work_area.2, "height": m.work_area.3,
                        },
                        "device_name": m.device_name,
                    })
                })
                .collect();
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "count": payload.len(),
                    "monitors": payload,
                }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = start;
            Err(ToolError::NotAuthorized(
                "windows_list_monitors only works on Windows".to_string(),
            ))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// =============================================================================
// snap window
// =============================================================================

pub struct WindowsSnapWindowTool;

#[async_trait]
impl Tool for WindowsSnapWindowTool {
    fn name(&self) -> &str {
        "windows_snap_window"
    }

    fn description(&self) -> &str {
        "Resize and reposition a window to a snap zone on a monitor. Identify \
         the window by `process` name (e.g. 'chrome', 'code') OR \
         `title_contains` substring. Specify `monitor_index` (from \
         windows_list_monitors) and `zone`: 'left' / 'right' / 'full' / \
         'top_left' / 'top_right' / 'bottom_left' / 'bottom_right'. \
         Approval-gated (visibly mutates the screen)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "monitor_index": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based monitor index (from windows_list_monitors, left-to-right)"
                },
                "zone": {
                    "type": "string",
                    "enum": ["left", "right", "full", "top_left", "top_right", "bottom_left", "bottom_right"],
                    "description": "Which half/quadrant of the monitor to fill"
                },
                "process": {
                    "type": "string",
                    "description": "Process name (without .exe), case-insensitive"
                },
                "title_contains": {
                    "type": "string",
                    "description": "Case-insensitive substring of the window title"
                }
            },
            "required": ["monitor_index", "zone"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let monitor_index = params
            .get("monitor_index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'monitor_index'".to_string()))?
            as usize;
        let zone = params
            .get("zone")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'zone'".to_string()))?
            .to_string();
        let process = params
            .get("process")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        let title = params
            .get("title_contains")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());

        if process.is_none() && title.is_none() {
            return Err(ToolError::InvalidParameters(
                "must provide either 'process' or 'title_contains'".to_string(),
            ));
        }

        #[cfg(target_os = "windows")]
        {
            let process_for_blocking = process.clone();
            let title_for_blocking = title.clone();
            let zone_for_blocking = zone.clone();
            // Walk the monitors and the windows in one blocking pass.
            let result = tokio::task::spawn_blocking(move || {
                let monitors = enumerate_monitors().map_err(|e| e.to_string())?;
                let mut sorted = monitors;
                sorted.sort_by_key(|m| m.bounds.0);
                let mon = sorted
                    .get(monitor_index)
                    .ok_or_else(|| format!("monitor_index {monitor_index} out of range ({} known)", sorted.len()))?;
                let rect = zone_rect(&zone_for_blocking, mon)?;
                let (hwnd_raw, title_match) = super::windows_desktop::find_window_handle(
                    process_for_blocking.as_deref(),
                    title_for_blocking.as_deref(),
                )
                .ok_or_else(|| "no matching window found".to_string())?;
                snap_window(hwnd_raw, rect)?;
                Ok::<_, String>((hwnd_raw, title_match, rect))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            return Ok(ToolOutput::success(
                serde_json::json!({
                    "status": "snapped",
                    "hwnd": result.0 as u64,
                    "matched_title": result.1,
                    "monitor_index": monitor_index,
                    "zone": zone,
                    "rect": {
                        "x": result.2.0, "y": result.2.1,
                        "width": result.2.2, "height": result.2.3,
                    },
                }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (monitor_index, zone, process, title, start);
            Err(ToolError::NotAuthorized(
                "windows_snap_window only works on Windows".to_string(),
            ))
        }
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// =============================================================================
// Win32 internals (Windows-only)
// =============================================================================

#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub(crate) struct MonitorInfo {
    pub bounds: (i32, i32, i32, i32),    // x, y, w, h (raw monitor rect)
    pub work_area: (i32, i32, i32, i32), // x, y, w, h (minus taskbar)
    pub primary: bool,
    pub device_name: String,
}

#[cfg(target_os = "windows")]
pub(crate) fn enumerate_monitors() -> Result<Vec<MonitorInfo>, String> {
    use std::cell::RefCell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    // MONITORINFOF_PRIMARY isn't re-exported in windows 0.52 GDI module; use
    // the literal value from MSDN.
    const MONITORINFOF_PRIMARY: u32 = 0x00000001;

    thread_local! {
        static MONS: RefCell<Vec<MonitorInfo>> = const { RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn cb(
        h: HMONITOR,
        _hdc: HDC,
        _r: *mut RECT,
        _lp: LPARAM,
    ) -> BOOL {
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let info_ptr = &mut info as *mut _ as *mut MONITORINFO;
        if !unsafe { GetMonitorInfoW(h, info_ptr) }.as_bool() {
            return TRUE;
        }
        let device_name = String::from_utf16_lossy(
            &info
                .szDevice
                .iter()
                .take_while(|c| **c != 0)
                .copied()
                .collect::<Vec<_>>(),
        );
        let m = MonitorInfo {
            bounds: rect_to_xywh(info.monitorInfo.rcMonitor),
            work_area: rect_to_xywh(info.monitorInfo.rcWork),
            primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
            device_name,
        };
        MONS.with(|v| v.borrow_mut().push(m));
        TRUE
    }

    // Wrap the unsafe FFI in catch_unwind so a Rust panic inside the
    // callback (or a downstream slice/copy issue) returns a clean error
    // instead of unwinding through the extern "system" boundary, which
    // would be UB. Doesn't catch a native access violation from the OS —
    // those bypass Rust's unwind machinery — but it does close the
    // most-likely failure modes.
    let result: Result<Result<(), String>, _> = catch_unwind(AssertUnwindSafe(|| {
        MONS.with(|v| v.borrow_mut().clear());
        unsafe {
            if !EnumDisplayMonitors(HDC(0), None, Some(cb), LPARAM(0)).as_bool() {
                return Err("EnumDisplayMonitors returned false".to_string());
            }
        }
        Ok(())
    }));
    match result {
        Ok(Ok(())) => Ok(MONS.with(|v| v.borrow().clone())),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("enumerate_monitors panicked inside Win32 callback".to_string()),
    }
}

#[cfg(target_os = "windows")]
fn rect_to_xywh(r: windows::Win32::Foundation::RECT) -> (i32, i32, i32, i32) {
    (r.left, r.top, r.right - r.left, r.bottom - r.top)
}

#[cfg(target_os = "windows")]
fn zone_rect(zone: &str, m: &MonitorInfo) -> Result<(i32, i32, i32, i32), String> {
    let (x, y, w, h) = m.work_area;
    let half_w = w / 2;
    let half_h = h / 2;
    Ok(match zone {
        "left" => (x, y, half_w, h),
        "right" => (x + half_w, y, w - half_w, h),
        "full" => (x, y, w, h),
        "top_left" => (x, y, half_w, half_h),
        "top_right" => (x + half_w, y, w - half_w, half_h),
        "bottom_left" => (x, y + half_h, half_w, h - half_h),
        "bottom_right" => (x + half_w, y + half_h, w - half_w, h - half_h),
        other => return Err(format!("unknown zone '{}'", other)),
    })
}

#[cfg(target_os = "windows")]
fn snap_window(hwnd_raw: isize, rect: (i32, i32, i32, i32)) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, SWP_NOZORDER, SW_RESTORE,
    };
    let hwnd = HWND(hwnd_raw);
    unsafe {
        // Restore in case the window is maximized — SetWindowPos on a
        // maximized window silently no-ops on its dimensions.
        ShowWindow(hwnd, SW_RESTORE);
        SetWindowPos(hwnd, HWND(0), rect.0, rect.1, rect.2, rect.3, SWP_NOZORDER)
            .map_err(|e| format!("SetWindowPos: {:?}", e))?;
    }
    Ok(())
}
