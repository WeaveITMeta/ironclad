//! `recent_logs` tool — surfaces Iron Clad's own ring-buffered tracing
//! output to JARVIS so he can self-diagnose. The LogBroadcaster keeps the
//! last 500 log entries in memory (everything that hit the tracing layer
//! at INFO/WARN/ERROR/DEBUG, depending on the filter). This tool exposes
//! that buffer as a callable query.
//!
//! Use case: McKale says "JARVIS, summarize what's gone wrong in the
//! last hour." JARVIS calls `recent_logs(level: "warn", limit: 100)`,
//! reads them, composes the summary. Or "draft a wishlist update from
//! the recent errors" — JARVIS pulls the warns + errors and writes them
//! into WISHLIST.md.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::channels::web::log_layer::LogBroadcaster;
use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

pub struct RecentLogsTool {
    broadcaster: Arc<LogBroadcaster>,
}

impl RecentLogsTool {
    pub fn new(broadcaster: Arc<LogBroadcaster>) -> Self {
        Self { broadcaster }
    }
}

#[async_trait]
impl Tool for RecentLogsTool {
    fn name(&self) -> &str {
        "recent_logs"
    }

    fn description(&self) -> &str {
        "Return Iron Clad's own recent tracing log entries (the same lines \
         that scroll in the gateway terminal). Filter by minimum `level` \
         (debug | info | warn | error) and cap with `limit` (default 50, max \
         500). Use this to self-diagnose what went wrong, summarize boot \
         state, or draft wishlist updates from recent warnings. Read-only."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "level": {
                    "type": "string",
                    "enum": ["debug", "info", "warn", "error"],
                    "description": "Minimum severity to include (e.g. 'warn' returns only WARN and ERROR)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 500,
                    "description": "Max number of entries (default 50)"
                },
                "contains": {
                    "type": "string",
                    "description": "Case-insensitive substring filter on message body"
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
        let level_floor = params
            .get("level")
            .and_then(|v| v.as_str())
            .map(|s| level_rank(s))
            .unwrap_or(0); // default: everything
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .min(500) as usize;
        let needle = params
            .get("contains")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());

        let all = self.broadcaster.recent_entries();
        let filtered: Vec<serde_json::Value> = all
            .into_iter()
            .rev() // newest-first
            .filter(|e| level_rank(&e.level) >= level_floor)
            .filter(|e| {
                needle
                    .as_ref()
                    .map(|n| e.message.to_ascii_lowercase().contains(n))
                    .unwrap_or(true)
            })
            .take(limit)
            .map(|e| {
                serde_json::json!({
                    "level": e.level,
                    "target": e.target,
                    "message": e.message,
                    "timestamp": e.timestamp,
                })
            })
            .collect();

        Ok(ToolOutput::success(
            serde_json::json!({
                "count": filtered.len(),
                "entries": filtered,
            }),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

fn level_rank(s: &str) -> u8 {
    match s.to_ascii_lowercase().as_str() {
        "error" => 4,
        "warn" => 3,
        "info" => 2,
        "debug" => 1,
        "trace" => 0,
        _ => 0,
    }
}
