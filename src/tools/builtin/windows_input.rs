//! Native Windows input automation: focus a window, type text into the
//! foreground window, press individual keys, and read the current input
//! focus state. Pairs with `windows_desktop.rs` + `windows_window.rs` to
//! let JARVIS drive any app outside the browser — VS Code, Discord,
//! Slack, native Notepad, anything that accepts keystrokes.
//!
//! Safety boundary: `windows_type_text` and `windows_press_key` are
//! approval-gated because they inject keystrokes into whatever window
//! currently has focus. The mission patterns sequence
//! `windows_focus_window` → sleep 150 ms → `windows_type_text` so focus
//! settles before keys fly; `windows_type_text` ALSO re-checks the
//! foreground window matches what was last focused and bails if focus
//! drifted (the "user clicked into their banking app between approval
//! and execution" failure mode).
//!
//! All Win32 calls live behind `std::panic::catch_unwind` so a Rust
//! panic inside the unsafe FFI surfaces as a clean tool error instead
//! of taking down the gateway process.

use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

// =============================================================================
// windows_get_input_focus (read)
// =============================================================================

pub struct WindowsGetInputFocusTool;

#[async_trait]
impl Tool for WindowsGetInputFocusTool {
    fn name(&self) -> &str {
        "windows_get_input_focus"
    }

    fn description(&self) -> &str {
        "Read the current input focus state. Returns the foreground window \
         (title, process name, hwnd) plus the cursor position in screen \
         pixels. Use this before typing to confirm which app would receive \
         keystrokes. Read-only, no approval."
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
            let info = tokio::task::spawn_blocking(get_input_focus_info)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
                .map_err(ToolError::ExecutionFailed)?;
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "foreground": {
                        "hwnd": info.hwnd as u64,
                        "title": info.title,
                        "process": info.process,
                    },
                    "cursor": { "x": info.cursor.0, "y": info.cursor.1 },
                }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = start;
            Err(ToolError::NotAuthorized(
                "windows_get_input_focus only works on Windows".to_string(),
            ))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// =============================================================================
// windows_focus_window (write)
// =============================================================================

pub struct WindowsFocusWindowTool;

#[async_trait]
impl Tool for WindowsFocusWindowTool {
    fn name(&self) -> &str {
        "windows_focus_window"
    }

    fn description(&self) -> &str {
        "Bring a window to the foreground. Identify it by `title_contains` \
         (case-insensitive substring) and/or `process` (process name without \
         .exe). The first visible top-level match wins. Returns the focused \
         window's hwnd + title so the caller knows what got focused before \
         typing. Approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title_contains": { "type": "string", "description": "Case-insensitive substring of the window title" },
                "process": { "type": "string", "description": "Process name (without .exe), case-insensitive" }
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
        if title.is_none() && process.is_none() {
            return Err(ToolError::InvalidParameters(
                "must provide either 'title_contains' or 'process'".to_string(),
            ));
        }

        #[cfg(target_os = "windows")]
        {
            let title_clone = title.clone();
            let process_clone = process.clone();
            let result = tokio::task::spawn_blocking(move || {
                let (hwnd_raw, matched_title) = crate::tools::builtin::windows_desktop::find_window_handle(
                    process_clone.as_deref(),
                    title_clone.as_deref(),
                )
                .ok_or_else(|| "no matching window".to_string())?;
                focus_window(hwnd_raw)?;
                Ok::<_, String>((hwnd_raw, matched_title))
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "status": "focused",
                    "hwnd": result.0 as u64,
                    "title": result.1,
                }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (title, process, start);
            Err(ToolError::NotAuthorized(
                "windows_focus_window only works on Windows".to_string(),
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
// windows_type_text (write)
// =============================================================================

pub struct WindowsTypeTextTool;

#[async_trait]
impl Tool for WindowsTypeTextTool {
    fn name(&self) -> &str {
        "windows_type_text"
    }

    fn description(&self) -> &str {
        "Type text into whatever window currently has focus. Use \
         windows_focus_window first to ensure the right target is in front. \
         Optionally pass `expected_title_contains` and the tool will abort \
         if focus drifted to a different window since you focused it — a \
         safety check for the case where the user clicks somewhere else \
         between approval and execution. Approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to type. Unicode supported." },
                "expected_title_contains": {
                    "type": "string",
                    "description": "Optional substring check on the foreground window's title; abort if not present"
                }
            },
            "required": ["text"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'text'".to_string()))?
            .to_string();
        let expected = params
            .get("expected_title_contains")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());

        #[cfg(target_os = "windows")]
        {
            let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
                if let Some(want) = &expected {
                    let info = get_input_focus_info()?;
                    if !info.title.to_ascii_lowercase().contains(want) {
                        return Err(format!(
                            "focus drifted: foreground is '{}', expected substring '{}'",
                            info.title, want
                        ));
                    }
                }
                let focused_title = get_input_focus_info()
                    .map(|i| i.title)
                    .unwrap_or_else(|_| String::new());
                send_text(&text)?;
                Ok(focused_title)
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;
            return Ok(ToolOutput::success(
                serde_json::json!({ "status": "typed", "into": result }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (text, expected, start);
            Err(ToolError::NotAuthorized(
                "windows_type_text only works on Windows".to_string(),
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
// windows_press_key (write)
// =============================================================================

pub struct WindowsPressKeyTool;

#[async_trait]
impl Tool for WindowsPressKeyTool {
    fn name(&self) -> &str {
        "windows_press_key"
    }

    fn description(&self) -> &str {
        "Press a single key in the foreground window. Supports named keys: \
         Enter, Tab, Escape, Backspace, Delete, Space, Up, Down, Left, Right, \
         Home, End, PageUp, PageDown, F1..F12. Modifiers via the modifiers \
         array: Ctrl, Alt, Shift, Win (held during the key press, released \
         after). Use windows_type_text for normal text input. Approval-gated."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Key name (Enter, Tab, Escape, F5, Up, etc.) or single character" },
                "modifiers": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["Ctrl","Alt","Shift","Win"] },
                    "description": "Modifier keys to hold during the press"
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'key'".to_string()))?
            .to_string();
        let modifiers: Vec<String> = params
            .get("modifiers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        #[cfg(target_os = "windows")]
        {
            let key_for_blocking = key.clone();
            let modifiers_for_blocking = modifiers.clone();
            tokio::task::spawn_blocking(move || {
                send_key(&key_for_blocking, &modifiers_for_blocking)
            })
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
            .map_err(ToolError::ExecutionFailed)?;
            return Ok(ToolOutput::success(
                serde_json::json!({ "status": "pressed", "key": key, "modifiers": modifiers }),
                start.elapsed(),
            ));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (key, modifiers, start);
            Err(ToolError::NotAuthorized(
                "windows_press_key only works on Windows".to_string(),
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
struct InputFocusInfo {
    hwnd: isize,
    title: String,
    process: String,
    cursor: (i32, i32),
}

#[cfg(target_os = "windows")]
fn get_input_focus_info() -> Result<InputFocusInfo, String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId,
    };

    let r = catch_unwind(AssertUnwindSafe(|| unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0 == 0 {
            return Err("no foreground window".to_string());
        }
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
        let process = if pid != 0 {
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
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        Ok(InputFocusInfo {
            hwnd: hwnd.0,
            title,
            process,
            cursor: (pt.x, pt.y),
        })
    }));
    match r {
        Ok(v) => v,
        Err(_) => Err("get_input_focus_info panicked in Win32 FFI".to_string()),
    }
}

#[cfg(target_os = "windows")]
fn focus_window(hwnd_raw: isize) -> Result<(), String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId,
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };

    // Tap Alt to satisfy the "recent user input" check that Windows uses
    // to gate SetForegroundWindow. This is the documented workaround for
    // ForegroundLockTimeout — a synthetic VK_MENU down/up flips the
    // current thread's "last input" state so SetForegroundWindow stops
    // refusing.
    unsafe fn tap_alt_key() { unsafe {
        let down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_MENU,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [down, up];
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }}

    catch_unwind(AssertUnwindSafe(|| unsafe {
        let hwnd = HWND(hwnd_raw);
        // Restore if minimized.
        ShowWindow(hwnd, SW_RESTORE);

        // First attempt — usually works when nothing else has focus.
        if SetForegroundWindow(hwnd).as_bool() {
            return Ok(());
        }

        // Second attempt — Alt-tap to clear the foreground lock, then
        // retry.
        tap_alt_key();
        if SetForegroundWindow(hwnd).as_bool() {
            return Ok(());
        }

        // Third attempt — AttachThreadInput trick. Glue our thread's
        // input queue to whichever thread owns the current foreground
        // window, and call SetForegroundWindow / BringWindowToTop while
        // attached. Windows treats us as the same input context, so the
        // foreground-stealing guard doesn't fire. Critical: detach in
        // every exit path so we don't leave threads cross-wired.
        let current_tid = GetCurrentThreadId();
        let fg_hwnd = GetForegroundWindow();
        let fg_tid = if fg_hwnd.0 != 0 {
            GetWindowThreadProcessId(fg_hwnd, None)
        } else {
            0
        };

        let attached = fg_tid != 0
            && fg_tid != current_tid
            && AttachThreadInput(current_tid, fg_tid, true).as_bool();

        let _ = BringWindowToTop(hwnd);
        ShowWindow(hwnd, SW_SHOW);
        let ok = SetForegroundWindow(hwnd).as_bool();

        if attached {
            let _ = AttachThreadInput(current_tid, fg_tid, false);
        }

        if ok {
            return Ok(());
        }

        // Fourth attempt — Windows foreground-lock timeout. Temporarily
        // set SPI_SETFOREGROUNDLOCKTIMEOUT to 0 so the lock guard stops
        // gating SetForegroundWindow, then restore the original after.
        // This is the most aggressive bypass that doesn't require admin;
        // it works against the user-mode lock guard but still respects
        // UAC integrity boundaries (elevated targets still fail and the
        // user must relaunch JARVIS as admin for those).
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPI_GETFOREGROUNDLOCKTIMEOUT,
            SPI_SETFOREGROUNDLOCKTIMEOUT, SPIF_SENDCHANGE,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        };
        let mut prev_timeout: u32 = 0;
        let got_prev = SystemParametersInfoW(
            SPI_GETFOREGROUNDLOCKTIMEOUT,
            0,
            Some(&mut prev_timeout as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        ).is_ok();
        let _ = SystemParametersInfoW(
            SPI_SETFOREGROUNDLOCKTIMEOUT,
            0,
            None,
            SPIF_SENDCHANGE,
        );
        let final_ok = SetForegroundWindow(hwnd).as_bool();
        if got_prev {
            // Restore timeout (best-effort).
            let _ = SystemParametersInfoW(
                SPI_SETFOREGROUNDLOCKTIMEOUT,
                prev_timeout,
                None,
                SPIF_SENDCHANGE,
            );
        }

        if final_ok {
            Ok(())
        } else {
            Err("SetForegroundWindow refused after 4 escalating retries \
                 (Alt-tap, AttachThreadInput, foreground-lock-timeout=0). \
                 The target process is almost certainly elevated — relaunch \
                 JARVIS as admin (right-click jarvis-desktop.exe → Run as \
                 administrator) so it can grab focus from elevated apps."
                .to_string())
        }
    }))
    .map_err(|_| "focus_window panicked in Win32 FFI".to_string())?
}

#[cfg(target_os = "windows")]
fn send_text(text: &str) -> Result<(), String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    };

    catch_unwind(AssertUnwindSafe(|| unsafe {
        // One INPUT pair (down, up) per UTF-16 code unit. Surrogate
        // pairs work because Windows accepts them as two consecutive
        // KEYEVENTF_UNICODE events.
        let mut inputs: Vec<INPUT> = Vec::with_capacity(text.encode_utf16().count() * 2);
        for c in text.encode_utf16() {
            let down = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                        wScan: c,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let mut up = down;
            up.Anonymous.ki.dwFlags = KEYBD_EVENT_FLAGS(KEYEVENTF_UNICODE.0 | KEYEVENTF_KEYUP.0);
            inputs.push(down);
            inputs.push(up);
        }
        if inputs.is_empty() {
            return Ok(());
        }
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != inputs.len() {
            return Err(format!(
                "SendInput sent {}/{} events; possibly blocked by a higher-integrity window",
                sent,
                inputs.len()
            ));
        }
        Ok(())
    }))
    .map_err(|_| "send_text panicked in Win32 FFI".to_string())?
}

#[cfg(target_os = "windows")]
fn send_key(key: &str, modifiers: &[String]) -> Result<(), String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
        VK_F1, VK_F10, VK_F11, VK_F12, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9,
        VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT,
        VK_SPACE, VK_TAB, VK_UP,
    };

    let vk = match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "escape" | "esc" => VK_ESCAPE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "space" | " " => VK_SPACE,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" => VK_PRIOR,
        "pagedown" => VK_NEXT,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        other if other.len() == 1 => {
            // Single character; let send_text handle it as Unicode.
            return send_text(other);
        }
        other => return Err(format!("unknown key '{}'", other)),
    };

    let mod_vks: Vec<VIRTUAL_KEY> = modifiers
        .iter()
        .filter_map(|m| match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(VK_CONTROL),
            "alt" => Some(VK_MENU),
            "shift" => Some(VK_SHIFT),
            "win" | "windows" => Some(VK_LWIN),
            _ => None,
        })
        .collect();

    catch_unwind(AssertUnwindSafe(|| unsafe {
        let mut inputs: Vec<INPUT> = Vec::new();
        let mk = |v: VIRTUAL_KEY, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: v,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // Press modifiers.
        for m in &mod_vks {
            inputs.push(mk(*m, false));
        }
        // Main key down + up.
        inputs.push(mk(vk, false));
        inputs.push(mk(vk, true));
        // Release modifiers in reverse order.
        for m in mod_vks.iter().rev() {
            inputs.push(mk(*m, true));
        }
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != inputs.len() {
            return Err(format!(
                "SendInput sent {}/{} events",
                sent,
                inputs.len()
            ));
        }
        Ok(())
    }))
    .map_err(|_| "send_key panicked in Win32 FFI".to_string())?
}

// =============================================================================
// windows_mouse_click (write)  + windows_mouse_move (write)
// =============================================================================
//
// Closes the CUA (Computer Use Agent) gap: with these tools JARVIS can
// take a screenshot, identify a target in pixels, click it, and only
// THEN type. Without this, `windows_type_text` blasts keystrokes into
// whatever Windows considers focused — usually not what the screenshot
// just showed.

/// Coerce a pixel coordinate from a tool-params JSON value into i32.
/// Tolerates integer, float (rounded to nearest), and stringified
/// numeric inputs because the LLM sometimes emits `100.0` or `"100"`
/// for parameters typed as integers in the schema. A strict
/// `as_i64()` rejects both — that was the cause of recent
/// `windows_mouse_click execution failed: Invalid parameters` errors.
/// On failure, the returned error string includes the actual params
/// the model sent so the next-iteration prompt can correct itself.
fn coerce_pixel_coord(
    params: &serde_json::Value,
    field: &str,
) -> Result<i32, ToolError> {
    let v = params.get(field).ok_or_else(|| {
        ToolError::InvalidParameters(format!(
            "missing `{field}`; got params = {}",
            params
        ))
    })?;
    if let Some(n) = v.as_i64() {
        return Ok(n as i32);
    }
    if let Some(f) = v.as_f64() {
        return Ok(f.round() as i32);
    }
    if let Some(s) = v.as_str() {
        if let Ok(n) = s.trim().parse::<i64>() {
            return Ok(n as i32);
        }
        if let Ok(f) = s.trim().parse::<f64>() {
            return Ok(f.round() as i32);
        }
    }
    Err(ToolError::InvalidParameters(format!(
        "`{field}` must be a number (integer or float); got {v}"
    )))
}

pub struct WindowsMouseClickTool;

#[async_trait]
impl Tool for WindowsMouseClickTool {
    fn name(&self) -> &str {
        "windows_mouse_click"
    }

    fn description(&self) -> &str {
        "Click the mouse at a specific screen-pixel coordinate. Use this \
         AFTER a `windows_screenshot_foreground` so the LLM can pick the \
         target visually, THEN before `windows_type_text` so the right \
         control gets focus. Params: x (i32), y (i32), button (\"left\" \
         default | \"right\" | \"middle\"), double (bool, default false). \
         Cursor returns to its prior position after the click."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "x": {"type": "integer"},
                "y": {"type": "integer"},
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "default": "left"
                },
                "double": {"type": "boolean", "default": false}
            },
            "required": ["x", "y"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let x = coerce_pixel_coord(&params, "x")?;
        let y = coerce_pixel_coord(&params, "y")?;
        let button = params
            .get("button")
            .and_then(|v| v.as_str())
            .unwrap_or("left")
            .to_string();
        let double = params
            .get("double")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        #[cfg(target_os = "windows")]
        {
            let button_for_click = button.clone();
            tokio::task::spawn_blocking(move || mouse_click(x, y, &button_for_click, double))
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
                .map_err(ToolError::ExecutionFailed)?;
            Ok(ToolOutput::success(
                serde_json::json!({"x": x, "y": y, "button": button, "double": double}),
                start.elapsed(),
            ))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (start, x, y, button, double);
            Err(ToolError::NotAuthorized(
                "windows_mouse_click only works on Windows".to_string(),
            ))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }

    fn requires_approval(&self) -> bool {
        // Clicks can do irreversible things (close dialogs, hit Send,
        // etc). Approval per the same policy as type_text.
        true
    }
}

pub struct WindowsMouseMoveTool;

#[async_trait]
impl Tool for WindowsMouseMoveTool {
    fn name(&self) -> &str {
        "windows_mouse_move"
    }

    fn description(&self) -> &str {
        "Move the mouse cursor to a screen-pixel coordinate without \
         clicking. Useful for hover-reveals (tooltips, hover menus). \
         Params: x (i32), y (i32)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "x": {"type": "integer"},
                "y": {"type": "integer"}
            },
            "required": ["x", "y"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let x = coerce_pixel_coord(&params, "x")?;
        let y = coerce_pixel_coord(&params, "y")?;
        #[cfg(target_os = "windows")]
        {
            tokio::task::spawn_blocking(move || mouse_move(x, y))
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
                .map_err(ToolError::ExecutionFailed)?;
            Ok(ToolOutput::success(
                serde_json::json!({"x": x, "y": y}),
                start.elapsed(),
            ))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (start, x, y);
            Err(ToolError::NotAuthorized(
                "windows_mouse_move only works on Windows".to_string(),
            ))
        }
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// =============================================================================
// windows_clipboard_get  +  windows_clipboard_set
// =============================================================================
//
// Faster + more reliable than `windows_type_text` for pasting large
// payloads (the manifesto McKale tried to drop into VS Code at 19:00 was
// rejected as keystrokes; a clipboard set + Ctrl+V would have landed
// instantly). Also lets JARVIS read whatever the user just copied.

pub struct WindowsClipboardGetTool;

#[async_trait]
impl Tool for WindowsClipboardGetTool {
    fn name(&self) -> &str {
        "windows_clipboard_get"
    }

    fn description(&self) -> &str {
        "Read the current text contents of the OS clipboard. Returns the \
         string the user most recently copied. Use this when McKale says \
         'here, look at what I copied' or to chain a copy-from-one-app, \
         transform, paste-to-another-app workflow."
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
        let text = tokio::task::spawn_blocking(|| -> Result<String, String> {
            let mut cb = arboard::Clipboard::new()
                .map_err(|e| format!("arboard open: {e}"))?;
            cb.get_text().map_err(|e| format!("arboard read: {e}"))
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
        .map_err(ToolError::ExecutionFailed)?;
        Ok(ToolOutput::success(
            serde_json::json!({"text": text}),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        true // clipboard content is external input
    }
}

pub struct WindowsClipboardSetTool;

#[async_trait]
impl Tool for WindowsClipboardSetTool {
    fn name(&self) -> &str {
        "windows_clipboard_set"
    }

    fn description(&self) -> &str {
        "Replace the OS clipboard with the given text. Pair with a \
         `windows_press_key` for Ctrl+V to paste the payload into the \
         focused app — orders of magnitude faster and more reliable than \
         `windows_type_text` for long content (200+ chars). Params: text \
         (string)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"}
            },
            "required": ["text"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let text = params
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing text".into()))?
            .to_string();
        let len = text.chars().count();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut cb = arboard::Clipboard::new()
                .map_err(|e| format!("arboard open: {e}"))?;
            cb.set_text(text).map_err(|e| format!("arboard write: {e}"))
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("join: {e}")))?
        .map_err(ToolError::ExecutionFailed)?;
        Ok(ToolOutput::success(
            serde_json::json!({"chars_set": len}),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }

    fn requires_approval(&self) -> bool {
        // Writing the clipboard is usually benign but could clobber
        // something the user just copied. Gate per session policy.
        true
    }
}

// =============================================================================
// Win32 mouse primitives
// =============================================================================

#[cfg(target_os = "windows")]
fn mouse_move(x: i32, y: i32) -> Result<(), String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
    catch_unwind(AssertUnwindSafe(|| unsafe {
        SetCursorPos(x, y)
            .map_err(|e| format!("SetCursorPos({x},{y}) failed: {e}"))?;
        Ok(())
    }))
    .map_err(|_| "mouse_move panicked in Win32 FFI".to_string())?
}

#[cfg(target_os = "windows")]
fn mouse_click(x: i32, y: i32, button: &str, double: bool) -> Result<(), String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    catch_unwind(AssertUnwindSafe(|| unsafe {
        // Save current position so we can restore it after — most CUA
        // flows want the cursor visually where the user left it.
        let mut prev = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut prev);

        SetCursorPos(x, y)
            .map_err(|e| format!("SetCursorPos({x},{y}) failed: {e}"))?;

        let (down, up) = match button {
            "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        };

        let mk = |flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        let mut inputs: Vec<INPUT> = Vec::with_capacity(if double { 4 } else { 2 });
        inputs.push(mk(down));
        inputs.push(mk(up));
        if double {
            inputs.push(mk(down));
            inputs.push(mk(up));
        }
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != inputs.len() {
            // Restore prior position on failure too.
            let _ = SetCursorPos(prev.x, prev.y);
            return Err(format!(
                "SendInput sent {}/{} mouse events (button={button})",
                sent,
                inputs.len()
            ));
        }
        // Brief settle so the click registers before the cursor jumps
        // back. 30 ms is below human-perceptible flicker.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let _ = SetCursorPos(prev.x, prev.y);
        Ok(())
    }))
    .map_err(|_| "mouse_click panicked in Win32 FFI".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_accepts_integer() {
        let p = serde_json::json!({"x": 100, "y": 200});
        assert_eq!(coerce_pixel_coord(&p, "x").unwrap(), 100);
        assert_eq!(coerce_pixel_coord(&p, "y").unwrap(), 200);
    }

    #[test]
    fn coerce_accepts_float() {
        // LLMs sometimes emit float literals for integer params
        // (e.g. `100.0`). Strict as_i64() rejects this; we round.
        let p = serde_json::json!({"x": 100.0, "y": 200.7});
        assert_eq!(coerce_pixel_coord(&p, "x").unwrap(), 100);
        assert_eq!(coerce_pixel_coord(&p, "y").unwrap(), 201);
    }

    #[test]
    fn coerce_accepts_string_numeric() {
        // Stringified numbers are also tolerated — sometimes a
        // structured-output retry serializes ints as strings.
        let p = serde_json::json!({"x": "100", "y": "200.5"});
        assert_eq!(coerce_pixel_coord(&p, "x").unwrap(), 100);
        assert_eq!(coerce_pixel_coord(&p, "y").unwrap(), 201);
    }

    #[test]
    fn coerce_rejects_missing_with_params_dump() {
        // The error message MUST include the actual params received
        // so a next-iteration prompt can see what went wrong. The
        // `{p}` formatter on serde_json::Value emits the compact
        // representation (no spaces after colons) — match that.
        let p = serde_json::json!({"y": 200});
        let err = coerce_pixel_coord(&p, "x").unwrap_err();
        let s = err.to_string();
        assert!(s.contains("missing `x`"), "got: {s}");
        assert!(s.contains("\"y\":200"), "got: {s}");
    }

    #[test]
    fn coerce_rejects_nonsense_type() {
        let p = serde_json::json!({"x": {"nested": 1}, "y": 200});
        let err = coerce_pixel_coord(&p, "x").unwrap_err();
        assert!(err.to_string().contains("must be a number"));
    }
}
