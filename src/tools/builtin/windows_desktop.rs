//! Native Windows Virtual Desktop control via the `winvd_helper`
//! subprocess.
//!
//! winvd's undocumented COM calls into `IVirtualDesktopManagerInternal`
//! throw 0xC0000005 access violations whenever the Windows 11 COM
//! interface IDs shift between builds (1-2x per year, plus the gap
//! while the winvd crate catches up). Those are SEH exceptions, NOT
//! Rust panics — `catch_unwind` cannot trap them, and they take down
//! the entire ironclad process: every Playwright instance, the
//! autonomous loop, voice, all of it.
//!
//! The fix is process isolation. The `winvd_helper.exe` sibling binary
//! does the COM call and prints JSON. We exec it for every desktop
//! operation. A crash there means a non-zero exit code here — we
//! surface a clean ToolError. The parent never dies.
//!
//! The Win32 `find_window_handle` helper at the bottom of this file is
//! NOT a winvd call (it's plain EnumWindows + GetWindowText) and stays
//! in-process. windows_input.rs and windows_window.rs use it too.

use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

// =============================================================================
// helper subprocess dispatch
// =============================================================================

/// Invoke the `winvd_helper` sibling binary with the given args, capture
/// stdout (single-line JSON), parse it. Non-zero exit code surfaces as
/// a `ToolError::ExecutionFailed` with the helper's stderr; 0xC0000005
/// crashes inside the helper produce a distinct error message so the
/// LLM (and the user reading logs) knows what just happened.
#[cfg(target_os = "windows")]
async fn run_helper(args: Vec<String>) -> Result<serde_json::Value, ToolError> {
    let exe = helper_exe_path()?;
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&exe).args(&args).output()
    })
    .await
    .map_err(|e| ToolError::ExecutionFailed(format!("spawn join: {e}")))?
    .map_err(|e| ToolError::ExecutionFailed(format!("spawn winvd_helper: {e}")))?;

    if !output.status.success() {
        // Distinguish a logical error (helper printed to stderr + exit 1)
        // from a COM crash (no stderr, exit code is the raw exception code
        // or a negative number Windows surfaces as i32::from(u32) bits).
        let code = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let reason = match (code, stderr.as_str()) {
            (Some(1), s) if !s.is_empty() => format!("winvd_helper error: {s}"),
            (Some(c), s) if !s.is_empty() => format!("winvd_helper exit {c}: {s}"),
            (Some(c), _) => format!(
                "winvd_helper crashed with exit code {c} (0x{:08X}) — likely a COM \
                 access violation from a Windows 11 build shift. Restart usually \
                 clears it; if it persists, the winvd crate needs a bump.",
                c as u32
            ),
            (None, s) => format!("winvd_helper killed by signal: {s}"),
        };
        return Err(ToolError::ExecutionFailed(reason));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .map_err(|e| ToolError::ExecutionFailed(format!("parse helper output: {e}; raw: {stdout}")))
}

/// Resolve the path to the `winvd_helper.exe` sibling binary. It's built
/// alongside `ironclad.exe` and `jarvis_up.exe` in the same target dir.
/// Allow override via `IRONCLAD_WINVD_HELPER` if a packaging build needs
/// to drop the helper somewhere else.
#[cfg(target_os = "windows")]
fn helper_exe_path() -> Result<std::path::PathBuf, ToolError> {
    if let Ok(p) = std::env::var("IRONCLAD_WINVD_HELPER") {
        return Ok(std::path::PathBuf::from(p));
    }
    let current = std::env::current_exe()
        .map_err(|e| ToolError::ExecutionFailed(format!("current_exe: {e}")))?;
    let dir = current
        .parent()
        .ok_or_else(|| ToolError::ExecutionFailed("current_exe has no parent".to_string()))?;
    Ok(dir.join("winvd_helper.exe"))
}

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
         desktops: [{index, name, current}] }. Runs in an isolated \
         subprocess so a winvd COM crash can't take down the gateway. \
         Read-only, no approval needed."
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
            let v = run_helper(vec!["list".to_string()]).await?;
            let count = v.get("count").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let current_idx = v.get("current_idx").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let names: Vec<String> = v
                .get("names")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|n| n.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

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
         first if you don't know how many desktops exist. Runs through an \
         isolated subprocess so a COM crash won't kill the gateway."
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
            run_helper(vec!["switch".to_string(), idx.to_string()]).await?;
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
         (visible in Task View). Returns the new desktop's index. \
         Approval-gated. Subprocess-isolated so a COM crash won't kill \
         the gateway. The 30-desktop Win11 cap is enforced inside the \
         helper."
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
            let mut args = vec!["create".to_string()];
            if let Some(n) = &name {
                args.push(n.clone());
            }
            let v = run_helper(args).await?;
            let new_idx = v.get("idx").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "status": "created",
                    "index": new_idx,
                    "name": name,
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
         matches exist, the first visible top-level window wins. \
         Approval-gated. Subprocess-isolated for the COM call portion."
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
            // Step 1: find the hwnd in-process (Win32 EnumWindows, no
            // winvd involved, so it's safe to run here).
            let process_for_blocking = process.clone();
            let title_for_blocking = title.clone();
            let (hwnd_raw, matched_title) = tokio::task::spawn_blocking(move || {
                find_window_handle(process_for_blocking.as_deref(), title_for_blocking.as_deref())
                    .ok_or_else(|| "no matching window found".to_string())
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;

            // Step 2: hand the hwnd + desktop index to the subprocess.
            run_helper(vec![
                "move".to_string(),
                hwnd_raw.to_string(),
                idx.to_string(),
            ])
            .await?;

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
// Win32 helpers — only compiled on Windows. NOT a winvd call: pure
// EnumWindows + GetWindowText, safe to run in-process. Used by
// windows_input.rs and windows_window.rs as well.
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
                let any_filter =
                    state.process_lower.is_some() || state.title_lower.is_some();
                if any_filter && proc_ok && title_ok {
                    state.found = Some((hwnd.0 as isize, title));
                }
            });

            TRUE
        }
    }

    use std::panic::{catch_unwind, AssertUnwindSafe};

    let mut state = State {
        process_lower,
        title_lower,
        found: None,
    };
    let state_ptr: *mut State<'static> = unsafe { std::mem::transmute(&mut state as *mut State) };
    STATE.with(|s| *s.borrow_mut() = Some(state_ptr));
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }));
    STATE.with(|s| *s.borrow_mut() = None);

    state.found
}
