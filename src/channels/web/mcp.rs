//! MCP server transport over HTTP for the gateway.
//!
//! Exposes JARVIS as an MCP server so external clients (Claude Code
//! CLI, Claude Code Desktop App, future agents) can drive a
//! self-directing build loop:
//!
//!   1. The client (e.g. me, mid-session) calls `tell_jarvis` with a
//!      short summary of what it just finished.
//!   2. The client calls `wait_for_jarvis_response`, which blocks
//!      until JARVIS streams its next chat response on the broadcast
//!      bus.
//!   3. The tool returns `{ text, done }`. When `done == true`
//!      (JARVIS replied with the literal sentinel `"DONE"`), the
//!      client stops the loop. Otherwise it treats `text` as the
//!      next directive and executes, then back to step 1.
//!
//! ## Transport
//!
//! Minimal Streamable HTTP — POST `/api/mcp` with a JSON-RPC 2.0
//! envelope, response is a single JSON body (no SSE streaming on the
//! response). This is the simplest variant of the spec that Claude
//! Code's MCP client accepts. Session IDs are not assigned; the
//! server is stateless (every tool call resubscribes to the SSE bus
//! fresh).
//!
//! ## Auth
//!
//! The gateway binds to `127.0.0.1` by default. `/api/gateway/token`
//! is itself unauthenticated for bootstrap reasons, so adding auth to
//! `/api/mcp` would gate it behind a value already obtainable by
//! anyone on the loopback. We accept the loopback-bind trust model
//! and leave this route open. If/when the gateway ever binds to a
//! non-loopback interface, harden this path first.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::{Json, extract::State};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::channels::IncomingMessage;
use crate::channels::web::server::GatewayState;
use crate::channels::web::sse::SseManager;
use crate::channels::web::types::SseEvent;

/// State the MCP transport needs that doesn't fit in the broader
/// `GatewayState`. Owns a per-gateway event mailbox that buffers SSE
/// events between `wait_for_*` tool calls so a sub-agent finishing
/// mid-task (when no wait is in flight) doesn't lose its completion
/// event.
///
/// Single mailbox by design — this gateway is single-user, and the
/// MCP loop is sequential (Claude Code waits, acts, then waits
/// again). If multiple concurrent MCP clients ever need isolation,
/// upgrade to a per-session map keyed by `Mcp-Session-Id`.
pub struct McpState {
    /// One-time flag for spawning the broadcast→mailbox forwarder
    /// task. Set on the first MCP request.
    forwarder_started: AtomicBool,
    /// Drain side of the mailbox. The forwarder pushes every
    /// `SseEvent` here; `wait_for_*` tools pop with filtering.
    /// `Mutex<Option<...>>` so multiple in-flight wait calls
    /// serialize (the build loop is sequential anyway).
    mailbox: Mutex<Option<mpsc::UnboundedReceiver<SseEvent>>>,
}

impl McpState {
    pub fn new() -> Self {
        Self {
            forwarder_started: AtomicBool::new(false),
            mailbox: Mutex::new(None),
        }
    }

    /// Spawn the broadcast→mailbox forwarder on first call. Idempotent
    /// via the `forwarder_started` flag. The mpsc sender stays alive
    /// inside the spawned task; the receiver lives in `self.mailbox`.
    pub async fn ensure_forwarder(&self, sse: Arc<SseManager>) {
        if self
            .forwarder_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // already started
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *self.mailbox.lock().await = Some(rx);
        tokio::spawn(async move {
            let mut stream = std::pin::pin!(sse.subscribe_raw());
            while let Some(event) = stream.next().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
    }
}

impl Default for McpState {
    fn default() -> Self {
        Self::new()
    }
}

/// JSON-RPC 2.0 request envelope. `id` may be string, number, or null
/// — we keep it as a generic `Value` so we can echo it back unchanged.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Tool descriptions are LOAD-BEARING: they're how the MCP client
/// (Claude Code) discovers WHEN to use these tools. The tell-then-wait
/// loop pattern is documented in both descriptions so an agent reading
/// the tool list sees the intended usage shape without out-of-band
/// instructions.
const TELL_JARVIS_DESC: &str =
    "Send a message to JARVIS — the user's voice-first AI assistant — and \
     return immediately. Use this AFTER finishing any user-requested task to \
     report what was shipped. Pair with `wait_for_jarvis_response` to receive \
     the next directive in a self-directing build loop. Continue the loop \
     until `wait_for_jarvis_response` returns `{ done: true }`, which \
     signals JARVIS sent the literal sentinel 'DONE' and the session should \
     end. If no thread_id is supplied, the message goes to JARVIS's currently \
     active thread.";

const GET_ACTIVE_THREAD_DESC: &str =
    "Return the UUID of JARVIS's currently active conversation thread + a \
     compact list of recent threads with titles. Use this AT THE START of \
     a build-loop session to establish which conversation you'll be \
     reporting into, then pass that UUID as `thread_id` on every \
     subsequent `tell_jarvis` call. Without a sticky thread_id, each \
     `tell_jarvis` defaults to the gateway's active_thread which can \
     drift if the user is bouncing between conversations — you'd end up \
     reporting completions into a thread the user isn't watching. The \
     returned `threads` array is sorted by `updated_at` descending so \
     index 0 is the most recently active.";

const WAIT_FOR_JARVIS_DESC: &str =
    "Block until JARVIS or a sub-agent emits a notable event, then return \
     `{ kind, text, done, ... }`. \
     `kind` is one of: \
     - 'response' — JARVIS's top-level chat reply. Read `text` as the next \
     directive (or empty string when done==true). \
     - 'subagent_completed' — a sub-agent finished. Read `text` as the \
     completion summary and `subagent_id` / `success` for context. \
     `done` is true iff the JARVIS response contains the literal sentinel \
     'DONE' on its own line (terminate the loop). \
     This drains a buffered mailbox first, so events emitted while you \
     were busy working between calls are NOT lost. Pairs with `tell_jarvis` \
     to drive a self-directing build loop: ship → tell → wait → act → \
     repeat. Pass `kinds: [\"response\"]` to ignore sub-agent events; pass \
     `kinds: [\"subagent_completed\"]` to wait specifically for sub-agent \
     completions. Default kinds: both. Default timeout: 600 seconds.";

const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 600;

/// POST handler for the `/api/mcp` route. Implements the minimal MCP
/// surface — `initialize`, `tools/list`, `tools/call`, and
/// `notifications/initialized` (no-op).
pub async fn mcp_handler(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    // Start the broadcast→mailbox forwarder on EVERY request (idempotent
    // via an internal AtomicBool). This way a client that calls
    // `tell_jarvis` before its first `wait_for_jarvis_response` still
    // captures the resulting Response event into the mailbox — without
    // this, the forwarder would only start on the first wait, and any
    // event emitted in the gap would be lost.
    state.mcp.ensure_forwarder(Arc::clone(&state.sse)).await;
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => Json(JsonRpcResponse::ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": false }
                },
                "serverInfo": {
                    "name": "jarvis-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )),
        "notifications/initialized" => {
            // Per spec this is a notification (no response expected),
            // but we return an empty success in case the client treats
            // it as a request. Harmless either way.
            Json(JsonRpcResponse::ok(id, json!({})))
        }
        "tools/list" => Json(JsonRpcResponse::ok(id, json!({ "tools": tool_list() }))),
        "tools/call" => handle_tool_call(state, id, req.params).await,
        other => Json(JsonRpcResponse::err(
            id,
            -32601,
            format!("method not found: {other}"),
        )),
    }
}

fn tool_list() -> Value {
    json!([
        {
            "name": "tell_jarvis",
            "description": TELL_JARVIS_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "What you want to tell JARVIS — typically a brief summary of what you just shipped."
                    },
                    "thread_id": {
                        "type": "string",
                        "description": "Optional. Specific thread UUID to send to. Defaults to JARVIS's active thread."
                    }
                },
                "required": ["message"]
            }
        },
        {
            "name": "get_active_thread",
            "description": GET_ACTIVE_THREAD_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "wait_for_jarvis_response",
            "description": WAIT_FOR_JARVIS_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Optional. Only return events from this thread. Defaults to any thread."
                    },
                    "kinds": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["response", "subagent_completed"] },
                        "description": "Optional. Event types to wait for. Defaults to both."
                    },
                    "timeout_secs": {
                        "type": "number",
                        "description": "Optional. Maximum seconds to wait before timing out. Defaults to 600 (10 minutes)."
                    }
                }
            }
        }
    ])
}

async fn handle_tool_call(
    state: Arc<GatewayState>,
    id: Option<Value>,
    params: Option<Value>,
) -> Json<JsonRpcResponse> {
    let params = match params {
        Some(p) => p,
        None => return Json(JsonRpcResponse::err(id, -32602, "missing params")),
    };
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));

    match name.as_str() {
        "tell_jarvis" => tool_tell_jarvis(state, id, arguments).await,
        "get_active_thread" => tool_get_active_thread(state, id).await,
        "wait_for_jarvis_response" => tool_wait_for_jarvis(state, id, arguments).await,
        other => Json(JsonRpcResponse::err(
            id,
            -32602,
            format!("unknown tool: {other}"),
        )),
    }
}

async fn tool_get_active_thread(
    state: Arc<GatewayState>,
    id: Option<Value>,
) -> Json<JsonRpcResponse> {
    let Some(session_manager) = state.session_manager.as_ref() else {
        return Json(JsonRpcResponse::err(
            id,
            -32000,
            "session manager unavailable",
        ));
    };
    let session = session_manager.get_or_create_session(&state.user_id).await;
    let sess = session.lock().await;
    let active = sess.active_thread.map(|u| u.to_string()).unwrap_or_default();

    // Compact recent-threads list sorted newest-first so the caller has
    // labeled options when picking a sticky thread. Cap at 20 — the
    // build loop needs a picker, not the full archive.
    let mut threads: Vec<Value> = sess
        .threads
        .values()
        .map(|t| {
            json!({
                "id": t.id.to_string(),
                "title": t.title,
                "turn_count": t.turns.len(),
                "updated_at": t.updated_at.to_rfc3339(),
            })
        })
        .collect();
    threads.sort_by(|a, b| {
        b.get("updated_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(a.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""))
    });
    threads.truncate(20);

    Json(tool_result_text(
        id,
        json!({
            "active_thread": active,
            "threads": threads,
        })
        .to_string(),
    ))
}

async fn tool_tell_jarvis(
    state: Arc<GatewayState>,
    id: Option<Value>,
    args: Value,
) -> Json<JsonRpcResponse> {
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if message.is_empty() {
        return Json(JsonRpcResponse::err(id, -32602, "`message` is required"));
    }
    let thread_id = args
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Channel name MUST match a registered channel in the ChannelManager
    // so the agent loop's `channels.respond(msg, ...)` lookup finds a
    // home for the reply. There's no separate "mcp" channel registered;
    // we ride on the existing `GatewayChannel` (registered under
    // "gateway"), which broadcasts SseEvent::Response via the same SSE
    // bus our mailbox forwarder subscribes to. Without this, the agent
    // loop processes the turn fine, but `ChannelManager.respond` returns
    // `Channel not found` and the response never broadcasts — the
    // wait_for_jarvis_response tool then times out even though JARVIS
    // produced a reply.
    let mut msg = IncomingMessage::new("gateway", &state.user_id, &message);
    if let Some(t) = thread_id.as_ref() {
        msg = msg.with_thread(t);
    }
    let msg_id = msg.id;

    let tx_guard = state.msg_tx.read().await;
    let tx = match tx_guard.as_ref() {
        Some(t) => t,
        None => {
            return Json(JsonRpcResponse::err(
                id,
                -32000,
                "gateway channel not started",
            ));
        }
    };
    if let Err(e) = tx.send(msg).await {
        return Json(JsonRpcResponse::err(
            id,
            -32000,
            format!("send failed: {e}"),
        ));
    }

    Json(tool_result_text(
        id,
        format!(
            "Message delivered to JARVIS (id={msg_id}). Call `wait_for_jarvis_response` next."
        ),
    ))
}

async fn tool_wait_for_jarvis(
    state: Arc<GatewayState>,
    id: Option<Value>,
    args: Value,
) -> Json<JsonRpcResponse> {
    let target_thread = args
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS);
    // Default: both kinds enabled. An empty/missing `kinds` field is
    // the same as the default.
    let (accept_response, accept_subagent) = match args.get("kinds").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => {
            let mut r = false;
            let mut s = false;
            for item in arr {
                match item.as_str() {
                    Some("response") => r = true,
                    Some("subagent_completed") => s = true,
                    _ => {}
                }
            }
            (r, s)
        }
        _ => (true, true),
    };

    // Make sure the broadcast→mailbox forwarder is running so events
    // emitted between calls land in the buffer.
    state
        .mcp
        .ensure_forwarder(Arc::clone(&state.sse))
        .await;

    // Drain the mailbox. We hold the lock for the duration of the
    // call so concurrent waits serialize (the loop pattern is
    // sequential anyway). Inside the lock we either receive a
    // matching event from the buffer or await one from the stream.
    let mut guard = state.mcp.mailbox.lock().await;
    let rx = match guard.as_mut() {
        Some(rx) => rx,
        None => {
            return Json(JsonRpcResponse::err(
                id,
                -32000,
                "mailbox uninitialized (forwarder failed to start)",
            ));
        }
    };

    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            let event = match rx.recv().await {
                Some(e) => e,
                None => return None,
            };
            if let Some(matched) =
                match_event(&event, &target_thread, accept_response, accept_subagent)
            {
                return Some(matched);
            }
            // Non-matching event — discard and keep waiting.
        }
    })
    .await;

    let matched = match result {
        Ok(Some(m)) => m,
        Ok(None) => {
            return Json(JsonRpcResponse::err(
                id,
                -32000,
                "event stream closed before a matching event arrived",
            ));
        }
        Err(_) => {
            return Json(tool_result_text(
                id,
                json!({
                    "kind": "",
                    "text": "",
                    "done": false,
                    "timed_out": true,
                })
                .to_string(),
            ));
        }
    };

    Json(tool_result_text(id, matched.to_string()))
}

/// Apply the wait-tool filter to one event. Returns the JSON payload
/// to send back when the event matches; `None` to skip and keep
/// waiting on the mailbox.
fn match_event(
    event: &SseEvent,
    target_thread: &Option<String>,
    accept_response: bool,
    accept_subagent: bool,
) -> Option<Value> {
    match event {
        SseEvent::Response { content, thread_id } if accept_response => {
            let thread_match = target_thread
                .as_ref()
                .map(|t| t == thread_id)
                .unwrap_or(true);
            if !thread_match {
                return None;
            }
            Some(json!({
                "kind": "response",
                "text": content,
                "thread_id": thread_id,
                "done": is_done_sentinel(content),
                "timed_out": false,
            }))
        }
        SseEvent::SubAgentCompleted { id, success, summary } if accept_subagent => {
            // Sub-agent events don't carry thread_id; the target_thread
            // filter only applies to Response events. If the caller
            // narrowed to a thread, they probably don't want sub-agent
            // noise for OTHER threads — but JARVIS doesn't tag sub-
            // agents with a thread, so we surface them regardless and
            // the caller can disambiguate via subagent_id.
            Some(json!({
                "kind": "subagent_completed",
                "text": summary,
                "subagent_id": id,
                "success": success,
                "done": false,
                "timed_out": false,
            }))
        }
        _ => None,
    }
}

/// True iff `text` contains the literal sentinel "DONE" as a
/// standalone token (case-insensitive). Conservative — requires
/// the word to be on its own line or surrounded by non-letter chars
/// so prose like "I'm done thinking" doesn't accidentally halt the
/// loop. JARVIS is expected to say DONE on a line by itself.
fn is_done_sentinel(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        let stripped = trimmed.trim_end_matches(|c: char| c == '.' || c == '!' || c == '?');
        if stripped.eq_ignore_ascii_case("done") {
            return true;
        }
    }
    false
}

/// Wrap a string into the MCP tools/call response envelope:
/// `{ content: [{ type: "text", text: "..." }] }`.
fn tool_result_text(id: Option<Value>, text: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse::ok(
        id,
        json!({
            "content": [{
                "type": "text",
                "text": text.into()
            }]
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_sentinel_standalone() {
        assert!(is_done_sentinel("DONE"));
        assert!(is_done_sentinel("done"));
        assert!(is_done_sentinel("Done."));
        assert!(is_done_sentinel("DONE!"));
        assert!(is_done_sentinel("Finished the task.\nDONE"));
        assert!(is_done_sentinel("First line\nDone.\nSecond line"));
    }

    #[test]
    fn done_sentinel_no_false_positive() {
        assert!(!is_done_sentinel("I'm done thinking about it"));
        assert!(!is_done_sentinel("Almost done"));
        assert!(!is_done_sentinel("done with that"));
        assert!(!is_done_sentinel(""));
    }
}
