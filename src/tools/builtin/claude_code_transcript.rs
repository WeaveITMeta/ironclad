//! Read Claude Code's own session transcripts so JARVIS can see what
//! Claude Code is doing in this VS Code window — every user message,
//! assistant reply, tool call, and tool result lands in a JSONL file at
//! `%USERPROFILE%\.claude\projects\<encoded-cwd>\<session-uuid>.jsonl`.
//!
//! This is the cleanest path for the autonomous loop to know "did the
//! last cargo build fail?" — Claude Code's Bash tool surfaces stderr in
//! its tool_result, which lands as a structured entry here. No OCR, no
//! screenshot, just grep the JSONL.
//!
//! The tool picks the MOST RECENTLY MODIFIED `.jsonl` in the project
//! folder (the active session), reads the last N entries, and optionally
//! filters to only errors or to a substring. Returns a compact JSON
//! summary so the LLM doesn't drown in 50KB of replayed conversation.

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

pub struct ClaudeCodeTranscriptTailTool;

#[async_trait]
impl Tool for ClaudeCodeTranscriptTailTool {
    fn name(&self) -> &str {
        "claude_code_transcript_tail"
    }

    fn description(&self) -> &str {
        "Read the tail of the active Claude Code session transcript for \
         THIS workspace. Returns the last N entries (default 20, max 200) \
         as compact JSON: role, summary text, tool name + status if it's \
         a tool result. Filter to errors only with `errors_only: true`, \
         or substring-match the content with `contains`. Use this to see \
         what Claude Code just did and whether anything failed — it's the \
         autonomous loop's window into this VS Code session. Read-only."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Max number of entries to return (default 20)."
                },
                "errors_only": {
                    "type": "boolean",
                    "description": "If true, only return entries where a tool result was an error or contains common failure markers."
                },
                "contains": {
                    "type": "string",
                    "description": "Case-insensitive substring filter on entry content."
                },
                "project_path": {
                    "type": "string",
                    "description": "Override the workspace path used to locate the transcripts dir. Defaults to the current working directory."
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

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .min(200) as usize;
        let errors_only = params
            .get("errors_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let contains_filter = params
            .get("contains")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        let project_path_override = params
            .get("project_path")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Locate the project transcripts directory.
        let project_dir = match resolve_project_dir(project_path_override.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolOutput::success(
                    serde_json::json!({ "status": "error", "reason": e }),
                    start.elapsed(),
                ));
            }
        };

        if !project_dir.exists() {
            return Ok(ToolOutput::success(
                serde_json::json!({
                    "status": "no_transcripts",
                    "reason": format!("no Claude Code project dir at {}", project_dir.display()),
                }),
                start.elapsed(),
            ));
        }

        // Pick the most recently modified .jsonl in the project dir.
        let active = match latest_transcript(&project_dir) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(ToolOutput::success(
                    serde_json::json!({
                        "status": "no_transcripts",
                        "project_dir": project_dir.display().to_string(),
                    }),
                    start.elapsed(),
                ));
            }
            Err(e) => return Err(ToolError::ExecutionFailed(e)),
        };

        // Read the file. JSONL files can be megabytes; we read fully and
        // then take the tail. Tradeoff: simpler than seeking backwards,
        // and a few MB is fine for a 5-minute-tick autonomous loop.
        let content = tokio::fs::read_to_string(&active)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("read transcript: {e}")))?;

        let mut entries: Vec<serde_json::Value> = Vec::new();
        for line in content.lines().rev() {
            if entries.len() >= limit {
                break;
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(raw) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(summary) = summarize_entry(&raw) else {
                continue;
            };
            if errors_only && !summary.is_error {
                continue;
            }
            if let Some(needle) = &contains_filter {
                let hay = summary.text.to_ascii_lowercase();
                if !hay.contains(needle) {
                    continue;
                }
            }
            entries.push(serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null));
        }
        // Restore chronological order (we iterated reverse to take tail).
        entries.reverse();

        Ok(ToolOutput::success(
            serde_json::json!({
                "status": "ok",
                "transcript": active.display().to_string(),
                "count": entries.len(),
                "entries": entries,
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        // Reading our own Claude Code session transcripts — not external.
        false
    }
}

#[derive(serde::Serialize)]
struct EntrySummary {
    role: String,
    kind: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    is_error: bool,
}

/// Pull a compact summary out of one Claude Code JSONL entry. Returns
/// None for entries we don't want to surface (system reminders, empty
/// frames, internal metadata).
fn summarize_entry(raw: &serde_json::Value) -> Option<EntrySummary> {
    let entry_type = raw
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Claude Code records "user" and "assistant" entries with a nested
    // `message` object that follows the Anthropic Messages API shape.
    let message = raw.get("message");
    let role = message
        .and_then(|m| m.get("role"))
        .and_then(|v| v.as_str())
        .unwrap_or(entry_type)
        .to_string();

    let content = message.and_then(|m| m.get("content"));

    // Common case: content is a string.
    if let Some(text) = content.and_then(|v| v.as_str()) {
        if text.trim().is_empty() {
            return None;
        }
        return Some(EntrySummary {
            role,
            kind: "text".to_string(),
            text: truncate(text, 1200),
            tool_name: None,
            is_error: false,
        });
    }

    // Content is an array of blocks (Anthropic-style).
    if let Some(blocks) = content.and_then(|v| v.as_array()) {
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_name: Option<String> = None;
        let mut is_error = false;
        let mut kind = "text".to_string();

        for block in blocks {
            let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match bt {
                "text" => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text_parts.push(t.to_string());
                    }
                }
                "tool_use" => {
                    kind = "tool_call".to_string();
                    tool_name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let input = block.get("input").map(|v| v.to_string()).unwrap_or_default();
                    text_parts.push(format!(
                        "{}({})",
                        tool_name.as_deref().unwrap_or("?"),
                        truncate(&input, 240)
                    ));
                }
                "tool_result" => {
                    kind = "tool_result".to_string();
                    if block.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                        is_error = true;
                    }
                    let body = match block.get("content") {
                        Some(serde_json::Value::String(s)) => s.clone(),
                        Some(serde_json::Value::Array(arr)) => arr
                            .iter()
                            .filter_map(|b| {
                                b.get("text").and_then(|v| v.as_str()).map(String::from)
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    if looks_like_error(&body) {
                        is_error = true;
                    }
                    text_parts.push(body);
                }
                _ => {}
            }
        }

        let joined = text_parts.join(" | ");
        if joined.trim().is_empty() {
            return None;
        }
        return Some(EntrySummary {
            role,
            kind,
            text: truncate(&joined, 1200),
            tool_name,
            is_error,
        });
    }

    None
}

fn looks_like_error(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    // Common Claude Code / cargo / shell failure markers. Kept narrow on
    // purpose — false positives ("warning: unused") would make
    // errors_only useless.
    lower.contains("error[e")        // rustc
        || lower.contains("error: ") // generic CLI
        || lower.contains("panicked at")
        || lower.contains("exit code: ")
        || lower.contains("failed to compile")
        || lower.contains("traceback (most recent call last)")
        || lower.contains("uncaught exception")
        || lower.contains("fatal:")
}

fn truncate(s: &str, max: usize) -> String {
    // Operate on chars so we don't slice mid-codepoint.
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let head: String = chars.iter().take(max).collect();
    format!("{}... ({} chars total)", head, chars.len())
}

/// Where Claude Code stores transcripts. Each project gets its own
/// folder named by the *url-encoded current working directory* with `\`
/// and `/` and `:` replaced by `-`. The session UUIDs are file names
/// inside that folder.
fn resolve_project_dir(override_path: Option<&str>) -> Result<PathBuf, String> {
    let project_root = match override_path {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().map_err(|e| format!("cwd: {e}"))?,
    };
    let home = dirs::home_dir().ok_or_else(|| "no home dir".to_string())?;
    let encoded = encode_project_path(&project_root);
    Ok(home.join(".claude").join("projects").join(encoded))
}

/// Claude Code's path-encoding scheme: replace `\`, `/`, and `:` with
/// `-`. e.g. `c:\Users\miksu\Documents\Olson` →
/// `c--Users-miksu-Documents-Olson`. Matches the directory listing
/// the user has been operating in.
fn encode_project_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    s.replace(['\\', '/', ':'], "-")
}

fn latest_transcript(dir: &std::path::Path) -> Result<Option<PathBuf>, String> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match &best {
            Some((cur, _)) if *cur >= modified => {}
            _ => best = Some((modified, path)),
        }
    }
    Ok(best.map(|(_, p)| p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_windows_path() {
        let p = std::path::Path::new("c:\\Users\\miksu\\Documents\\Olson");
        assert_eq!(encode_project_path(p), "c--Users-miksu-Documents-Olson");
    }

    #[test]
    fn looks_like_error_catches_rustc() {
        assert!(looks_like_error("error[E0432]: unresolved import"));
        assert!(looks_like_error("thread 'main' panicked at src/main.rs"));
        assert!(looks_like_error("exit code: 1"));
        assert!(!looks_like_error("warning: unused variable"));
    }
}
