//! Tool trait and types.

use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::JobContext;

/// Error type for tool execution.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Invalid parameters: {0}")]
    InvalidParameters(String),

    #[error("Execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Timeout after {0:?}")]
    Timeout(Duration),

    #[error("Not authorized: {0}")]
    NotAuthorized(String),

    #[error("Rate limited, retry after {0:?}")]
    RateLimited(Option<Duration>),

    #[error("External service error: {0}")]
    ExternalService(String),

    #[error("Sandbox error: {0}")]
    Sandbox(String),
}

/// Output from a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The result data.
    pub result: serde_json::Value,
    /// Cost incurred (if any).
    pub cost: Option<Decimal>,
    /// Time taken.
    pub duration: Duration,
    /// Raw output before sanitization (for debugging).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Optional base64-encoded PNG images this tool wants Claude to see
    /// as actual vision content (not just text). The agent loop plumbs
    /// these into the `tool_result` message as image content blocks so
    /// Anthropic's vision model can read pixel data directly. Strip the
    /// `data:image/png;base64,` prefix before pushing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl ToolOutput {
    /// Create a successful output with a JSON result.
    pub fn success(result: serde_json::Value, duration: Duration) -> Self {
        Self {
            result,
            cost: None,
            duration,
            raw: None,
            images: Vec::new(),
        }
    }

    /// Create a text output.
    pub fn text(text: impl Into<String>, duration: Duration) -> Self {
        Self {
            result: serde_json::Value::String(text.into()),
            cost: None,
            duration,
            raw: None,
            images: Vec::new(),
        }
    }

    /// Attach base64-encoded PNG images so Claude sees them as vision.
    pub fn with_images(mut self, images: Vec<String>) -> Self {
        self.images = images;
        self
    }

    /// Set the cost.
    pub fn with_cost(mut self, cost: Decimal) -> Self {
        self.cost = Some(cost);
        self
    }

    /// Set the raw output.
    pub fn with_raw(mut self, raw: impl Into<String>) -> Self {
        self.raw = Some(raw.into());
        self
    }
}

/// Definition of a tool's parameters using JSON Schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSchema {
    /// Create a new tool schema.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    /// Set the parameters schema.
    pub fn with_parameters(mut self, parameters: serde_json::Value) -> Self {
        self.parameters = parameters;
        self
    }
}

/// Trait for tools that the agent can use.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name.
    fn name(&self) -> &str;

    /// Get a description of what the tool does.
    fn description(&self) -> &str;

    /// Get the JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given parameters.
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError>;

    /// Estimate the cost of running this tool with the given parameters.
    fn estimated_cost(&self, _params: &serde_json::Value) -> Option<Decimal> {
        None
    }

    /// Estimate how long this tool will take with the given parameters.
    fn estimated_duration(&self, _params: &serde_json::Value) -> Option<Duration> {
        None
    }

    /// Whether this tool's output needs sanitization.
    ///
    /// Returns true for tools that interact with external services,
    /// where the output might contain malicious content.
    fn requires_sanitization(&self) -> bool {
        true
    }

    /// Whether this tool requires explicit user approval before execution.
    ///
    /// Returns false by default since most tools run in a sandboxed/virtualized
    /// environment. Only tools that make external network calls or perform
    /// destructive operations should return true.
    ///
    /// When true, the agent will prompt the user for confirmation before
    /// executing this tool.
    fn requires_approval(&self) -> bool {
        false
    }

    /// Per-call override of the static `requires_approval()` decision,
    /// inspecting the specific parameters of this invocation.
    ///
    /// Returns:
    /// - `Some(true)` to FORCE approval for this specific call, even
    ///   when the session has "Always approved" this tool. Use for
    ///   parameter patterns that are inherently dangerous regardless
    ///   of prior consent (e.g. `rm -rf`, `sudo` in a shell command).
    ///   Prevents one blanket "Always" click from unlocking write
    ///   access to everything that tool can reach.
    /// - `Some(false)` to SKIP approval entirely for this call. Use
    ///   for safe-by-pattern invocations of an otherwise-approval-
    ///   gated tool (e.g. `cargo build`, `git status` in shell — the
    ///   tool as a whole isn't safe, but these specific commands are).
    /// - `None` to fall through to the default policy: the static
    ///   `requires_approval()` decision combined with the session's
    ///   per-tool auto-approval set.
    ///
    /// Default returns `None` so existing tools are unaffected. Tools
    /// that want pattern-based gating override this.
    fn approval_override(&self, _params: &serde_json::Value) -> Option<bool> {
        None
    }

    /// Get the tool schema for LLM function calling.
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }
}

/// A simple no-op tool for testing.
#[allow(dead_code)]
#[derive(Debug)]
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input message. Useful for testing."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to echo back"
                }
            },
            "required": ["message"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidParameters("missing 'message' parameter".to_string())
            })?;

        Ok(ToolOutput::text(message, Duration::from_millis(1)))
    }

    fn requires_sanitization(&self) -> bool {
        false // Echo is a trusted internal tool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_echo_tool() {
        let tool = EchoTool;
        let ctx = JobContext::default();

        let result = tool
            .execute(serde_json::json!({"message": "hello"}), &ctx)
            .await
            .unwrap();

        assert_eq!(result.result, serde_json::json!("hello"));
    }

    #[test]
    fn test_tool_schema() {
        let tool = EchoTool;
        let schema = tool.schema();

        assert_eq!(schema.name, "echo");
        assert!(!schema.description.is_empty());
    }
}
