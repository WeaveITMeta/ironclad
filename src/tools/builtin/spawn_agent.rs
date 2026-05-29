//! Sub-agent dispatcher.
//!
//! Lets the main JARVIS (Haiku, fast voice) delegate a focused subtask to a
//! beefier model (Sonnet for normal reasoning, Opus for hard reasoning).
//! Each spawn:
//!   - Runs a *fresh* chat history seeded only with the task description.
//!   - Sees only a curated subset of tools (default: read-only).
//!   - Returns a structured summary with token usage + cost so the main
//!     agent can speak the cost back to McKale and offer to extend.
//!
//! Hard-cap (Option A) approval: if `cap_hit: true`, the main agent
//! decides whether to spawn a continuation. No mid-flight budget prompts.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::channels::{ChannelManager, OutgoingResponse};
use crate::context::{ContextManager, JobContext, JobState};
use crate::llm::{
    ChatMessage, LlmProvider, ToolCompletionRequest, ToolDefinition,
};
use crate::tools::registry::ToolRegistry;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

/// Read-only tool set the sub-agent gets if the caller omits the `tools`
/// param. Side-effect tools (write, shell, browser navigate) must be
/// explicitly opted in, per spawn.
fn default_safe_tools() -> Vec<String> {
    [
        "vault_search",
        "vault_read",
        "vault_list",
        "memory_search",
        "memory_read",
        "memory_tree",
        "github_list_prs",
        "github_get_pr",
        "github_list_issues",
        "github_list_repos",
        "github_recent_commits",
        "time",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Approximate USD cost per million tokens. Public Anthropic pricing as of
/// 2026-05. Off by a few cents under volume discounts but fine for "did
/// I just burn 5 cents or 5 dollars" sanity.
const HAIKU_IN_PER_M: f64 = 1.0;
const HAIKU_OUT_PER_M: f64 = 5.0;
const SONNET_IN_PER_M: f64 = 3.0;
const SONNET_OUT_PER_M: f64 = 15.0;
const OPUS_IN_PER_M: f64 = 15.0;
const OPUS_OUT_PER_M: f64 = 75.0;

fn estimate_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (in_rate, out_rate) = match model {
        "opus" => (OPUS_IN_PER_M, OPUS_OUT_PER_M),
        "sonnet" => (SONNET_IN_PER_M, SONNET_OUT_PER_M),
        _ => (HAIKU_IN_PER_M, HAIKU_OUT_PER_M),
    };
    let cents = (input_tokens as f64 * in_rate + output_tokens as f64 * out_rate) / 1_000_000.0;
    (cents * 10000.0).round() / 10000.0
}

/// First 8 chars of the UUID — enough to disambiguate in conversation,
/// short enough to speak. ("sub-agent job 4f3a-..." not "job
/// 4f3aef21-09b8-4c61-...").
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Convert dollars to "N cents" or "$N.NN" depending on size, for voice
/// readout. Sub-agent costs are usually pennies, so "3 cents" reads
/// better than "$0.03".
fn cents(dollars: f64) -> String {
    let c = (dollars * 100.0).round();
    if c < 100.0 {
        format!("{} cents", c as i64)
    } else {
        format!("${:.2}", dollars)
    }
}

/// Trim a string so JARVIS's spoken headline doesn't drag on for two
/// minutes. Cuts at a word boundary when possible.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let cut = s[..max].rfind(' ').unwrap_or(max);
    format!("{}…", &s[..cut])
}

fn build_system_prompt(task: &str, allowed_tools: &[Arc<dyn Tool>]) -> String {
    format!(
        "You are a focused sub-agent dispatched by JARVIS (a voice assistant) to do \
         exactly one task and report the result. \n\n\
         ## Your task\n{}\n\n\
         ## Rules\n\
         - Call tools as needed. You have access to: {}.\n\
         - When the task is complete, respond with a SHORT plain-text summary \
         (no markdown, no code fences) of what you did and the headline finding. \
         That summary goes back to JARVIS, who will speak the key point aloud to \
         the user.\n\
         - Don't ask the user clarifying questions — you can't reach them. Make \
         reasonable assumptions and proceed.\n\
         - Stop when the task is done. Don't pad.\n",
        task,
        allowed_tools.iter().map(|t| t.name()).collect::<Vec<_>>().join(", "),
    )
}

/// Per-name conversation history for sub-agents that carry context
/// across calls. The friendly identity from `agent_names::identify` (or
/// whatever the caller passes as `name`) keys this map: subsequent
/// spawns with the same name receive the prior turns in their initial
/// message list, so "Memory Diver" stops re-introducing herself every
/// time and can build on what she did before.
///
/// Stored entirely in memory: gateway restart drops everything. Cap is
/// MAX_HISTORY_MESSAGES so a long-lived identity can't blow context.
#[derive(Default)]
pub struct SubAgentSessions {
    inner: tokio::sync::RwLock<std::collections::HashMap<String, Vec<ChatMessage>>>,
}

const MAX_HISTORY_MESSAGES: usize = 20;

impl SubAgentSessions {
    pub fn new() -> Self {
        Self::default()
    }

    async fn load(&self, name: &str) -> Vec<ChatMessage> {
        self.inner
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    async fn save(&self, name: &str, messages: Vec<ChatMessage>) {
        let mut trimmed = messages;
        // Keep the head (system + first user) intact and trim the
        // middle. Without this, repeated calls with a name eventually
        // include every prior turn — costly and noisy. 20 messages is
        // ~10 turn pairs, enough to reference "what we did last time"
        // without ballooning context.
        if trimmed.len() > MAX_HISTORY_MESSAGES {
            let head = trimmed.drain(..2).collect::<Vec<_>>();
            let tail_keep = MAX_HISTORY_MESSAGES.saturating_sub(2);
            let drop_n = trimmed.len().saturating_sub(tail_keep);
            trimmed.drain(..drop_n);
            let mut out = head;
            out.extend(trimmed);
            trimmed = out;
        }
        self.inner
            .write()
            .await
            .insert(name.to_string(), trimmed);
    }
}

pub struct SpawnAgentTool {
    registry: Arc<ToolRegistry>,
    haiku: Arc<dyn LlmProvider>,
    sonnet: Arc<dyn LlmProvider>,
    opus: Arc<dyn LlmProvider>,
    channels: Arc<ChannelManager>,
    context_manager: Arc<ContextManager>,
    sessions: Arc<SubAgentSessions>,
}

impl SpawnAgentTool {
    pub fn new(
        registry: Arc<ToolRegistry>,
        haiku: Arc<dyn LlmProvider>,
        sonnet: Arc<dyn LlmProvider>,
        opus: Arc<dyn LlmProvider>,
        channels: Arc<ChannelManager>,
        context_manager: Arc<ContextManager>,
    ) -> Self {
        Self {
            registry,
            haiku,
            sonnet,
            opus,
            channels,
            context_manager,
            sessions: Arc::new(SubAgentSessions::new()),
        }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Delegate a focused subtask to a sub-agent running on Sonnet (default, \
         good reasoning) or Opus (highest quality, ~5x cost). The sub-agent \
         starts with a fresh chat history seeded only by your `task` string \
         and sees only the `tools` you list (default: read-only set). It \
         runs to natural completion or the turn cap, then returns a JSON \
         summary with token usage, dollar cost, and `cap_hit: true` if it \
         ran out of budget mid-thought. \
         \
         Use this when: (a) the task needs deeper reasoning than Haiku, \
         (b) you want to isolate context (e.g., a triage run shouldn't \
         leak into the user's chat), or (c) the task is parallelizable \
         and you'll spawn several. \
         \
         If `cap_hit: true`, tell McKale the cost so far and ask if he wants \
         you to spawn a continuation. Don't auto-extend without checking."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Clear task description with explicit success criteria. \
                                    Treat this as the entire prompt the sub-agent will see — \
                                    it has no memory of your conversation, only the system \
                                    instructions and this task."
                },
                "model": {
                    "type": "string",
                    "enum": ["haiku", "sonnet", "opus"],
                    "default": "sonnet",
                    "description": "Haiku 4.5 for cheap parallel work at the same tier as main JARVIS (4-8 parallel reads, simple summaries). Sonnet 4.6 for most reasoning (default). Opus 4.7 for hardest tasks (code review, deep research, strategy)."
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool names the sub-agent can call. Omit for read-only \
                                    default. Include 'vault_write' for missions that write \
                                    a report; 'http' for web calls; 'shell' for code execution."
                },
                "max_turns": {
                    "type": "integer",
                    "default": 15,
                    "maximum": 50,
                    "description": "Maximum tool-call iterations before stopping. Each turn = one LLM call."
                },
                "max_tokens": {
                    "type": "integer",
                    "default": 8000,
                    "maximum": 64000,
                    "description": "Per-LLM-call max_tokens for the sub-agent's responses."
                },
                "name": {
                    "type": "string",
                    "description": "Persistent identity for this sub-agent. When set, the prior \
                                    conversation under the same name is loaded so the agent \
                                    builds on what it did before (e.g. 'Memory Diver' \
                                    remembers her last search and refines it). Same name \
                                    reuses the same sub-agent card in the right panel. \
                                    Omit for a one-shot fresh agent."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, params: Value, ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let task = params
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidParameters("missing 'task'".into()))?
            .trim()
            .to_string();
        if task.is_empty() {
            return Err(ToolError::InvalidParameters("'task' is empty".into()));
        }
        let model_choice = params
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("sonnet")
            .to_string();
        let env_max_turns = std::env::var("SUBAGENT_MAX_TURNS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(15);
        let max_turns = params
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .unwrap_or(env_max_turns)
            .min(50);
        let env_max_tokens = std::env::var("SUBAGENT_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(8000);
        let max_tokens = params
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(env_max_tokens)
            .min(64000) as u32;

        // Optional persistent-identity name. When present, we hydrate
        // the sub-agent's message list with whatever it did in prior
        // turns under the same name. Empty/missing → one-shot agent.
        let session_name = params
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let tool_names: Vec<String> = params
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(default_safe_tools);

        // Provider selection.
        let (provider, model_label) = match model_choice.as_str() {
            "haiku" => (Arc::clone(&self.haiku), "claude-haiku-4-5"),
            "opus" => (Arc::clone(&self.opus), "claude-opus-4-7"),
            _ => (Arc::clone(&self.sonnet), "claude-sonnet-4-6"),
        };

        // Resolve tools from the registry up front so we can validate the
        // spawn synchronously — McKale gets an immediate "good or not"
        // signal instead of finding out 30 seconds in that a tool was
        // misnamed.
        //
        // Supports trailing-`*` glob suffixes so missions can lock a
        // namespace cleanly. Examples:
        //   "playwright_marketing_*"  → only the marketing-Gmail profile
        //   "vault_*"                 → all vault tools (read/write/move/...)
        //   "github_list_*"           → just the read-only listing tools
        // Exact names still work; combine freely.
        let registry_names: Vec<String> = self.registry.list().await;
        let mut allowed_tools: Vec<Arc<dyn Tool>> = Vec::with_capacity(tool_names.len());
        let mut missing_tools: Vec<String> = Vec::new();
        let mut already_added: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for pattern in &tool_names {
            if pattern == "spawn_agent" {
                // Block self-recursion. Sub-agents spawning sub-agents is
                // a real feature later; for v1 it's a footgun.
                continue;
            }
            if let Some(prefix) = pattern.strip_suffix('*') {
                let mut matched = 0usize;
                for n in &registry_names {
                    if n == "spawn_agent" {
                        continue;
                    }
                    if n.starts_with(prefix) && !already_added.contains(n) {
                        if let Some(tool) = self.registry.get(n).await {
                            already_added.insert(n.clone());
                            allowed_tools.push(tool);
                            matched += 1;
                        }
                    }
                }
                if matched == 0 {
                    missing_tools.push(pattern.clone());
                }
            } else if !already_added.contains(pattern) {
                match self.registry.get(pattern).await {
                    Some(tool) => {
                        already_added.insert(pattern.clone());
                        allowed_tools.push(tool);
                    }
                    None => missing_tools.push(pattern.clone()),
                }
            }
        }

        let tool_defs: Vec<ToolDefinition> = allowed_tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
            })
            .collect();

        // Register the sub-agent as a real Iron Clad job so the existing
        // `list_jobs` / `job_status` / `cancel_job` tools see it. The
        // job's title is a 80-char truncation of the task so McKale can
        // scan a list and know what's running; the description carries
        // the model + tool count.
        let user_id = ctx.user_id.clone();
        let job_title = truncate(&task, 80);
        let job_description = format!(
            "Sub-agent on {} with {} tool(s)",
            model_label,
            allowed_tools.len()
        );
        let job_uuid = self
            .context_manager
            .create_job_for_user(user_id.clone(), job_title, job_description)
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "failed to create job for sub-agent: {e}"
                ))
            })?;

        // Move job into InProgress immediately so it shows up as active.
        let _ = self
            .context_manager
            .update_context(job_uuid, |c| {
                let _ = c.transition_to(JobState::InProgress, Some("sub-agent dispatched".into()));
                c.started_at = Some(chrono::Utc::now());
                // Stash the spawn parameters in metadata for forensics.
                c.metadata = json!({
                    "kind": "subagent",
                    "model": model_label,
                    "max_turns": max_turns,
                    "max_tokens": max_tokens,
                    "tools": tool_defs.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                });
            })
            .await;

        let job_id = job_uuid.to_string();

        // Capture everything the background task needs by value so the
        // spawned future is 'static.
        let task_for_bg = task.clone();
        let model_choice_bg = model_choice.clone();
        let model_label_bg = model_label.to_string();
        let job_id_bg = job_id.clone();
        let job_uuid_bg = job_uuid;
        let channels_bg = Arc::clone(&self.channels);
        let context_manager_bg = Arc::clone(&self.context_manager);
        let user_id_bg = user_id.clone();
        let allowed_tools_bg = allowed_tools.clone();
        let tool_defs_bg = tool_defs.clone();
        let sessions_bg = Arc::clone(&self.sessions);
        let session_name_bg = session_name.clone();

        // === FIRE-AND-FORGET ===
        //
        // The actual sub-agent loop runs detached. The main JARVIS agent
        // continues immediately so the user can speak again, fire a second
        // spawn_agent, or anything else. When the loop completes we
        // broadcast the result as an unsolicited assistant message via
        // the gateway channel — the dashboard receives it as a normal
        // SseEvent::Response and TTS reads the headline aloud.
        tokio::spawn(async move {
            // Hydrate the message list. If a `name` was provided and we
            // have a saved transcript for that identity, prepend it so
            // the sub-agent picks up where it left off. The new user
            // turn (the current `task`) is always appended last.
            let mut messages: Vec<ChatMessage> = if let Some(ref n) = session_name_bg {
                let prior = sessions_bg.load(n).await;
                if prior.is_empty() {
                    vec![
                        ChatMessage::system(build_system_prompt(&task_for_bg, &allowed_tools_bg)),
                        ChatMessage::user(&task_for_bg),
                    ]
                } else {
                    // Reuse the prior system + history, just add the
                    // new task as the next user turn. The system prompt
                    // is regenerated to reflect the current allowed
                    // tools (which may differ from last spawn) — drop
                    // the stored system message and substitute fresh.
                    let mut hydrated = Vec::with_capacity(prior.len() + 2);
                    hydrated.push(ChatMessage::system(build_system_prompt(
                        &task_for_bg,
                        &allowed_tools_bg,
                    )));
                    // Skip the prior system message (index 0); keep the
                    // rest. If the prior history doesn't start with
                    // system, take everything.
                    let skip = if matches!(
                        prior.first(),
                        Some(m) if matches!(m.role, crate::llm::Role::System)
                    ) {
                        1
                    } else {
                        0
                    };
                    hydrated.extend(prior.into_iter().skip(skip));
                    hydrated.push(ChatMessage::user(&task_for_bg));
                    hydrated
                }
            } else {
                vec![
                    ChatMessage::system(build_system_prompt(&task_for_bg, &allowed_tools_bg)),
                    ChatMessage::user(&task_for_bg),
                ]
            };

            let bg_ctx = JobContext::with_user(
                &user_id_bg,
                "subagent",
                format!("Sub-agent job {}", job_id_bg),
            );

            let mut total_input_tokens: u32 = 0;
            let mut total_output_tokens: u32 = 0;
            let mut tools_called: Vec<String> = Vec::new();
            let mut summary = String::new();
            let mut cap_hit = false;
            let mut turns_used: u32 = 0;
            let mut llm_error: Option<String> = None;

            for turn in 0..max_turns {
                turns_used = (turn as u32) + 1;
                let request = ToolCompletionRequest::new(messages.clone(), tool_defs_bg.clone())
                    .with_max_tokens(max_tokens)
                    .with_temperature(0.5)
                    .with_tool_choice("auto");
                let response = match provider.complete_with_tools(request).await {
                    Ok(r) => r,
                    Err(e) => {
                        llm_error = Some(format!("LLM call failed: {e}"));
                        break;
                    }
                };
                total_input_tokens = total_input_tokens.saturating_add(response.input_tokens);
                total_output_tokens = total_output_tokens.saturating_add(response.output_tokens);

                if response.tool_calls.is_empty() {
                    summary = response.content.unwrap_or_default();
                    break;
                }

                messages.push(ChatMessage::assistant_with_tool_calls(
                    response.content.clone().unwrap_or_default(),
                    response.tool_calls.clone(),
                ));

                for tc in &response.tool_calls {
                    tools_called.push(tc.name.clone());
                    let tool = match allowed_tools_bg.iter().find(|t| t.name() == tc.name) {
                        Some(t) => t,
                        None => {
                            let err_msg = format!(
                                "Tool '{}' was requested but isn't in this sub-agent's allowed set.",
                                tc.name
                            );
                            messages.push(ChatMessage::tool_result(&tc.id, &tc.name, err_msg));
                            continue;
                        }
                    };
                    let exec_result = tool.execute(tc.arguments.clone(), &bg_ctx).await;
                    let result_str = match exec_result {
                        Ok(out) => serde_json::to_string(&out.result).unwrap_or_else(|e| {
                            format!("(could not serialize tool output: {e})")
                        }),
                        Err(e) => format!("Error: {e}"),
                    };
                    messages.push(ChatMessage::tool_result(&tc.id, &tc.name, result_str));
                }

                if (turn + 1) >= max_turns {
                    cap_hit = true;
                    if summary.is_empty() {
                        summary = response.content.unwrap_or_else(|| {
                            format!(
                                "Sub-agent hit the {max_turns}-turn cap before producing a final \
                                 summary. Last action: called {}.",
                                tools_called.last().map(String::as_str).unwrap_or("nothing"),
                            )
                        });
                    }
                }
            }

            let cost = estimate_cost(&model_choice_bg, total_input_tokens, total_output_tokens);

            // Build the spoken message FIRST (borrows from owned data),
            // then move owned data into the job's metadata so list_jobs /
            // job_status surface real results.
            let spoken = if let Some(err) = &llm_error {
                format!(
                    "Sub-agent job {} failed: {}. Spent {} turns, about {} cents.",
                    short_id(&job_id_bg),
                    err,
                    turns_used,
                    cents(cost)
                )
            } else if cap_hit {
                format!(
                    "Sub-agent job {} hit the turn cap. Partial result: {}. Spent {} turns, \
                     about {} cents. Should I keep going?",
                    short_id(&job_id_bg),
                    truncate(&summary, 220),
                    turns_used,
                    cents(cost)
                )
            } else {
                format!(
                    "Sub-agent job {} done: {}. {} turns, {} cents on {}.",
                    short_id(&job_id_bg),
                    truncate(&summary, 260),
                    turns_used,
                    cents(cost),
                    model_label_bg
                )
            };

            // Final state transition + metadata stash. Moves owned values.
            let final_state = if llm_error.is_some() {
                JobState::Failed
            } else {
                JobState::Completed
            };
            let final_reason = llm_error.clone();
            let cost_decimal = rust_decimal::Decimal::from_f64_retain(cost).unwrap_or_default();
            let metadata_model = model_label_bg.clone();
            let metadata_summary = summary.clone();
            let metadata_tools_called = tools_called.clone();
            let metadata_error = llm_error.clone();
            let _ = context_manager_bg
                .update_context(job_uuid_bg, move |c| {
                    let _ = c.transition_to(final_state, final_reason);
                    c.completed_at = Some(chrono::Utc::now());
                    c.actual_cost = cost_decimal;
                    c.metadata = json!({
                        "kind": "subagent",
                        "model": metadata_model,
                        "summary": metadata_summary,
                        "turns_used": turns_used,
                        "max_turns": max_turns,
                        "input_tokens": total_input_tokens,
                        "output_tokens": total_output_tokens,
                        "cost_dollars": cost,
                        "tools_called": metadata_tools_called,
                        "cap_hit": cap_hit,
                        "error": metadata_error,
                    });
                })
                .await;

            tracing::info!(
                job_id = %job_id_bg,
                model = %model_label_bg,
                turns = turns_used,
                tokens_in = total_input_tokens,
                tokens_out = total_output_tokens,
                cost_dollars = cost,
                cap_hit = cap_hit,
                error = ?llm_error,
                "sub-agent finished"
            );

            if let Err(e) = channels_bg
                .broadcast("gateway", &user_id_bg, OutgoingResponse::text(spoken))
                .await
            {
                tracing::warn!(job_id = %job_id_bg, error = ?e, "broadcasting sub-agent result failed");
            }

            // Persist the conversation under the identity name so the
            // next spawn with the same name resumes here. Append the
            // final assistant summary as a synthetic message so the
            // next turn sees what we concluded last time. If no name
            // was provided, this is a no-op.
            if let Some(name) = session_name_bg {
                if llm_error.is_none() && !summary.is_empty() {
                    messages.push(ChatMessage::assistant(&summary));
                }
                sessions_bg.save(&name, messages).await;
            }
        });

        // Immediate return — main JARVIS gets this as the tool result and
        // can keep talking to McKale.
        let dispatched_summary = format!(
            "Sub-agent {} dispatched on {} with {} tools{}. I'll surface the result when it finishes; you can keep talking to me.",
            short_id(&job_id),
            model_label,
            allowed_tools.len(),
            if missing_tools.is_empty() {
                String::new()
            } else {
                format!(" (skipped unknown: {})", missing_tools.join(", "))
            },
        );
        let output_json = json!({
            "status": "dispatched",
            "job_id": job_id,
            "model": model_label,
            "tools_allowed": tool_defs.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
            "tools_skipped_unknown": missing_tools,
            "message_to_user": dispatched_summary,
        });
        Ok(ToolOutput::success(output_json, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}
