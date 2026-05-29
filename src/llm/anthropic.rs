//! Anthropic Claude API provider.
//!
//! Talks directly to `https://api.anthropic.com/v1/messages` with an API key.
//! Implements `LlmProvider` by translating between Iron Clad's OpenAI-shaped
//! `ChatMessage` model and Anthropic's messages format:
//! - System messages are lifted to the top-level `system` field.
//! - Assistant tool calls become `tool_use` content blocks.
//! - Tool results become user messages with `tool_result` content blocks.

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::config::AnthropicConfig;
use crate::error::LlmError;
use crate::llm::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, FinishReason, LlmProvider, Role, ToolCall,
    ToolCompletionRequest, ToolCompletionResponse,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const PROVIDER: &str = "anthropic";

/// Direct Anthropic Claude API provider. No proxy, no session, just an API key.
pub struct AnthropicProvider {
    client: Client,
    config: AnthropicConfig,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider. Errors if no API key is configured.
    pub fn new(config: AnthropicConfig) -> Result<Self, LlmError> {
        if config.api_key.is_none() {
            return Err(LlmError::AuthFailed {
                provider: PROVIDER.to_string(),
            });
        }

        // 10-minute per-call ceiling. Most Anthropic calls finish in 5-30s
        // even on Opus; this exists to catch a wedged connection without
        // killing legitimate long Opus chains-of-thought mid-sub-agent.
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .unwrap_or_else(|_| Client::new());

        Ok(Self { client, config })
    }

    fn api_url(&self) -> String {
        format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'))
    }

    fn api_key(&self) -> String {
        self.config
            .api_key
            .as_ref()
            .map(|k| k.expose_secret().to_string())
            .unwrap_or_default()
    }

    async fn send(&self, body: &MessagesRequest) -> Result<MessagesResponse, LlmError> {
        let url = self.api_url();
        tracing::debug!("anthropic POST {}", url);

        let response = self
            .client
            .post(&url)
            .header("x-api-key", self.api_key())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("anthropic request failed: {}", e);
                LlmError::RequestFailed {
                    provider: PROVIDER.to_string(),
                    reason: e.to_string(),
                }
            })?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        tracing::debug!("anthropic status {} body: {}", status, text);

        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => LlmError::AuthFailed {
                    provider: PROVIDER.to_string(),
                },
                429 => LlmError::RateLimited {
                    provider: PROVIDER.to_string(),
                    retry_after: None,
                },
                _ => LlmError::RequestFailed {
                    provider: PROVIDER.to_string(),
                    reason: format!("HTTP {}: {}", status, text),
                },
            });
        }

        let parsed: MessagesResponse =
            serde_json::from_str(&text).map_err(|e| LlmError::InvalidResponse {
                provider: PROVIDER.to_string(),
                reason: format!("JSON parse: {}; raw: {}", e, text),
            })?;
        log_cache_usage(&parsed.usage);
        Ok(parsed)
    }
}

/// Surface prompt-cache hit ratio to logs so we can verify the prefix
/// is actually being reused. Anything above 0 cache_read_input_tokens
/// after the first call of a session means the breakpoint took.
fn log_cache_usage(u: &Usage) {
    if u.cache_read_input_tokens > 0 || u.cache_creation_input_tokens > 0 {
        let total = u.input_tokens + u.cache_read_input_tokens;
        let pct = if total > 0 {
            (u.cache_read_input_tokens as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        tracing::info!(
            "anthropic cache: read={} write={} fresh={} hit_rate={:.0}%",
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
            u.input_tokens,
            pct
        );
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn model_name(&self) -> &str {
        &self.config.model
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        // Defaults for Claude Sonnet 4.5: $3/Mtok input, $15/Mtok output.
        // Override at the model level once Iron Clad tracks per-model costs.
        (dec!(0.000003), dec!(0.000015))
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let (system, mut messages) = convert_messages(req.messages);
        apply_message_history_cache(&mut messages);
        let body = MessagesRequest {
            model: self.config.model.clone(),
            max_tokens: req.max_tokens.unwrap_or(self.config.default_max_tokens),
            messages,
            system,
            temperature: req.temperature,
            stop_sequences: req.stop_sequences,
            tools: None,
            tool_choice: None,
        };

        let resp = self.send(&body).await?;
        let content = resp
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(CompletionResponse {
            content,
            finish_reason: map_stop_reason(resp.stop_reason.as_deref()),
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
        })
    }

    async fn complete_with_tools(
        &self,
        req: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let (system, mut messages) = convert_messages(req.messages);
        apply_message_history_cache(&mut messages);
        let mut tools: Vec<AnthropicTool> = req
            .tools
            .into_iter()
            .map(|t| AnthropicTool {
                name: t.name,
                description: t.description,
                input_schema: t.parameters,
                cache_control: None,
            })
            .collect();
        // Cache the system + tools prefix. Last tool gets the marker,
        // which extends the cache breakpoint backward to cover every
        // earlier block. Huge for sub-agents and the autonomous loop
        // where the same 200-tool palette ships on every call.
        apply_prompt_cache(&mut tools);
        let tool_choice = req.tool_choice.map(map_tool_choice);

        let body = MessagesRequest {
            model: self.config.model.clone(),
            max_tokens: req.max_tokens.unwrap_or(self.config.default_max_tokens),
            messages,
            system,
            temperature: req.temperature,
            stop_sequences: None,
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice,
        };

        let resp = self.send(&body).await?;

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        for block in resp.content {
            match block {
                ContentBlock::Text { text, .. } => text_parts.push(text),
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: input,
                    });
                }
                ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {
                    // Models never emit tool_result or image; ignore if seen.
                }
            }
        }
        let content = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n"))
        };

        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolUse
        } else {
            map_stop_reason(resp.stop_reason.as_deref())
        };

        Ok(ToolCompletionResponse {
            content,
            tool_calls,
            finish_reason,
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
        })
    }

    /// Tool-aware streaming. Streams `text_delta` payloads to `on_chunk`
    /// while accumulating any `tool_use` content blocks (built up from
    /// `input_json_delta` events). Returns the full response including any
    /// tool calls the model made.
    async fn complete_with_tools_streaming(
        &self,
        request: ToolCompletionRequest,
        mut on_chunk: Box<dyn FnMut(String) + Send>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let (system, mut messages) = convert_messages(request.messages);
        apply_message_history_cache(&mut messages);
        let mut tools: Vec<AnthropicTool> = request
            .tools
            .into_iter()
            .map(|t| AnthropicTool {
                name: t.name,
                description: t.description,
                input_schema: t.parameters,
                cache_control: None,
            })
            .collect();
        apply_prompt_cache(&mut tools);
        let tool_choice = request.tool_choice.map(map_tool_choice);

        let body = StreamingMessagesRequest {
            model: self.config.model.clone(),
            max_tokens: request.max_tokens.unwrap_or(self.config.default_max_tokens),
            messages,
            system,
            temperature: request.temperature,
            stop_sequences: None,
            stream: true,
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice,
        };

        let mut state = run_stream(self, &body, &mut |chunk: &str| on_chunk(chunk.to_string()))
            .await?;

        let text_parts: Vec<String> = state.finalized_text.drain(..).collect();
        let content = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n"))
        };
        let tool_calls = state.finalized_tools.drain(..).collect::<Vec<_>>();
        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolUse
        } else {
            map_stop_reason(state.stop_reason.as_deref())
        };

        Ok(ToolCompletionResponse {
            content,
            tool_calls,
            finish_reason,
            input_tokens: state.input_tokens,
            output_tokens: state.output_tokens,
        })
    }

    /// Streaming variant of `complete`. Text-only path; ignores tool calls.
    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        mut on_chunk: Box<dyn FnMut(String) + Send>,
    ) -> Result<CompletionResponse, LlmError> {
        let (system, mut messages) = convert_messages(request.messages);
        apply_message_history_cache(&mut messages);
        let body = StreamingMessagesRequest {
            model: self.config.model.clone(),
            max_tokens: request.max_tokens.unwrap_or(self.config.default_max_tokens),
            messages,
            system,
            temperature: request.temperature,
            stop_sequences: request.stop_sequences,
            stream: true,
            tools: None,
            tool_choice: None,
        };

        let state = run_stream(self, &body, &mut |chunk: &str| on_chunk(chunk.to_string()))
            .await?;
        let content = state.finalized_text.join("\n");

        Ok(CompletionResponse {
            content,
            finish_reason: map_stop_reason(state.stop_reason.as_deref()),
            input_tokens: state.input_tokens,
            output_tokens: state.output_tokens,
        })
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// Accumulated state from a streaming Anthropic SSE pass. Both
/// `complete_streaming` and `complete_with_tools_streaming` feed this through
/// `run_stream` and then build their respective response shapes from it.
struct StreamState {
    /// One entry per completed `content_block` of type `text`.
    finalized_text: Vec<String>,
    /// One entry per completed `tool_use` content block.
    finalized_tools: Vec<ToolCall>,
    input_tokens: u32,
    output_tokens: u32,
    stop_reason: Option<String>,
}

enum CurrentBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        /// Accumulated `input_json_delta` strings; parsed when the block closes.
        partial_json: String,
    },
}

/// Issue the streaming POST and drain the SSE event stream into a `StreamState`.
/// `on_text` is called with each `text_delta` chunk as it arrives.
async fn run_stream(
    provider: &AnthropicProvider,
    body: &StreamingMessagesRequest,
    on_text: &mut (dyn FnMut(&str) + Send),
) -> Result<StreamState, LlmError> {
    let url = provider.api_url();
    tracing::debug!("anthropic stream POST {}", url);

    let response = provider
        .client
        .post(&url)
        .header("x-api-key", provider.api_key())
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| LlmError::RequestFailed {
            provider: PROVIDER.to_string(),
            reason: e.to_string(),
        })?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(match status.as_u16() {
            401 | 403 => LlmError::AuthFailed {
                provider: PROVIDER.to_string(),
            },
            429 => LlmError::RateLimited {
                provider: PROVIDER.to_string(),
                retry_after: None,
            },
            _ => LlmError::RequestFailed {
                provider: PROVIDER.to_string(),
                reason: format!("HTTP {}: {}", status, text),
            },
        });
    }

    let mut stream = response.bytes_stream();
    let mut buf = Vec::<u8>::new();
    let mut state = StreamState {
        finalized_text: Vec::new(),
        finalized_tools: Vec::new(),
        input_tokens: 0,
        output_tokens: 0,
        stop_reason: None,
    };
    let mut current: Option<CurrentBlock> = None;

    while let Some(item) = stream.next().await {
        let bytes = item.map_err(|e| LlmError::RequestFailed {
            provider: PROVIDER.to_string(),
            reason: format!("stream read: {e}"),
        })?;
        buf.extend_from_slice(&bytes);

        while let Some(pos) = find_double_newline(&buf) {
            let event_bytes = buf.drain(..pos + 2).collect::<Vec<u8>>();
            let event_str = String::from_utf8_lossy(&event_bytes);
            for line in event_str.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let ev: StreamEvent = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match ev {
                    StreamEvent::MessageStart { message } => {
                        if let Some(u) = message.usage {
                            state.input_tokens = u.input_tokens;
                        }
                    }
                    StreamEvent::ContentBlockStart { content_block, .. } => match content_block {
                        BlockStart::Text { .. } => {
                            current = Some(CurrentBlock::Text(String::new()));
                        }
                        BlockStart::ToolUse { id, name } => {
                            current = Some(CurrentBlock::ToolUse {
                                id,
                                name,
                                partial_json: String::new(),
                            });
                        }
                        BlockStart::Other => {
                            current = None;
                        }
                    },
                    StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                        DeltaPayload::TextDelta { text } => {
                            on_text(&text);
                            if let Some(CurrentBlock::Text(s)) = current.as_mut() {
                                s.push_str(&text);
                            }
                        }
                        DeltaPayload::InputJsonDelta { partial_json } => {
                            if let Some(CurrentBlock::ToolUse { partial_json: pj, .. }) =
                                current.as_mut()
                            {
                                pj.push_str(&partial_json);
                            }
                        }
                        DeltaPayload::Other => {}
                    },
                    StreamEvent::ContentBlockStop { .. } => {
                        if let Some(block) = current.take() {
                            match block {
                                CurrentBlock::Text(s) => state.finalized_text.push(s),
                                CurrentBlock::ToolUse {
                                    id,
                                    name,
                                    partial_json,
                                } => {
                                    let arguments = if partial_json.is_empty() {
                                        serde_json::json!({})
                                    } else {
                                        serde_json::from_str(&partial_json)
                                            .unwrap_or_else(|_| serde_json::json!({}))
                                    };
                                    state.finalized_tools.push(ToolCall {
                                        id,
                                        name,
                                        arguments,
                                    });
                                }
                            }
                        }
                    }
                    StreamEvent::MessageDelta { delta, usage } => {
                        if let Some(reason) = delta.stop_reason {
                            state.stop_reason = Some(reason);
                        }
                        if let Some(u) = usage {
                            state.output_tokens = u.output_tokens;
                        }
                    }
                    StreamEvent::Other => {}
                }
            }
        }
    }

    Ok(state)
}

// ---- Streaming wire types ----

#[derive(Serialize)]
struct StreamingMessagesRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum StreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: StreamMessageMeta },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        #[allow(dead_code)]
        index: u32,
        content_block: BlockStart,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        #[allow(dead_code)]
        index: u32,
        delta: DeltaPayload,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[allow(dead_code)]
        index: u32,
    },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: StopReasonDelta,
        usage: Option<StreamUsage>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum BlockStart {
    #[serde(rename = "text")]
    Text {
        #[allow(dead_code)]
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct StreamMessageMeta {
    usage: Option<StreamUsage>,
}

#[derive(Deserialize)]
struct StreamUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum DeltaPayload {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct StopReasonDelta {
    stop_reason: Option<String>,
}

// ---- Conversion helpers ----

fn convert_messages(msgs: Vec<ChatMessage>) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system_parts = Vec::new();
    let mut out = Vec::new();

    for m in msgs {
        match m.role {
            Role::System => system_parts.push(m.content),
            Role::User => {
                if let Some(imgs) = m.images.as_ref().filter(|v| !v.is_empty()) {
                    // Multi-modal: text + image blocks. Anthropic wants
                    // images BEFORE the text per their docs.
                    let mut blocks: Vec<ContentBlock> = imgs
                        .iter()
                        .map(|data| ContentBlock::Image {
                            source: ImageSource {
                                source_type: "base64".to_string(),
                                media_type: sniff_image_media_type(data).to_string(),
                                data: data.clone(),
                            },
                            cache_control: None,
                        })
                        .collect();
                    if !m.content.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: m.content,
                            cache_control: None,
                        });
                    }
                    out.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                } else {
                    out.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: AnthropicContent::Text(m.content),
                    });
                }
            }
            Role::Assistant => {
                if let Some(calls) = m.tool_calls {
                    let mut blocks = Vec::new();
                    if !m.content.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: m.content,
                            cache_control: None,
                        });
                    }
                    for c in calls {
                        blocks.push(ContentBlock::ToolUse {
                            id: c.id,
                            name: c.name,
                            input: c.arguments,
                        });
                    }
                    out.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                } else {
                    out.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicContent::Text(m.content),
                    });
                }
            }
            Role::Tool => {
                let tool_use_id = m.tool_call_id.unwrap_or_default();
                // If the tool attached images, emit them as standalone
                // image blocks AFTER the text-only tool_result. Anthropic's
                // tool_result content field is strict text-only, but the
                // adjacent image blocks in the same user message get
                // associated with the just-completed tool call by Claude
                // automatically — this is the documented pattern for
                // tools returning vision content.
                let mut blocks: Vec<ContentBlock> = vec![ContentBlock::ToolResult {
                    tool_use_id,
                    content: m.content,
                    cache_control: None,
                }];
                if let Some(imgs) = m.images.as_ref().filter(|v| !v.is_empty()) {
                    for data in imgs {
                        blocks.push(ContentBlock::Image {
                            source: ImageSource {
                                source_type: "base64".to_string(),
                                media_type: sniff_image_media_type(data).to_string(),
                                data: data.clone(),
                            },
                            cache_control: None,
                        });
                    }
                }
                out.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Blocks(blocks),
                });
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, out)
}

fn map_stop_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolUse,
        _ => FinishReason::Unknown,
    }
}

/// Detect the actual image format from a base64-encoded payload by
/// looking at the first few characters. Base64 of the binary magic
/// bytes is deterministic: PNG starts with `iVBORw0` (89 50 4E 47),
/// JPEG with `/9j/` (FF D8 FF), GIF with `R0lGOD` (47 49 46 38), WEBP
/// with `UklGR` (52 49 46 46). Falls back to `image/png` when the
/// prefix doesn't match any known format — that's the dominant case
/// historically and matches what the dashboard used to send.
///
/// The dashboard now sends JPEG for screen-share frames (much smaller
/// for photographic content). Without this sniff, the provider was
/// shipping `media_type: image/png` for JPEG bytes and Anthropic 400'd
/// the whole turn with "image was specified using the image/png media
/// type, but the image appears to be a image/jpeg image".
fn sniff_image_media_type(b64: &str) -> &'static str {
    if b64.starts_with("iVBORw0") {
        "image/png"
    } else if b64.starts_with("/9j/") {
        "image/jpeg"
    } else if b64.starts_with("R0lGOD") {
        "image/gif"
    } else if b64.starts_with("UklGR") {
        "image/webp"
    } else {
        "image/png"
    }
}

fn map_tool_choice(choice: String) -> AnthropicToolChoice {
    match choice.as_str() {
        "auto" => AnthropicToolChoice {
            tool_type: "auto".to_string(),
            name: None,
        },
        "required" | "any" => AnthropicToolChoice {
            tool_type: "any".to_string(),
            name: None,
        },
        "none" => AnthropicToolChoice {
            tool_type: "none".to_string(),
            name: None,
        },
        other => AnthropicToolChoice {
            tool_type: "tool".to_string(),
            name: Some(other.to_string()),
        },
    }
}

// ---- Anthropic API wire types ----

#[derive(Debug, Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
        /// Cache breakpoint marker. When set on the last block of the
        /// last message, Anthropic caches the entire system+tools+
        /// conversation prefix; only the LLM's new response on top of
        /// that prefix is billed at full input-token rate.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    /// Anthropic prompt cache marker. When present, Anthropic caches
    /// everything UP TO AND INCLUDING this block (so setting it on the
    /// last tool caches system + tools as a single prefix). 5-minute
    /// TTL by default — perfectly aligned with the autonomous loop's
    /// 5-min cadence and chat sessions in general.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// Anthropic cache breakpoint marker. `type: "ephemeral"` is the only
/// supported value today; the implicit TTL is 5 minutes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".to_string(),
        }
    }
}

/// Mark the last tool in the slice as a cache breakpoint so Anthropic
/// caches the entire system + tools prefix. Idempotent — if the slice
/// is empty, does nothing. Call this right before building the request
/// body; both streaming and non-streaming paths share it.
fn apply_prompt_cache(tools: &mut [AnthropicTool]) {
    if let Some(last) = tools.last_mut() {
        last.cache_control = Some(CacheControl::ephemeral());
    }
}

/// Mark the last block of the last message with a cache_control marker.
/// Combined with `apply_prompt_cache` on the tools list, this gives us
/// two cache breakpoints — one that covers system+tools, and a rolling
/// one that covers system+tools+conversation-history. Each turn, the
/// previous user message becomes part of the cached prefix; only the
/// newest assistant response is billed at full input-token rate.
///
/// Converts an AnthropicContent::Text into a Blocks-with-cache_control
/// when needed so the marker has somewhere to live. Skips short
/// conversations (under 2 messages) where caching would write more
/// than it saves.
fn apply_message_history_cache(messages: &mut Vec<AnthropicMessage>) {
    if messages.len() < 2 {
        return;
    }
    let Some(last) = messages.last_mut() else {
        return;
    };
    match &mut last.content {
        AnthropicContent::Text(text) => {
            let upgraded = vec![ContentBlock::Text {
                text: std::mem::take(text),
                cache_control: Some(CacheControl::ephemeral()),
            }];
            last.content = AnthropicContent::Blocks(upgraded);
        }
        AnthropicContent::Blocks(blocks) => {
            // Walk from the end and tag the first markable block. Images
            // and tool_results count; only ToolUse blocks (assistant-only)
            // don't carry cache_control in this codebase. Last markable
            // wins because cache_control there extends backward to
            // include everything earlier in the request.
            for block in blocks.iter_mut().rev() {
                match block {
                    ContentBlock::Text { cache_control, .. }
                    | ContentBlock::ToolResult { cache_control, .. }
                    | ContentBlock::Image { cache_control, .. } => {
                        *cache_control = Some(CacheControl::ephemeral());
                        return;
                    }
                    ContentBlock::ToolUse { .. } => continue,
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    tool_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[allow(dead_code)]
    id: String,
    content: Vec<ContentBlock>,
    #[allow(dead_code)]
    model: String,
    stop_reason: Option<String>,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    /// Tokens served from the prompt cache. When non-zero, the prefix
    /// (system + tools) was a hit and we're being billed at 10% of the
    /// normal input rate for those tokens.
    #[serde(default)]
    cache_read_input_tokens: u32,
    /// Tokens newly written to the prompt cache on this call (only
    /// relevant on the first call of a series; billed at 1.25x normal
    /// input but amortizes across subsequent reads).
    #[serde(default)]
    cache_creation_input_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_system_into_top_level() {
        let msgs = vec![
            ChatMessage::system("you are jarvis"),
            ChatMessage::user("hello"),
        ];
        let (system, m) = convert_messages(msgs);
        assert_eq!(system.as_deref(), Some("you are jarvis"));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].role, "user");
    }

    #[test]
    fn assistant_with_tool_calls_becomes_blocks() {
        let calls = vec![ToolCall {
            id: "t1".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"q": "foo"}),
        }];
        let msgs = vec![ChatMessage::assistant_with_tool_calls("", calls)];
        let (_, m) = convert_messages(msgs);
        match &m[0].content {
            AnthropicContent::Blocks(b) => {
                assert_eq!(b.len(), 1);
                assert!(matches!(b[0], ContentBlock::ToolUse { .. }));
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn tool_result_becomes_user_block() {
        let msgs = vec![ChatMessage::tool_result("t1", "search", "results")];
        let (_, m) = convert_messages(msgs);
        assert_eq!(m[0].role, "user");
        match &m[0].content {
            AnthropicContent::Blocks(b) => {
                assert!(matches!(b[0], ContentBlock::ToolResult { .. }));
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason(Some("end_turn")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("max_tokens")), FinishReason::Length);
        assert_eq!(map_stop_reason(Some("tool_use")), FinishReason::ToolUse);
        assert_eq!(map_stop_reason(None), FinishReason::Unknown);
    }
}
