//! Persistence layer for the native Settings panel.
//!
//! `write_settings_to_env` does an atomic in-place merge of the settings
//! the SettingsPanel exposes (Anthropic key/model, ElevenLabs key/voice,
//! heartbeat + autonomous toggles) into the ironclad root's `.env`
//! file. Existing keys are updated in place; new keys append at the
//! bottom under a marker comment. Atomic = write to `.env.tmp`, fsync,
//! rename — readers always see a complete file or the prior file,
//! never a partially-written file.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WriteSettings {
    pub anthropic_key: String,
    pub model: String,
    pub elevenlabs_key: String,
    pub elevenlabs_voice_id: String,
    pub heartbeat: bool,
    pub autonomous: bool,
}

/// Write the settings into `.ironclad/.env`. Reads existing file, merges
/// updates in by key, writes back atomically via tmp + rename. Returns
/// the path that was written so the UI can show "saved to X".
pub fn write_settings_to_env(s: WriteSettings) -> anyhow::Result<PathBuf> {
    let root = ironclad_root_for_settings()?;
    let env_path = root.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();

    let mut updates: BTreeMap<&str, String> = BTreeMap::new();
    updates.insert("ANTHROPIC_API_KEY", s.anthropic_key);
    updates.insert("ANTHROPIC_MODEL", s.model);
    updates.insert("ELEVENLABS_API_KEY", s.elevenlabs_key);
    updates.insert("ELEVENLABS_VOICE_ID", s.elevenlabs_voice_id);
    updates.insert(
        "HEARTBEAT_ENABLED",
        if s.heartbeat { "true" } else { "false" }.to_string(),
    );
    updates.insert(
        "AUTONOMOUS_LOOP_ENABLED",
        if s.autonomous { "true" } else { "false" }.to_string(),
    );

    let mut seen: HashSet<&str> = HashSet::new();
    let mut out_lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            out_lines.push(line.to_string());
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let key_trim = key.trim();
            if let Some(val) = updates.get(key_trim) {
                out_lines.push(format!("{}={}", key_trim, val));
                seen.insert(key_trim);
                continue;
            }
        }
        out_lines.push(line.to_string());
    }
    let mut appended = false;
    for (k, v) in &updates {
        if !seen.contains(k) {
            if !appended {
                out_lines.push(String::new());
                out_lines.push("# Added by jarvis-desktop SettingsPanel".to_string());
                appended = true;
            }
            out_lines.push(format!("{}={}", k, v));
        }
    }
    out_lines.push(String::new());
    let content = out_lines.join("\n");
    let tmp = env_path.with_extension("env.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &env_path)?;
    Ok(env_path)
}

/// Walk up from `current_exe()` until we find an ironclad root (a
/// directory with a `dashboard/` sibling AND either a `.env` file or a
/// `Cargo.toml`). Falls back to cwd if no marker is found.
pub fn ironclad_root_for_settings() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut p = exe.as_path();
    while let Some(parent) = p.parent() {
        if parent.join("dashboard").is_dir() && parent.join(".env").exists() {
            return Ok(parent.to_path_buf());
        }
        if parent.join("Cargo.toml").is_file() && parent.join("dashboard").is_dir() {
            return Ok(parent.to_path_buf());
        }
        p = parent;
    }
    std::env::current_dir().map_err(|e| anyhow::anyhow!("cwd: {e}"))
}

