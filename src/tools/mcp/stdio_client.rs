//! Stdio MCP transport. The Streamable HTTP client in `client.rs` covers
//! remote MCP servers; this module covers local executables that speak MCP
//! over newline-delimited JSON-RPC on stdin/stdout (the canonical local
//! transport for things like Claude Desktop's MCP servers, Eustress, etc.).
//!
//! Lifecycle: one long-lived child process per server. Requests are sent
//! as one JSON line on stdin; the reader task parses stdout line-by-line
//! and dispatches each response to the waiter that owns its `id`. stderr
//! is drained into tracing logs.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, RwLock, oneshot};

use crate::context::JobContext;
use crate::tools::mcp::config::McpServerConfig;
use crate::tools::mcp::protocol::{CallToolResult, ListToolsResult, McpResponse, McpTool};
use crate::tools::tool::{Tool, ToolError, ToolOutput};

const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Spawned-and-attached MCP server speaking JSON-RPC over stdio.
pub struct StdioMcpClient {
    server_name: String,
    next_id: AtomicU64,
    /// Stdin pipe to the child. Wrapped in Mutex so concurrent callers
    /// serialize their writes; a partial write would corrupt the stream.
    stdin: Arc<Mutex<ChildStdin>>,
    /// Pending requests waiting on a response, keyed by JSON-RPC id.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<McpResponse>>>>,
    /// Cached tool list (populated lazily by `list_tools`).
    tools_cache: RwLock<Option<Vec<McpTool>>>,
    /// Hold onto the child so it doesn't get reaped while we're alive.
    /// JoinHandle for the reader/stderr drains is intentionally not stored
    /// — they exit when the child closes its pipes.
    _child: Arc<Mutex<Child>>,
}

impl StdioMcpClient {
    /// Spawn the configured executable and hand back a client that has
    /// already completed the MCP `initialize` handshake.
    pub async fn spawn(config: &McpServerConfig) -> Result<Self, ToolError> {
        let command = config.command.as_deref().ok_or_else(|| {
            ToolError::InvalidParameters("stdio MCP server missing `command`".to_string())
        })?;
        if !Path::new(command).exists() {
            return Err(ToolError::ExternalService(format!(
                "MCP executable not found: {}",
                command
            )));
        }

        let mut cmd = Command::new(command);
        if let Some(args) = config.args.as_ref() {
            cmd.args(args);
        }
        if let Some(env) = config.env.as_ref() {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::ExternalService(format!("spawn MCP: {}", e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::ExternalService("MCP stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::ExternalService("MCP stdout unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::ExternalService("MCP stderr unavailable".to_string()))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<McpResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Reader task: pull lines from stdout, dispatch by request id.
        let server_name_reader = config.name.clone();
        let pending_reader = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<McpResponse>(&line) {
                    Ok(resp) => {
                        let id = resp.id;
                        if let Some(tx) = pending_reader.lock().await.remove(&id) {
                            let _ = tx.send(resp);
                        } else {
                            tracing::debug!(
                                "MCP stdio '{}' response with no waiter (id={}): {}",
                                server_name_reader,
                                id,
                                line
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            "MCP stdio '{}' non-JSON line on stdout: {} ({})",
                            server_name_reader,
                            line,
                            e
                        );
                    }
                }
            }
            tracing::info!("MCP stdio '{}' stdout reader exited", server_name_reader);
        });

        // Stderr drain: log into tracing so server's own startup messages
        // and errors aren't black-holed.
        let server_name_err = config.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.is_empty() {
                    continue;
                }
                tracing::debug!("[mcp:{}] {}", server_name_err, line);
            }
        });

        let client = Self {
            server_name: config.name.clone(),
            next_id: AtomicU64::new(1),
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            tools_cache: RwLock::new(None),
            _child: Arc::new(Mutex::new(child)),
        };

        // MCP `initialize` handshake.
        client.initialize().await?;

        Ok(client)
    }

    async fn initialize(&self) -> Result<(), ToolError> {
        let params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "ironclad", "version": env!("CARGO_PKG_VERSION") }
        });
        let resp = self.request("initialize", params).await?;
        if let Some(err) = resp.error {
            return Err(ToolError::ExternalService(format!(
                "MCP initialize error: {}",
                err.message
            )));
        }
        // initialized notification (no response expected per spec).
        let notify = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let mut line = serde_json::to_string(&notify).unwrap_or_default();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| ToolError::ExternalService(format!("write initialized: {}", e)))?;
        stdin.flush().await.ok();
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<McpResponse, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let mut line = serde_json::to_string(&req)
            .map_err(|e| ToolError::ExternalService(format!("serialize request: {}", e)))?;
        let req_preview = if line.len() > 400 {
            format!("{}... ({} bytes)", &line[..400], line.len())
        } else {
            line.clone()
        };
        tracing::debug!(
            "mcp[{}/stdio] → {} :: {}",
            self.server_name, method, req_preview
        );
        line.push('\n');

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await.map_err(|e| {
                ToolError::ExternalService(format!("write stdio MCP request: {}", e))
            })?;
            stdin.flush().await.ok();
        }

        match tokio::time::timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS), rx).await {
            Ok(Ok(resp)) => {
                let resp_preview = serde_json::to_string(&resp).unwrap_or_default();
                let p = if resp_preview.len() > 400 {
                    format!("{}... ({} bytes)", &resp_preview[..400], resp_preview.len())
                } else {
                    resp_preview
                };
                tracing::debug!("mcp[{}/stdio] ← :: {}", self.server_name, p);
                Ok(resp)
            }
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(ToolError::ExternalService(format!(
                    "MCP stdio '{}' dropped before responding",
                    self.server_name
                )))
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(ToolError::Timeout(std::time::Duration::from_secs(
                    REQUEST_TIMEOUT_SECS,
                )))
            }
        }
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>, ToolError> {
        if let Some(cached) = self.tools_cache.read().await.as_ref() {
            return Ok(cached.clone());
        }
        let resp = self.request("tools/list", serde_json::json!({})).await?;
        if let Some(err) = resp.error {
            return Err(ToolError::ExternalService(format!(
                "tools/list error: {}",
                err.message
            )));
        }
        let result: ListToolsResult = serde_json::from_value(resp.result.unwrap_or(Value::Null))
            .map_err(|e| ToolError::ExternalService(format!("decode tools/list: {}", e)))?;
        *self.tools_cache.write().await = Some(result.tools.clone());
        Ok(result.tools)
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, ToolError> {
        let resp = self
            .request(
                "tools/call",
                serde_json::json!({ "name": tool_name, "arguments": arguments }),
            )
            .await?;
        if let Some(err) = resp.error {
            return Err(ToolError::ExecutionFailed(format!(
                "MCP tool '{}' error: {}",
                tool_name, err.message
            )));
        }
        serde_json::from_value(resp.result.unwrap_or(Value::Null))
            .map_err(|e| ToolError::ExternalService(format!("decode tools/call: {}", e)))
    }

    /// Wrap each remote tool as a local `Tool` impl that the registry can
    /// dispatch. Names are prefixed with the server name to avoid collisions.
    ///
    /// Heuristic: any tool whose (unprefixed) name starts with `delete_`,
    /// `drop_`, `reset_`, `clear_`, `purge_`, `destroy_`, `remove_`, or
    /// `wipe_` is auto-marked as destructive and approval-gated. Catches
    /// the obvious footguns in things like Eustress's 75-tool surface
    /// without us having to manually classify every single tool. Tools
    /// declared safe can still need approval if the registering server
    /// explicitly opts in later.
    pub async fn create_tools(self: &Arc<Self>) -> Result<Vec<Arc<dyn Tool>>, ToolError> {
        let mcp_tools = self.list_tools().await?;
        Ok(mcp_tools
            .into_iter()
            .map(|t| {
                let prefixed_name = format!("{}_{}", self.server_name, t.name);
                let destructive = is_destructive_tool_name(&t.name);
                Arc::new(StdioMcpToolWrapper {
                    tool: t,
                    prefixed_name,
                    client: Arc::clone(self),
                    destructive,
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

/// Name-prefix heuristic for MCP tools that mutate or destroy state.
/// Conservative; we'd rather pop one extra approval banner than silently
/// wipe a simulation universe.
pub(crate) fn is_destructive_tool_name(name: &str) -> bool {
    const DESTRUCTIVE_PREFIXES: &[&str] = &[
        "delete_", "drop_", "reset_", "clear_", "purge_", "destroy_", "remove_", "wipe_",
        "overwrite_", "kill_", "terminate_", "rollback_",
    ];
    let lower = name.to_ascii_lowercase();
    DESTRUCTIVE_PREFIXES.iter().any(|p| lower.starts_with(p))
}

struct StdioMcpToolWrapper {
    tool: McpTool,
    prefixed_name: String,
    client: Arc<StdioMcpClient>,
    /// Set at wrap-time by `is_destructive_tool_name`. Drives the approval
    /// gate without per-tool config.
    destructive: bool,
}

#[async_trait]
impl Tool for StdioMcpToolWrapper {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn description(&self) -> &str {
        &self.tool.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.tool.input_schema.clone()
    }

    fn requires_approval(&self) -> bool {
        self.destructive
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();
        let result = self.client.call_tool(&self.tool.name, params).await?;
        let content: String = result
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .collect::<Vec<_>>()
            .join("\n");
        if result.is_error {
            return Err(ToolError::ExecutionFailed(content));
        }
        Ok(ToolOutput::text(content, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true
    }
}
