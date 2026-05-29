//! Native OS notifications.
//!
//! On Windows: real Action Center toast via the WinRT Notifications API
//! (through `tauri-winrt-notification`). Notifications survive in the
//! action center until the user dismisses them, which is the right
//! behavior for "JARVIS responded while you were in another app" —
//! McKale can click later to summon the HUD.
//!
//! On other platforms: log only (until libnotify / NSUserNotification
//! support lands).

/// Show a native OS notification with a title + body. Fire-and-forget —
/// failures are logged but never propagate, since a missed toast must
/// not block the chat flow.
pub fn show_native_notification(title: &str, body: &str) {
    tracing::info!("[notify] {title}: {body}");
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = show_windows_toast(title, body) {
            tracing::debug!("[notify] toast skipped: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn show_windows_toast(title: &str, body: &str) -> Result<(), String> {
    use tauri_winrt_notification::{Sound, Toast};
    // Default AUMID = PowerShell shell. Works without app registration
    // and produces an Action Center-visible toast; the "PowerShell"
    // attribution is the documented trade-off for skipping AUMID
    // registration. Registering a proper AUMID would require an
    // installer + Start Menu shortcut, which is overkill for a single-
    // user desktop overlay.
    Toast::new(Toast::POWERSHELL_APP_ID)
        .title(title)
        .text1(body)
        .sound(Some(Sound::Default))
        .show()
        .map_err(|e| format!("toast show failed: {e}"))
}
