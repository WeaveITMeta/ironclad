//! Process-wide crash isolation. Two surfaces:
//!
//!   1. **`install_panic_hook`** — sets a global panic hook that
//!      writes a structured crash report to
//!      `~/.ironclad/crashes/<unix-ts>.json` BEFORE letting the default
//!      hook run. So even when a panic does take the process down (UI
//!      thread, main, etc.) we have a forensic trail. Logs to stderr
//!      as well via `tracing`.
//!
//!   2. **`safe_callback`** — `catch_unwind` wrapper used inside cpal
//!      input/output callbacks. cpal's callback closures run on
//!      platform audio threads (WASAPI on Windows, CoreAudio on mac,
//!      ALSA/Pulse on Linux); a panic unwinding through C FFI is
//!      undefined behavior — almost always abort. Wrapping the body
//!      in catch_unwind catches the panic on the Rust side, logs it,
//!      and keeps the audio thread alive (filling the affected frame
//!      with silence). This is what Chrome's audio service does
//!      internally to survive a misbehaving plugin without dying.
//!
//! Together these mean a panic inside (say) the VAD state machine no
//! longer kills the whole binary; the audio stream eats one frame of
//! silence, the next frame works fine, and the crash report shows up
//! in `~/.ironclad/crashes/`.

use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::SystemTime;

/// Where crash reports go. Default: `<dirs::data_dir>/ironclad/crashes`.
/// Can be overridden with `JARVIS_CRASH_DIR=...` for testing.
fn crash_dir() -> PathBuf {
    if let Ok(p) = std::env::var("JARVIS_CRASH_DIR") {
        return PathBuf::from(p);
    }
    // Fall back to the .ironclad directory relative to CWD if no env
    // var is set and dirs aren't available. Last resort: temp dir.
    if let Some(home) = dirs::home_dir() {
        return home.join(".ironclad").join("crashes");
    }
    std::env::temp_dir().join("ironclad-crashes")
}

static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

/// Install the global panic hook. Idempotent — calling it twice in a
/// row is a no-op.
pub fn install_panic_hook() {
    if PANIC_HOOK_INSTALLED.set(()).is_err() {
        return;
    }

    let dir = crash_dir();
    // Best-effort: create the crash dir up-front so the panic hook
    // doesn't have to. If it fails (read-only fs, weird perms), the
    // hook will retry.
    let _ = std::fs::create_dir_all(&dir);

    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Compose a structured report. We DO NOT use serde_json here —
        // the panic hook should depend on as little as possible since
        // it runs in arbitrary thread state. Hand-format the JSON.
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let payload = info.payload();
        let message: &str = if let Some(s) = payload.downcast_ref::<&'static str>() {
            s
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.as_str()
        } else {
            "<non-string panic payload>"
        };
        let thread_name = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();

        // Log first — even if disk is full, stderr still works.
        tracing::error!(
            "PANIC in thread '{}' at {}: {}",
            thread_name,
            location,
            message
        );

        // Write JSON report. Hand-escape the message + backtrace
        // because we can't safely call into serde_json here.
        let json = format!(
            "{{\n  \"ts\": {},\n  \"thread\": \"{}\",\n  \"location\": \"{}\",\n  \"message\": \"{}\",\n  \"backtrace\": \"{}\"\n}}\n",
            ts,
            escape_json(&thread_name),
            escape_json(&location),
            escape_json(message),
            escape_json(&backtrace),
        );
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("crash-{}.json", ts));
        if let Err(e) = std::fs::write(&path, json) {
            tracing::error!("crash report write failed for {}: {}", path.display(), e);
        } else {
            tracing::error!("crash report written: {}", path.display());
        }

        // Defer to whatever hook was there before us so the default
        // unwinding/backtrace behavior still runs.
        previous(info);
    }));
}

/// Run `f` and catch any panic. Returns true on success, false if a
/// panic was caught. Used inside cpal callbacks so the audio thread
/// survives a panic in user code (VAD, splitter, downmix, etc.).
///
/// `f` is wrapped in `AssertUnwindSafe` internally because cpal
/// callbacks legitimately need shared state (parking_lot Mutexes,
/// RefCells, Arc<PlaybackQueue>) and Rust's auto-trait UnwindSafe
/// excludes anything with interior mutability. We're asserting:
/// "yes, this state can be observed after the panic; that's safe
/// because each frame is independent — a panicked frame just drops
/// silently and the next one runs clean against possibly-mutated
/// state, which is exactly what happens during normal operation
/// when one frame writes and the next reads."
///
/// Logs the panic via tracing before swallowing it. The crash report
/// from `install_panic_hook` also fires because the hook runs BEFORE
/// catch_unwind catches the unwind.
pub fn safe_callback<F>(label: &str, f: F) -> bool
where
    F: FnOnce(),
{
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(()) => true,
        Err(_) => {
            tracing::error!(
                "audio callback '{}' panicked — frame dropped, stream continues",
                label
            );
            false
        }
    }
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_callback_swallows_panic() {
        let ok = safe_callback("test", || panic!("intentional"));
        assert!(!ok, "safe_callback should return false on panic");
        // And on a clean call:
        let ok = safe_callback("test", || {});
        assert!(ok);
    }

    #[test]
    fn escape_handles_control_chars() {
        assert_eq!(escape_json("hi\nthere"), "hi\\nthere");
        assert_eq!(escape_json("a\"b"), "a\\\"b");
        assert_eq!(escape_json("c\\d"), "c\\\\d");
    }
}
