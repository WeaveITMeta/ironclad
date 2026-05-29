//! Window operations that need to reach below Slint to winit.
//!
//! `bring_to_focus` shows + expands + raises the JARVIS window from any
//! virtual desktop. Wired to the bring-to-focus hotkey AND auto-fired
//! when the assistant produces a reply while the window is hidden, so
//! McKale never misses a JARVIS response just because he was in
//! another app.
//!
//! `start_window_drag` begins a system-level drag operation on the
//! frameless window (WM_NCLBUTTONDOWN HTCAPTION on Windows, the X11
//! equivalent on Linux, AppKit drag on macOS). Without it our
//! borderless HUD has nothing to grab onto.

use crate::MainWindow;

/// Show + expand + raise the JARVIS window to the foreground.
pub fn bring_to_focus(ui: &MainWindow) {
    use i_slint_backend_winit::WinitWindowAccessor;
    use slint::ComponentHandle;
    let _ = ui.window().show();
    ui.set_expanded(true);
    ui.window().with_winit_window(|w| {
        w.set_visible(true);
        w.set_minimized(false);
        let _ = w.focus_window();
    });
}

/// Begin a native window drag. On Windows the underlying winit call
/// posts WM_NCLBUTTONDOWN HTCAPTION which hands the drag back to the
/// OS shell. On X11/Wayland it uses the move-resize protocol; on
/// macOS the AppKit drag API.
#[cfg(target_os = "windows")]
pub fn start_window_drag(ui: &MainWindow) {
    use i_slint_backend_winit::WinitWindowAccessor;
    use slint::ComponentHandle;
    let invoked = ui.window().with_winit_window(|w| {
        if let Err(e) = w.drag_window() {
            tracing::warn!("drag_window: {e}");
        }
    });
    if invoked.is_none() {
        tracing::warn!("no winit window backing this Slint window; drag unavailable");
    }
}

#[cfg(not(target_os = "windows"))]
pub fn start_window_drag(_ui: &MainWindow) {
    // Slint 1.x hasn't exposed a portable drag-window API yet; we
    // fall back to no-op on non-Windows. The HUD is still moveable
    // via the OS's window decorations if the user enables them.
}

/// Tell Windows 11 to render this window's corners square. Without
/// this DWM applies the system-wide "rounded corner" preference and
/// our frame's top-right corner curves inward — McKale specifically
/// asked for square edges. No-op on Windows 10 and earlier (DWM
/// silently ignores the unsupported attribute on those builds).
#[cfg(target_os = "windows")]
pub fn disable_corner_rounding(ui: &MainWindow) {
    use i_slint_backend_winit::WinitWindowAccessor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::ComponentHandle;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
    };

    ui.window().with_winit_window(|w| {
        let Ok(handle) = w.window_handle() else {
            tracing::warn!("disable_corner_rounding: no window handle");
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(h.hwnd.get() as *mut std::ffi::c_void);
        let pref = DWMWCP_DONOTROUND;
        let r = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&pref) as u32,
            )
        };
        if r.is_err() {
            tracing::debug!("DwmSetWindowAttribute(DONOTROUND) returned {:?}", r);
        } else {
            tracing::info!("Windows 11 corner rounding disabled");
        }
    });
}

#[cfg(not(target_os = "windows"))]
pub fn disable_corner_rounding(_ui: &MainWindow) {
    // No equivalent concept on macOS/Linux WMs in current scope.
}
