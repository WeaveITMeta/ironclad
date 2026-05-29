//! Global hotkey parsing.
//!
//! `parse_key_env` reads a key name from an environment variable and
//! resolves it to the matching `global_hotkey::hotkey::Code`. Used so
//! McKale can override the default hotkey bindings without recompile —
//! e.g. `JARVIS_HOTKEY_EXPAND=F9` to avoid a Ctrl+Alt+J conflict from
//! some other app.
//!
//! Bare single letters are auto-prefixed with "Key" so `J` and `KeyJ`
//! both work. Unknown names log a warning and fall back to the default.

use global_hotkey::hotkey::Code;

/// Parse a key code from an env var, falling back to `default` if the
/// var is unset or names a key we don't recognize. Accepts the canonical
/// names global-hotkey uses (e.g. "KeyJ", "F9", "Space", "Semicolon");
/// also accepts a bare single letter ("J") and prefixes "Key".
pub fn parse_key_env(var: &str, default: Code) -> Code {
    let Ok(raw) = std::env::var(var) else {
        return default;
    };
    let candidate = raw.trim();
    let candidate_upper = candidate.to_uppercase();
    let normalized = if candidate.len() == 1
        && candidate
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false)
    {
        format!("Key{}", candidate_upper)
    } else {
        candidate.to_string()
    };
    match normalized.as_str() {
        "KeyA" => Code::KeyA, "KeyB" => Code::KeyB, "KeyC" => Code::KeyC,
        "KeyD" => Code::KeyD, "KeyE" => Code::KeyE, "KeyF" => Code::KeyF,
        "KeyG" => Code::KeyG, "KeyH" => Code::KeyH, "KeyI" => Code::KeyI,
        "KeyJ" => Code::KeyJ, "KeyK" => Code::KeyK, "KeyL" => Code::KeyL,
        "KeyM" => Code::KeyM, "KeyN" => Code::KeyN, "KeyO" => Code::KeyO,
        "KeyP" => Code::KeyP, "KeyQ" => Code::KeyQ, "KeyR" => Code::KeyR,
        "KeyS" => Code::KeyS, "KeyT" => Code::KeyT, "KeyU" => Code::KeyU,
        "KeyV" => Code::KeyV, "KeyW" => Code::KeyW, "KeyX" => Code::KeyX,
        "KeyY" => Code::KeyY, "KeyZ" => Code::KeyZ,
        "F1" => Code::F1, "F2" => Code::F2, "F3" => Code::F3, "F4" => Code::F4,
        "F5" => Code::F5, "F6" => Code::F6, "F7" => Code::F7, "F8" => Code::F8,
        "F9" => Code::F9, "F10" => Code::F10, "F11" => Code::F11, "F12" => Code::F12,
        "Semicolon" => Code::Semicolon,
        "Quote" => Code::Quote,
        "Comma" => Code::Comma,
        "Period" => Code::Period,
        "Slash" => Code::Slash,
        "Backslash" => Code::Backslash,
        "Backquote" => Code::Backquote,
        "Space" => Code::Space,
        other => {
            tracing::warn!(
                "unknown key code '{other}' for env '{var}' — falling back to default"
            );
            default
        }
    }
}
