//! Autonomous loop — JARVIS wakes up on a timer, gathers a situation
//! report, decides one action, executes it, and either reports back to
//! the transcript or stays silent. No user prompt required.
//!
//! Architecture:
//!   - Tick every `interval` (default 5 min).
//!   - On each tick, read `AUTONOMOUS.md` (priorities) + assemble a
//!     situation report from observe-only tools.
//!   - Run a tool-calling LLM turn (Sonnet by default) bounded by
//!     `max_iterations_per_tick` and an allow/deny list.
//!   - Parse the final assistant text for a sentinel:
//!       `LOOP_OK`              → no transcript, no voice
//!       `LOOP_SILENT: <...>`   → transcript only, no voice
//!       `LOOP_VOICE: <...>`    → transcript + voice (ElevenLabs picks up
//!                                 the broadcast like any chat reply)
//!     Anything else falls back to `LOOP_SILENT` semantics so a confused
//!     model can't suddenly start narrating itself every 5 minutes.
//!
//! Safety:
//!   - The allow/deny list is checked BEFORE the model sees the tool. A
//!     model that hallucinates a `vault_delete` call gets a tool_result
//!     error, not an executed delete.
//!   - `requires_approval()` is bypassed inside the loop because there's
//!     no user to ask. Risky tools must be kept off the allow list — not
//!     gated by interactive approval.
//!   - Per-tick consecutive failures stop the loop after `max_failures`
//!     to keep a misfire from burning budget overnight.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::channels::OutgoingResponse;
use crate::context::JobContext;
use crate::llm::{
    ChatMessage, LlmProvider, Role, ToolCall, ToolCompletionRequest, ToolDefinition,
};
use crate::tools::ToolRegistry;
use crate::workspace::Workspace;

/// Embedded fallback in case AUTONOMOUS.md hasn't been seeded into the
/// workspace DB yet (first boot on a fresh install).
const AUTONOMOUS_SEED: &str = include_str!("../../workspace_seed/AUTONOMOUS.md");

/// Configuration for the autonomous loop.
#[derive(Debug, Clone)]
pub struct AutonomousLoopConfig {
    pub interval: Duration,
    pub enabled: bool,
    pub max_iterations_per_tick: u32,
    pub max_failures: u32,
    pub notify_user_id: String,
    pub notify_channel: String,
    /// If `Some`, only these tools are exposed to the model. If `None`,
    /// every registered tool is available except those in `denied`.
    pub allowed: Option<HashSet<String>>,
    pub denied: HashSet<String>,
}

impl Default for AutonomousLoopConfig {
    fn default() -> Self {
        let mut denied = HashSet::new();
        // Hard denies — never on, even if accidentally added to allow.
        denied.insert("vault_delete".to_string());
        denied.insert("windows_new_desktop".to_string());

        Self {
            interval: Duration::from_secs(5 * 60),
            enabled: false, // opt-in via main.rs env var
            max_iterations_per_tick: 6,
            max_failures: 3,
            notify_user_id: "default".to_string(),
            notify_channel: "gateway".to_string(),
            allowed: None,
            denied,
        }
    }
}

impl AutonomousLoopConfig {
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }
    pub fn with_notify(mut self, user_id: impl Into<String>, channel: impl Into<String>) -> Self {
        self.notify_user_id = user_id.into();
        self.notify_channel = channel.into();
        self
    }
}

pub struct AutonomousLoopRunner {
    config: AutonomousLoopConfig,
    workspace: Arc<Workspace>,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    response_tx: Option<mpsc::Sender<OutgoingResponse>>,
    consecutive_failures: u32,
}

impl AutonomousLoopRunner {
    pub fn new(
        config: AutonomousLoopConfig,
        workspace: Arc<Workspace>,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            config,
            workspace,
            llm,
            tools,
            response_tx: None,
            consecutive_failures: 0,
        }
    }

    pub fn with_response_channel(mut self, tx: mpsc::Sender<OutgoingResponse>) -> Self {
        self.response_tx = Some(tx);
        self
    }

    pub async fn run(&mut self) {
        if !self.config.enabled {
            tracing::info!("autonomous loop disabled, not starting");
            return;
        }
        tracing::info!(
            "🤖 autonomous loop starting, interval={:?}, model={}",
            self.config.interval,
            self.llm.model_name()
        );

        let mut interval = tokio::time::interval(self.config.interval);
        interval.tick().await; // skip immediate first tick

        loop {
            interval.tick().await;
            match self.run_tick().await {
                Ok(_) => {
                    self.consecutive_failures = 0;
                }
                Err(e) => {
                    self.consecutive_failures += 1;
                    tracing::error!(
                        "autonomous loop tick failed ({}/{}): {}",
                        self.consecutive_failures,
                        self.config.max_failures,
                        e
                    );
                    if self.consecutive_failures >= self.config.max_failures {
                        tracing::error!(
                            "autonomous loop disabled after {} consecutive failures",
                            self.consecutive_failures
                        );
                        break;
                    }
                }
            }
        }
    }

    async fn run_tick(&self) -> Result<(), String> {
        let started = Instant::now();
        tracing::info!("🤖 autonomous tick start");

        // 1. Load priorities. Prefer the live workspace copy so user
        //    edits via memory_write take effect without a rebuild.
        let priorities = match self.workspace.read("AUTONOMOUS.md").await {
            Ok(doc) => doc.content,
            Err(_) => AUTONOMOUS_SEED.to_string(),
        };

        // 1b. Pull recent feedback so prior lessons inform this tick.
        //     This is the difference between "JARVIS runs the same
        //     failing tool every 5 minutes" and "JARVIS stops trying
        //     things it already learned don't work."
        let prior_lessons = load_recent_lessons(&self.workspace).await;

        // 2. Pull system prompt (identity + soul + agents).
        let system_prompt = self
            .workspace
            .system_prompt()
            .await
            .unwrap_or_default();

        // 3. Build tool palette filtered by allow/deny.
        let palette = self.build_tool_palette().await;
        if palette.is_empty() {
            return Err("no tools available after allow/deny filtering".to_string());
        }
        tracing::debug!("autonomous palette: {} tools", palette.len());

        // 4. Compose the user prompt — the tick instruction itself.
        let user_prompt = format!(
            "## AUTONOMOUS_TICK\n\n\
             It is now your turn to act without user input. Read the priorities \
             below, decide ONE action, execute it via tools, and report back \
             using the sentinel rules (LOOP_OK / LOOP_SILENT: ... / LOOP_VOICE: ...).\n\n\
             ## Prior lessons (most recent first — DO NOT repeat these failures)\n\n{}\n\n\
             ## Allowed tools\n{}\n\n\
             ## Priorities (AUTONOMOUS.md)\n\n{}",
            if prior_lessons.trim().is_empty() {
                "(no prior lessons logged yet)".to_string()
            } else {
                prior_lessons
            },
            palette
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            priorities
        );

        let mut messages: Vec<ChatMessage> = Vec::new();
        if !system_prompt.is_empty() {
            messages.push(ChatMessage::system(&system_prompt));
        }
        messages.push(ChatMessage::user(&user_prompt));

        // 5. Tool-call loop, bounded.
        let ctx = JobContext::with_user(&self.config.notify_user_id, "autonomous", "autonomous-tick");
        let mut final_text = String::new();
        for iter in 0..self.config.max_iterations_per_tick {
            let request = ToolCompletionRequest::new(messages.clone(), palette.clone())
                .with_max_tokens(2048)
                .with_temperature(0.4)
                .with_tool_choice("auto");
            let resp = self
                .llm
                .complete_with_tools(request)
                .await
                .map_err(|e| format!("llm: {e}"))?;

            tracing::info!(
                "autonomous iter {}: {} tool calls, finish={:?}",
                iter,
                resp.tool_calls.len(),
                resp.finish_reason
            );

            if resp.tool_calls.is_empty() {
                final_text = resp.content.unwrap_or_default();
                break;
            }

            // Record the assistant turn with tool calls so providers
            // that need it (OpenAI-shape) replay correctly.
            messages.push(ChatMessage::assistant_with_tool_calls(
                resp.content.clone().unwrap_or_default(),
                resp.tool_calls.clone(),
            ));

            for tc in resp.tool_calls {
                let (result_text, is_error) = self.dispatch_tool(&tc, &ctx).await;
                tracing::info!(
                    "autonomous tool {} → {} ({} chars)",
                    tc.name,
                    if is_error { "ERROR" } else { "ok" },
                    result_text.len()
                );
                messages.push(ChatMessage::tool_result(&tc.id, &tc.name, result_text));
            }
        }

        // 6. Interpret sentinel + emit + record outcome in feedback log.
        let elapsed = started.elapsed();
        let report = classify_report(&final_text);
        let outcome_summary = match &report {
            LoopReport::Ok => format!("autonomous tick LOOP_OK in {:?}", elapsed),
            LoopReport::Silent(line) => format!("LOOP_SILENT: {}", line),
            LoopReport::Voice(line) => format!("LOOP_VOICE: {}", line),
        };
        // Feedback: record what this tick concluded so future ticks
        // can see the trajectory. Silent failures here never bubble up.
        crate::tools::builtin::feedback_record_silently(
            self.workspace.as_ref(),
            "outcome",
            &outcome_summary,
            "autonomous tick result — useful as context for the next tick",
            None,
        )
        .await;
        match report {
            LoopReport::Ok => {
                tracing::info!("autonomous tick LOOP_OK in {:?}", elapsed);
            }
            LoopReport::Silent(line) => {
                tracing::info!("autonomous tick LOOP_SILENT: {} ({:?})", line, elapsed);
                self.emit(&line, false).await;
            }
            LoopReport::Voice(line) => {
                tracing::info!("autonomous tick LOOP_VOICE: {} ({:?})", line, elapsed);
                self.emit(&line, true).await;
            }
        }
        Ok(())
    }

    /// Walk the registry, drop denied tools, then filter to the allow
    /// list if one was configured. Returns the tool DEFINITIONS the
    /// model gets to see.
    async fn build_tool_palette(&self) -> Vec<ToolDefinition> {
        let names = self.tools.list().await;
        let mut out: Vec<ToolDefinition> = Vec::new();
        for name in names {
            if self.config.denied.contains(&name) {
                continue;
            }
            if let Some(allowed) = &self.config.allowed {
                if !allowed.contains(&name) {
                    continue;
                }
            }
            let Some(tool) = self.tools.get(&name).await else {
                continue;
            };
            out.push(ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            });
        }
        out
    }

    /// Run one tool call, defending against deny-list bypass even if a
    /// hallucinated tool name slipped through. Returns the tool-result
    /// string plus an is_error flag (only for logging — the tool_result
    /// itself is what the model sees).
    async fn dispatch_tool(&self, tc: &ToolCall, ctx: &JobContext) -> (String, bool) {
        if self.config.denied.contains(&tc.name) {
            return (
                format!("Error: tool '{}' is on the autonomous deny list", tc.name),
                true,
            );
        }
        if let Some(allowed) = &self.config.allowed {
            if !allowed.contains(&tc.name) {
                return (
                    format!("Error: tool '{}' is not in autonomous allow list", tc.name),
                    true,
                );
            }
        }
        let Some(tool) = self.tools.get(&tc.name).await else {
            return (format!("Error: unknown tool '{}'", tc.name), true);
        };

        let result = tokio::time::timeout(
            Duration::from_secs(120),
            tool.execute(tc.arguments.clone(), ctx),
        )
        .await;
        match result {
            Ok(Ok(output)) => {
                let text = serde_json::to_string_pretty(&output.result)
                    .unwrap_or_else(|_| "<serialize failure>".to_string());
                (text, false)
            }
            Ok(Err(e)) => (format!("Error: {}", e), true),
            Err(_) => (format!("Error: tool '{}' timed out", tc.name), true),
        }
    }

    /// Push a report to the channel. `voice` flag is metadata only —
    /// the broadcast layer can use it to gate ElevenLabs.
    async fn emit(&self, message: &str, voice: bool) {
        let Some(tx) = self.response_tx.as_ref() else {
            return;
        };
        let chip = if voice { "loop+voice" } else { "loop" };
        let content = format!("[{}] {}", chip, message);
        let resp = OutgoingResponse {
            content,
            thread_id: None,
            metadata: serde_json::json!({
                "source": "autonomous_loop",
                "chip": chip,
                "voice": voice,
            }),
        };
        if let Err(e) = tx.send(resp).await {
            tracing::warn!("autonomous emit failed: {}", e);
        }
    }
}

/// Pull the most recent feedback-log entries from the workspace and
/// format them as a compact prompt block. Bounded at 15 entries / ~1500
/// chars so the prompt doesn't bloat. If there are no entries yet (first
/// run on a fresh install), returns empty so the caller can write a
/// placeholder. Silent on errors — feedback failures must not block
/// the loop.
async fn load_recent_lessons(workspace: &Workspace) -> String {
    let today = chrono::Utc::now().date_naive();
    let mut lines: Vec<String> = Vec::new();
    for d in 0..3 {
        let date = today - chrono::Duration::days(d);
        let path = format!("feedback/{}.md", date.format("%Y-%m-%d"));
        let Ok(doc) = workspace.read(&path).await else {
            continue;
        };
        let mut day_lines: Vec<String> = Vec::new();
        for line in doc.content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("```") || line.starts_with('#') {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let lesson = entry.get("lesson").and_then(|v| v.as_str()).unwrap_or("");
            // Skip the loop's own "outcome" entries from prior ticks —
            // they're noise; we want failure lessons and reflections.
            if kind == "outcome" {
                continue;
            }
            day_lines.push(format!("- [{}] {}", kind, lesson));
        }
        day_lines.reverse();
        lines.extend(day_lines);
        if lines.len() >= 15 {
            break;
        }
    }
    lines.truncate(15);
    lines.join("\n")
}

enum LoopReport {
    Ok,
    Silent(String),
    Voice(String),
}

fn classify_report(text: &str) -> LoopReport {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains("LOOP_OK") {
        return LoopReport::Ok;
    }
    if let Some(rest) = trimmed.strip_prefix("LOOP_VOICE:") {
        return LoopReport::Voice(rest.trim().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("LOOP_SILENT:") {
        return LoopReport::Silent(rest.trim().to_string());
    }
    // Sentinel-less response. During stabilization we treat this as
    // silent so the loop can't accidentally start broadcasting every
    // 5 minutes. Trim to a single line to keep transcript compact.
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    LoopReport::Silent(format!("(no sentinel) {}", first_line))
}

pub fn spawn_autonomous_loop(
    config: AutonomousLoopConfig,
    workspace: Arc<Workspace>,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    response_tx: Option<mpsc::Sender<OutgoingResponse>>,
) -> tokio::task::JoinHandle<()> {
    let mut runner = AutonomousLoopRunner::new(config, workspace, llm, tools);
    if let Some(tx) = response_tx {
        runner = runner.with_response_channel(tx);
    }
    tokio::spawn(async move {
        runner.run().await;
    })
}

// Silence the unused Role import if conditional compilation removes
// uses below — keeps the import explicit for readers.
#[allow(dead_code)]
fn _role_marker(r: Role) -> Role {
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_loop_ok() {
        assert!(matches!(classify_report("LOOP_OK"), LoopReport::Ok));
        assert!(matches!(classify_report(""), LoopReport::Ok));
        assert!(matches!(
            classify_report("Everything fine. LOOP_OK"),
            LoopReport::Ok
        ));
    }

    #[test]
    fn classify_silent_and_voice() {
        match classify_report("LOOP_SILENT: observed cargo warnings") {
            LoopReport::Silent(s) => assert_eq!(s, "observed cargo warnings"),
            _ => panic!("expected silent"),
        }
        match classify_report("LOOP_VOICE: fixed the focus bug") {
            LoopReport::Voice(s) => assert_eq!(s, "fixed the focus bug"),
            _ => panic!("expected voice"),
        }
    }

    #[test]
    fn classify_without_sentinel_falls_back_silent() {
        match classify_report("I think I should poke at the cargo build") {
            LoopReport::Silent(s) => assert!(s.starts_with("(no sentinel)")),
            _ => panic!("expected silent fallback"),
        }
    }
}
