//! Native Windows Virtual Desktop control.
//!
//! Talks to the undocumented `IVirtualDesktopManagerInternal` COM interface
//! through the `winvd` crate, which keeps the GUID/vtable mappings in
//! sync with Windows 11 builds. The thin native layer avoids the
//! PowerShell `VirtualDesktop` module dependency and shaves the
//! shell-out latency off every desktop operation.
//!
//! Trade-off (per `WISHLIST.md` discussion): when Microsoft ships a Win11
//! build that shifts the internal COM interface IDs (1–2× per year), the
//! tools throw `winvd`-level errors until that crate updates. The
//! `winvd` crate has been keeping pace; if it ever falls behind, the
//! fallback is to swap to the PowerShell-backed implementation listed in
//! the wishlist for ~30 minutes of work.

use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

// =============================================================================
// list desktops
// =============================================================================

pub struct WindowsListDesktopsTool;

#[async_trait]
impl Tool for WindowsListDesktopsTool {
    fn name(&self) -> &str {
        "windows_list_desktops"
    }

    fn description(&self) -> &str {
        "List Windows 11 virtual desktops. Returns { count, current_index, \
         desktops: [{index, name, current}] }. Read-only, no approval needed."
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
            let (count, current_idx, names) = tokio::task::spawn_blocking(|| {
                let desktops = winvd::get_desktops()
                    .map_err(|e| format!("get_desktops: {:?}", e))?;
                let current = winvd::get_current_desktop()
                    .map_err(|e| format!("get_current_desktop: {:?}", e))?;
                let current_idx = current
                    .get_index()
                    .map_err(|e| format!("current index: {:?}", e))?;
                let mut names = Vec::new();
                for d in &desktops {
                    let n = d.get_name().unwrap_or_default();
                    names.push(n);
                }
                Ok::<_, String>((desktops.len(), current_idx, names))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            let desks: Vec<serde_json::Value> = names
                .into_iter()
                .enumerate()
                .map(|(i, name)| {
                    serde_json::json!({
                        "index": i,
                        "name": if name.is_empty() {
                            format!("Desktop {}", i + 1)
                        } else {
                            name
                        },
                        "current": i as u32 == current_idx,
                    })
                })
                .collect();

            return Ok(ToolOutput::success(
                serde_json::json!({
                    "count": count,
                    "current_index": current_idx,
                    "desktops": desks,
                }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = start;
            Err(ToolError::NotAuthorized(
                "windows_list_desktops only works on Windows".to_string(),
            ))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// =============================================================================
// switch desktop
// =============================================================================

pub struct WindowsSwitchDesktopTool;

#[async_trait]
impl Tool for WindowsSwitchDesktopTool {
    fn name(&self) -> &str {
        "windows_switch_desktop"
    }

    fn description(&self) -> &str {
        "Switch the active Windows 11 virtual desktop by zero-based index. \
         Approval-gated — visibly changes the screen. Call windows_list_desktops \
         first if you don't know how many desktops exist."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "index": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based desktop index to switch to"
                }
            },
            "required": ["index"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let idx = params
            .get("index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'index'".to_string()))?
            as u32;

        #[cfg(target_os = "windows")]
        {
            tokio::task::spawn_blocking(move || {
                let d = winvd::get_desktop(idx);
                winvd::switch_desktop(d).map_err(|e| format!("switch_desktop: {:?}", e))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            return Ok(ToolOutput::success(
                serde_json::json!({ "status": "switched", "index": idx }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (idx, start);
            Err(ToolError::NotAuthorized(
                "windows_switch_desktop only works on Windows".to_string(),
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
// new desktop
// =============================================================================

pub struct WindowsNewDesktopTool;

#[async_trait]
impl Tool for WindowsNewDesktopTool {
    fn name(&self) -> &str {
        "windows_new_desktop"
    }

    fn description(&self) -> &str {
        "Create a new Windows 11 virtual desktop. Optional `name` labels it \
         (visible in Task View). Returns the new desktop's index. Approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Optional human-readable name for the desktop (e.g. 'Eustress', 'Inbox')"
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
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from);

        #[cfg(target_os = "windows")]
        {
            // Windows 11 hard-caps virtual desktops at 30. Refuse the
            // create call instead of letting the OS swallow it silently.
            let existing = tokio::task::spawn_blocking(|| {
                winvd::get_desktops()
                    .map(|d| d.len())
                    .map_err(|e| format!("count: {:?}", e))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;
            if existing >= 30 {
                return Err(ToolError::ExecutionFailed(format!(
                    "Windows 11 caps virtual desktops at 30. You currently have {existing}; \
                     delete one before creating a new desktop."
                )));
            }

            let new_idx = tokio::task::spawn_blocking(move || {
                let d = winvd::create_desktop().map_err(|e| format!("create: {:?}", e))?;
                if let Some(n) = &name {
                    let _ = d.set_name(n);
                }
                d.get_index().map_err(|e| format!("get_index: {:?}", e))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            return Ok(ToolOutput::success(
                serde_json::json!({
                    "status": "created",
                    "index": new_idx,
                }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (name, start);
            Err(ToolError::NotAuthorized(
                "windows_new_desktop only works on Windows".to_string(),
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
// move window to desktop
// =============================================================================

pub struct WindowsMoveWindowToDesktopTool;

#[async_trait]
impl Tool for WindowsMoveWindowToDesktopTool {
    fn name(&self) -> &str {
        "windows_move_window_to_desktop"
    }

    fn description(&self) -> &str {
        "Move a window to a different Windows 11 virtual desktop. Identify the \
         window by `process` (process name, e.g. 'chrome', 'code') OR \
         `title_contains` (case-insensitive title substring). If multiple \
         matches exist, the first visible top-level window wins. Approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "desktop_index": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Zero-based desktop index to move the window onto"
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
            "required": ["desktop_index"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let idx = params
            .get("desktop_index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'desktop_index'".to_string()))?
            as u32;
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
            let (hwnd_raw, matched_title) = tokio::task::spawn_blocking(move || {
                find_window_handle(process_for_blocking.as_deref(), title_for_blocking.as_deref())
                    .ok_or_else(|| "no matching window found".to_string())
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            tokio::task::spawn_blocking(move || {
                let d = winvd::get_desktop(idx);
                let hwnd = windows::Win32::Foundation::HWND(hwnd_raw);
                winvd::move_window_to_desktop(d, &hwnd)
                    .map_err(|e| format!("move_window_to_desktop: {:?}", e))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            return Ok(ToolOutput::success(
                serde_json::json!({
                    "status": "moved",
                    "desktop_index": idx,
                    "matched_title": matched_title,
                    "hwnd": hwnd_raw as u64,
                }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (idx, process, title, start);
            Err(ToolError::NotAuthorized(
                "windows_move_window_to_desktop only works on Windows".to_string(),
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
// Win32 helpers — only compiled on Windows
// =============================================================================

#[cfg(target_os = "windows")]
pub(crate) fn find_window_handle(
    process_lower: Option<&str>,
    title_lower: Option<&str>,
) -> Option<(isize, String)> {
    use std::cell::RefCell;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
    use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible,
    };

    struct State<'a> {
        process_lower: Option<&'a str>,
        title_lower: Option<&'a str>,
        found: Option<(isize, String)>,
    }

    thread_local! {
        static STATE: RefCell<Option<*mut State<'static>>> = const { RefCell::new(None) };
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return TRUE;
            }
            // Read window title.
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return TRUE;
            }
            let mut buf = vec![0u16; (len as usize) + 1];
            let copied = GetWindowTextW(hwnd, &mut buf);
            if copied <= 0 {
                return TRUE;
            }
            let title = String::from_utf16_lossy(&buf[..copied as usize]);
            let title_lower = title.to_ascii_lowercase();

            // Look up owning process module name.
            let mut pid: u32 = 0;
            let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let proc_name = if pid != 0 {
                match OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                    false,
                    pid,
                ) {
                    Ok(handle) => {
                        let mut pname = [0u16; 260];
                        let n = GetModuleBaseNameW(handle, None, &mut pname);
                        let _ = windows::Win32::Foundation::CloseHandle(handle);
                        if n > 0 {
                            let s = String::from_utf16_lossy(&pname[..n as usize]);
                            s.trim_end_matches(".exe").to_ascii_lowercase()
                        } else {
                            String::new()
                        }
                    }
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            };

            STATE.with(|s| {
                let ptr = match *s.borrow() {
                    Some(p) => p,
                    None => return,
                };
                let state = &mut *ptr;
                if state.found.is_some() {
                    return;
                }
                let proc_ok = state
                    .process_lower
                    .as_ref()
                    .map(|q| !proc_name.is_empty() && proc_name.contains(*q))
                    .unwrap_or(true);
                let title_ok = state
                    .title_lower
                    .as_ref()
                    .map(|q| title_lower.contains(*q))
                    .unwrap_or(true);
                // If neither filter is set, don't claim a match — we already
                // bailed at the caller in that case, but defensive guard.
                let any_filter =
                    state.process_lower.is_some() || state.title_lower.is_some();
                if any_filter && proc_ok && title_ok {
                    state.found = Some((hwnd.0 as isize, title));
                }
            });

            TRUE
        }
    }

    let mut state = State {
        process_lower,
        title_lower,
        found: None,
    };
    // SAFETY: STATE's lifetime is bound to this call. We clear it before
    // returning, so the 'static cast never escapes the function.
    let state_ptr: *mut State<'static> = unsafe { std::mem::transmute(&mut state as *mut State) };
    STATE.with(|s| *s.borrow_mut() = Some(state_ptr));
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }
    STATE.with(|s| *s.borrow_mut() = None);

    state.found
}
