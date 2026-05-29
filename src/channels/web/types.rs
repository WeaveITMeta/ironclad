//! Request and response DTOs for the web gateway API.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

// --- Chat ---

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    pub content: String,
    pub thread_id: Option<String>,
    /// Base64-encoded PNG screenshots to send alongside the text turn.
    /// Stored without the `data:image/png;base64,` prefix. The dashboard's
    /// "look at screen" button captures a frame and inlines it here.
    #[serde(default)]
    pub images: Option<Vec<String>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SendMessageResponse {
    pub message_id: Uuid,
    pub status: &'static str,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadInfo {
    pub id: Uuid,
    pub state: String,
    pub turn_count: usize,
    /// Optional user-assigned title (set via the sidebar's
    /// right-click → Rename). Absent when the user hasn't renamed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// User-pinned: float this thread to the top of the sidebar.
    #[serde(default)]
    pub pinned: bool,
    /// Venture (collapsible sidebar group) this thread belongs to.
    /// Absent / null means "loose" (shown below all ventures).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venture_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VentureInfo {
    pub id: Uuid,
    pub name: String,
    pub collapsed: bool,
    pub created_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadListResponse {
    pub threads: Vec<ThreadInfo>,
    pub active_thread: Option<Uuid>,
    /// All ventures defined for this user. The sidebar bucket assigns
    /// threads to ventures by matching `thread.venture_id`; threads
    /// pointing at a venture id not in this list (e.g. one that was
    /// deleted by another client mid-poll) are treated as loose.
    #[serde(default)]
    pub ventures: Vec<VentureInfo>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TurnInfo {
    pub turn_number: usize,
    pub user_input: String,
    pub response: Option<String>,
    pub state: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub tool_calls: Vec<ToolCallInfo>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ToolCallInfo {
    pub name: String,
    pub has_result: bool,
    pub has_error: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HistoryResponse {
    pub thread_id: Uuid,
    pub turns: Vec<TurnInfo>,
}

// --- Approval ---

#[derive(Debug, Deserialize, ToSchema)]
pub struct ApprovalRequest {
    pub request_id: String,
    /// "approve", "always", or "deny"
    pub action: String,
}

// --- SSE Event Types ---

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum SseEvent {
    #[serde(rename = "response")]
    Response { content: String, thread_id: String },
    #[serde(rename = "thinking")]
    Thinking { message: String },
    #[serde(rename = "tool_started")]
    ToolStarted { name: String },
    #[serde(rename = "tool_completed")]
    ToolCompleted { name: String, success: bool },
    #[serde(rename = "tool_result")]
    ToolResult { name: String, preview: String },
    #[serde(rename = "stream_chunk")]
    StreamChunk { content: String },
    #[serde(rename = "status")]
    Status { message: String },
    #[serde(rename = "approval_needed")]
    ApprovalNeeded {
        request_id: String,
        tool_name: String,
        description: String,
        parameters: String,
    },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "heartbeat")]
    Heartbeat,
    /// A sub-agent (parallel sub-task, background worker, fan-out
    /// task) started running. `id` uniquely identifies this sub-agent
    /// so subsequent progress/completed events can update the same
    /// row; `kind` is one of "tool_exec" | "background" | "job".
    #[serde(rename = "sub_agent_started")]
    SubAgentStarted {
        id: String,
        label: String,
        kind: String,
    },
    /// In-flight progress message for a previously-started sub-agent.
    #[serde(rename = "sub_agent_progress")]
    SubAgentProgress { id: String, message: String },
    /// A sub-agent finished. Caller marks the row done.
    #[serde(rename = "sub_agent_completed")]
    SubAgentCompleted {
        id: String,
        success: bool,
        summary: String,
    },
}

// --- Memory ---

#[derive(Debug, Serialize, ToSchema)]
pub struct MemoryTreeResponse {
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TreeEntry {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MemoryListResponse {
    pub path: String,
    pub entries: Vec<ListEntry>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MemoryReadResponse {
    pub path: String,
    pub content: String,
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct MemoryWriteRequest {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MemoryWriteResponse {
    pub path: String,
    pub status: &'static str,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct MemorySearchRequest {
    pub query: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MemorySearchResponse {
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SearchHit {
    pub path: String,
    pub content: String,
    pub score: f64,
}

// --- Jobs ---

#[derive(Debug, Serialize, ToSchema)]
pub struct JobInfo {
    pub id: Uuid,
    pub title: String,
    pub state: String,
    pub user_id: String,
    pub created_at: String,
    pub started_at: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct JobListResponse {
    pub jobs: Vec<JobInfo>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct JobSummaryResponse {
    pub total: usize,
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub failed: usize,
    pub stuck: usize,
}

// --- Extensions ---

#[derive(Debug, Serialize, ToSchema)]
pub struct ExtensionInfo {
    pub name: String,
    pub kind: String,
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub authenticated: bool,
    pub active: bool,
    pub tools: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExtensionListResponse {
    pub extensions: Vec<ExtensionInfo>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ToolListResponse {
    pub tools: Vec<ToolInfo>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct InstallExtensionRequest {
    pub name: String,
    pub url: Option<String>,
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ActionResponse {
    pub success: bool,
    pub message: String,
    /// Auth URL to open (when activation requires OAuth).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    /// Whether the extension is waiting for a manual token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting_token: Option<bool>,
    /// Instructions for manual token entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl ActionResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            auth_url: None,
            awaiting_token: None,
            instructions: None,
        }
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            auth_url: None,
            awaiting_token: None,
            instructions: None,
        }
    }
}

// --- WebSocket ---

/// Message sent by a WebSocket client to the server.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum WsClientMessage {
    /// Send a chat message to the agent.
    #[serde(rename = "message")]
    Message {
        content: String,
        thread_id: Option<String>,
    },
    /// Approve or deny a pending tool execution.
    #[serde(rename = "approval")]
    Approval {
        request_id: String,
        /// "approve", "always", or "deny"
        action: String,
    },
    /// Client heartbeat ping.
    #[serde(rename = "ping")]
    Ping,
}

/// Message sent by the server to a WebSocket client.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum WsServerMessage {
    /// An SSE-style event forwarded over WebSocket.
    #[serde(rename = "event")]
    Event {
        /// The event sub-type (response, thinking, tool_started, etc.)
        event_type: String,
        /// The event payload as a JSON value.
        data: serde_json::Value,
    },
    /// Server heartbeat pong.
    #[serde(rename = "pong")]
    Pong,
    /// Error message.
    #[serde(rename = "error")]
    Error { message: String },
}

impl WsServerMessage {
    /// Create a WsServerMessage from an SseEvent.
    pub fn from_sse_event(event: &SseEvent) -> Self {
        let event_type = match event {
            SseEvent::Response { .. } => "response",
            SseEvent::Thinking { .. } => "thinking",
            SseEvent::ToolStarted { .. } => "tool_started",
            SseEvent::ToolCompleted { .. } => "tool_completed",
            SseEvent::ToolResult { .. } => "tool_result",
            SseEvent::StreamChunk { .. } => "stream_chunk",
            SseEvent::Status { .. } => "status",
            SseEvent::ApprovalNeeded { .. } => "approval_needed",
            SseEvent::Error { .. } => "error",
            SseEvent::Heartbeat => "heartbeat",
            SseEvent::SubAgentStarted { .. } => "sub_agent_started",
            SseEvent::SubAgentProgress { .. } => "sub_agent_progress",
            SseEvent::SubAgentCompleted { .. } => "sub_agent_completed",
        };
        let data = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
        WsServerMessage::Event {
            event_type: event_type.to_string(),
            data,
        }
    }
}

// --- Health ---

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: &'static str,
    pub channel: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- WsClientMessage deserialization tests ----

    #[test]
    fn test_ws_client_message_parse() {
        let json = r#"{"type":"message","content":"hello","thread_id":"t1"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Message { content, thread_id } => {
                assert_eq!(content, "hello");
                assert_eq!(thread_id.as_deref(), Some("t1"));
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_ws_client_message_no_thread() {
        let json = r#"{"type":"message","content":"hi"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Message { content, thread_id } => {
                assert_eq!(content, "hi");
                assert!(thread_id.is_none());
            }
            _ => panic!("Expected Message variant"),
        }
    }

    #[test]
    fn test_ws_client_approval_parse() {
        let json = r#"{"type":"approval","request_id":"abc-123","action":"approve"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            WsClientMessage::Approval { request_id, action } => {
                assert_eq!(request_id, "abc-123");
                assert_eq!(action, "approve");
            }
            _ => panic!("Expected Approval variant"),
        }
    }

    #[test]
    fn test_ws_client_ping_parse() {
        let json = r#"{"type":"ping"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMessage::Ping));
    }

    #[test]
    fn test_ws_client_unknown_type_fails() {
        let json = r#"{"type":"unknown"}"#;
        let result: Result<WsClientMessage, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ---- WsServerMessage serialization tests ----

    #[test]
    fn test_ws_server_pong_serialize() {
        let msg = WsServerMessage::Pong;
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"pong"}"#);
    }

    #[test]
    fn test_ws_server_error_serialize() {
        let msg = WsServerMessage::Error {
            message: "bad request".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["message"], "bad request");
    }

    #[test]
    fn test_ws_server_from_sse_response() {
        let sse = SseEvent::Response {
            content: "hello".to_string(),
            thread_id: "t1".to_string(),
        };
        let ws = WsServerMessage::from_sse_event(&sse);
        match ws {
            WsServerMessage::Event { event_type, data } => {
                assert_eq!(event_type, "response");
                assert_eq!(data["content"], "hello");
                assert_eq!(data["thread_id"], "t1");
            }
            _ => panic!("Expected Event variant"),
        }
    }

    #[test]
    fn test_ws_server_from_sse_thinking() {
        let sse = SseEvent::Thinking {
            message: "reasoning...".to_string(),
        };
        let ws = WsServerMessage::from_sse_event(&sse);
        match ws {
            WsServerMessage::Event { event_type, data } => {
                assert_eq!(event_type, "thinking");
                assert_eq!(data["message"], "reasoning...");
            }
            _ => panic!("Expected Event variant"),
        }
    }

    #[test]
    fn test_ws_server_from_sse_approval_needed() {
        let sse = SseEvent::ApprovalNeeded {
            request_id: "r1".to_string(),
            tool_name: "shell".to_string(),
            description: "Run ls".to_string(),
            parameters: "{}".to_string(),
        };
        let ws = WsServerMessage::from_sse_event(&sse);
        match ws {
            WsServerMessage::Event { event_type, data } => {
                assert_eq!(event_type, "approval_needed");
                assert_eq!(data["tool_name"], "shell");
            }
            _ => panic!("Expected Event variant"),
        }
    }

    #[test]
    fn test_ws_server_from_sse_heartbeat() {
        let sse = SseEvent::Heartbeat;
        let ws = WsServerMessage::from_sse_event(&sse);
        match ws {
            WsServerMessage::Event { event_type, .. } => {
                assert_eq!(event_type, "heartbeat");
            }
            _ => panic!("Expected Event variant"),
        }
    }
}
