//! Persistent feedback loop for JARVIS — the missing piece between
//! "JARVIS ran a command" and "JARVIS learned from running that command."
//!
//! Each entry is appended as a JSON line to a daily file at
//! `feedback/<YYYY-MM-DD>.md` in the workspace. The file is markdown
//! with a fenced ```jsonl``` block so it shows up readably in Obsidian
//! while still being machine-parseable.
//!
//! Three writers feed this log:
//!   1. The agent loop auto-captures every tool failure (the model
//!      can't forget to record losses — they're recorded for it).
//!   2. The model uses `feedback_log_write` to record explicit
//!      reflections at end of turn ("what worked, what didn't").
//!   3. The autonomous loop writes a one-line outcome per tick.
//!
//! Two readers consume it:
//!   - `feedback_log_read` — the model queries past lessons before
//!     deciding the next action.
//!   - The autonomous loop reads recent entries as part of its
//!     situation report each tick.
//!
//! This is the foundation of self-improvement: the same mistake
//! shouldn't ship twice if it was logged once.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};
use crate::workspace::Workspace;

// =============================================================================
// feedback_log_write
// =============================================================================

pub struct FeedbackLogWriteTool {
    workspace: Arc<Workspace>,
}

impl FeedbackLogWriteTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FeedbackLogWriteTool {
    fn name(&self) -> &str {
        "feedback_log_write"
    }

    fn description(&self) -> &str {
        "Record a structured lesson learned from this turn or tick so \
         FUTURE turns can read it. Call this at the end of any \
         non-trivial action sequence, especially after a failure or a \
         non-obvious win. Required fields: `kind` (one of: \
         'tool_failure', 'tool_success', 'reflection', 'outcome'), \
         `summary` (one sentence — what happened), and `lesson` (one \
         sentence — what to do or avoid next time). Optional: `tool`, \
         `tags`. The log is persistent and queryable via \
         `feedback_log_read`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["tool_failure", "tool_success", "reflection", "outcome"],
                    "description": "What sort of entry this is."
                },
                "summary": {
                    "type": "string",
                    "description": "One sentence: what happened."
                },
                "lesson": {
                    "type": "string",
                    "description": "One sentence: what to do or avoid next time. THIS is the part future ticks read."
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name involved, if any."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tags for retrieval (e.g. 'playwright', 'github', 'eustress')."
                }
            },
            "required": ["kind", "summary", "lesson"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let kind = params
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'kind'".into()))?;
        let summary = params
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'summary'".into()))?;
        let lesson = params
            .get("lesson")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'lesson'".into()))?;
        let tool_name = params.get("tool").and_then(|v| v.as_str());
        let tags: Vec<String> = params
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        append_entry(&self.workspace, kind, summary, lesson, tool_name, &tags).await?;

        Ok(ToolOutput::text(
            format!("logged: [{}] {}", kind, lesson),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// =============================================================================
// feedback_log_read
// =============================================================================

pub struct FeedbackLogReadTool {
    workspace: Arc<Workspace>,
}

impl FeedbackLogReadTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for FeedbackLogReadTool {
    fn name(&self) -> &str {
        "feedback_log_read"
    }

    fn description(&self) -> &str {
        "Read recent feedback log entries — past lessons, failures, and \
         outcomes JARVIS has captured. Use this BEFORE deciding the next \
         action when you've done similar work before. Filters: `kind` to \
         narrow by entry type, `tool` to filter by which tool was used, \
         `contains` to substring-match summary or lesson, `limit` (max \
         100, default 30). Returns most-recent-first."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["tool_failure", "tool_success", "reflection", "outcome"]
                },
                "tool": { "type": "string" },
                "contains": { "type": "string" },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Default 30."
                },
                "days_back": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 30,
                    "description": "How many days of log files to scan. Default 7."
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
        let kind_filter = params.get("kind").and_then(|v| v.as_str()).map(String::from);
        let tool_filter = params.get("tool").and_then(|v| v.as_str()).map(String::from);
        let contains_filter = params
            .get("contains")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(30)
            .min(100) as usize;
        let days_back = params
            .get("days_back")
            .and_then(|v| v.as_u64())
            .unwrap_or(7)
            .min(30) as i64;

        let entries = read_recent(
            &self.workspace,
            days_back,
            kind_filter.as_deref(),
            tool_filter.as_deref(),
            contains_filter.as_deref(),
            limit,
        )
        .await?;

        Ok(ToolOutput::success(
            serde_json::json!({
                "count": entries.len(),
                "entries": entries,
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// =============================================================================
// shared helpers — used by tools AND by the agent loop's auto-capture path
// =============================================================================

/// Append one entry to today's feedback log. Public so the agent loop
/// can record tool failures automatically without going through a tool
/// dispatch (it already has the workspace reference).
pub async fn append_entry(
    workspace: &Workspace,
    kind: &str,
    summary: &str,
    lesson: &str,
    tool: Option<&str>,
    tags: &[String],
) -> Result<(), ToolError> {
    let entry = serde_json::json!({
        "ts": Utc::now().to_rfc3339(),
        "kind": kind,
        "tool": tool,
        "summary": summary,
        "lesson": lesson,
        "tags": tags,
    });
    let line = format!("{}\n", entry);
    let path = today_path();
    // The append helper auto-creates parent files. We append raw JSONL
    // (no fence) so reads are straightforward; Obsidian will render it
    // as code-like text either way.
    workspace
        .append(&path, &line)
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("append feedback: {e}")))?;
    Ok(())
}

/// Convenience wrapper for non-tool callers (autonomous_loop emits its
/// own outcomes) that swallows errors with a warning rather than
/// surfacing them — feedback logging should NEVER block real work.
pub async fn record_silently(
    workspace: &Workspace,
    kind: &str,
    summary: &str,
    lesson: &str,
    tool: Option<&str>,
) {
    if let Err(e) = append_entry(workspace, kind, summary, lesson, tool, &[]).await {
        tracing::warn!("feedback log write failed (non-fatal): {}", e);
    }
}

fn today_path() -> String {
    format!("feedback/{}.md", Utc::now().format("%Y-%m-%d"))
}

/// Read up to `limit` matching entries from the last `days_back` days,
/// newest first.
async fn read_recent(
    workspace: &Workspace,
    days_back: i64,
    kind_filter: Option<&str>,
    tool_filter: Option<&str>,
    contains_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<serde_json::Value>, ToolError> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let today = Utc::now().date_naive();
    for d in 0..days_back {
        let date = today - chrono::Duration::days(d);
        let path = format!("feedback/{}.md", date.format("%Y-%m-%d"));
        let doc = match workspace.read(&path).await {
            Ok(d) => d,
            Err(_) => continue, // no file that day, fine
        };
        // Parse JSONL — every non-empty line is one entry.
        let mut day_entries: Vec<serde_json::Value> = Vec::new();
        for line in doc.content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("```") || line.starts_with('#') {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(k) = kind_filter {
                if entry.get("kind").and_then(|v| v.as_str()) != Some(k) {
                    continue;
                }
            }
            if let Some(t) = tool_filter {
                if entry.get("tool").and_then(|v| v.as_str()) != Some(t) {
                    continue;
                }
            }
            if let Some(needle) = contains_filter {
                let summary = entry.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                let lesson = entry.get("lesson").and_then(|v| v.as_str()).unwrap_or("");
                let hay = format!("{} {}", summary, lesson).to_ascii_lowercase();
                if !hay.contains(needle) {
                    continue;
                }
            }
            day_entries.push(entry);
        }
        // Day file is chronological; reverse so newest-first within day.
        day_entries.reverse();
        out.extend(day_entries);
        if out.len() >= limit {
            out.truncate(limit);
            break;
        }
    }
    Ok(out)
}
