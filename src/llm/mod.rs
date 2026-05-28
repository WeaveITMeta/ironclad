//! LLM integration for the agent.
//!
//! Single backend: **Anthropic Claude API**, accessed directly via `ANTHROPIC_API_KEY`.
//! NEAR AI and its session-based auth were removed in the 2026-05-26 rip.

mod anthropic;
mod provider;
mod reasoning;

pub use anthropic::AnthropicProvider;
pub use provider::{
    ChatMessage, CompletionRequest, CompletionResponse, LlmProvider, Role, ToolCall,
    ToolCompletionRequest, ToolCompletionResponse, ToolDefinition, ToolResult,
};
pub use reasoning::{ActionPlan, Reasoning, ReasoningContext, RespondResult, ToolSelection};

use std::sync::Arc;

use crate::config::LlmConfig;
use crate::error::LlmError;

/// Create the LLM provider. Single path: Anthropic Claude API.
pub fn create_llm_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    tracing::info!("LLM: Anthropic Claude API (model: {})", config.anthropic.model);
    Ok(Arc::new(AnthropicProvider::new(config.anthropic.clone())?))
}
