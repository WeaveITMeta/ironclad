//! Iron Clad gateway client.
//!
//! Three responsibilities:
//!   1. POST /api/chat/send — submit a user message
//!   2. GET  /api/chat/events — subscribe to the SSE stream of replies
//!   3. POST /api/voice/stt + /api/voice/tts_stream — voice round-trip
//!
//! The gateway is purely local (`http://127.0.0.1:3030`); no auth in
//! single-user mode. Multi-user deployments add a bearer token, which
//! we'll plumb in when that feature ships.
//!
//! All event handlers are async closures dispatched onto the tokio
//! runtime. UI updates happen via `slint::invoke_from_event_loop` so
//! the renderer thread owns the property writes.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eventsource_client::{Client, SSE};
use futures::stream::TryStreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

/// Per-endpoint circuit breaker state. Tracks consecutive failures and
/// suppresses requests during the backoff window. Reset on first
/// success. Capped at 30s total backoff so a long outage still gets
/// retried periodically.
#[derive(Default)]
struct Breaker {
    consecutive_failures: AtomicU32,
    /// Wall-clock millis-since-start when the breaker "opens" (i.e.
    /// when we should stop sending). 0 = closed (free to send).
    open_until_ms: AtomicU64,
}

impl Breaker {
    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
        self.open_until_ms.store(0, Ordering::Release);
    }

    fn record_failure(&self, start: Instant) {
        let n = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        // After 3 consecutive failures the breaker trips. Backoff =
        // 2^(n-3) seconds, capped at 30s.
        if n >= 3 {
            let exp = (n - 3).min(5);
            let backoff_secs = 2u64.pow(exp).min(30);
            let now_ms = start.elapsed().as_millis() as u64;
            self.open_until_ms
                .store(now_ms + backoff_secs * 1000, Ordering::Release);
        }
    }

    fn is_open(&self, start: Instant) -> bool {
        let open_until = self.open_until_ms.load(Ordering::Acquire);
        if open_until == 0 {
            return false;
        }
        let now_ms = start.elapsed().as_millis() as u64;
        now_ms < open_until
    }
}

/// One ChatEvent shape per route the dashboard listens for. We mirror
/// the dashboard's `ChatEvent` enum 1:1; field names match the gateway's
/// JSON wire format.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Response { content: String },
    StreamChunk { content: String },
    Status { message: String },
    ToolStarted { name: String },
    ToolCompleted { name: String, success: bool },
    ToolResult { name: String, preview: String },
    ApprovalNeeded {
        request_id: String,
        tool_name: String,
        description: String,
        parameters: serde_json::Value,
    },
    Thinking { message: String },
    Error { message: String },
    /// A sub-agent (parallel sub-task, background worker) has started.
    /// Renders as a row in the Sub-Agents panel below the transcript.
    SubAgentStarted {
        id: String,
        label: String,
        kind: String,
    },
    /// In-flight progress update for a previously-started sub-agent.
    /// Updates the matching row's status text.
    SubAgentProgress { id: String, message: String },
    /// Sub-agent finished. Row gets marked done with a colored dot
    /// (green ok / red fail) + final summary.
    SubAgentCompleted {
        id: String,
        success: bool,
        summary: String,
    },
}

#[derive(Debug, Clone, Serialize)]
struct SendMessageRequest<'a> {
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<&'a [String]>,
}

/// Compact thread metadata for the sidebar conversation list.
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadInfo {
    pub id: String,
    pub state: String,
    pub turn_count: usize,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadListResponse {
    pub threads: Vec<ThreadInfo>,
    pub active_thread: Option<String>,
}

/// One conversation turn as the gateway records it: the user's input
/// and (if the loop has finished) JARVIS's response. Returned in order
/// by `/api/chat/history`.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnInfo {
    pub turn_number: usize,
    pub user_input: String,
    pub response: Option<String>,
    #[allow(dead_code)]
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryResponse {
    #[allow(dead_code)]
    pub thread_id: String,
    pub turns: Vec<TurnInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct SendMessageResponse {
    message_id: String,
    status: String,
}

#[derive(Clone)]
pub struct Gateway {
    base_url: String,
    http: HttpClient,
    /// Bearer token for chat/voice endpoints. Fetched lazily from
    /// /api/gateway/token (unauthenticated endpoint) on first need;
    /// the gateway rotates this per boot, so we re-fetch on 401.
    token: Arc<tokio::sync::RwLock<Option<String>>>,
    /// Per-endpoint circuit breakers. Each endpoint trips after 3
    /// consecutive failures + backs off exponentially (2s, 4s, 8s,
    /// 16s, capped at 30s) before allowing the next attempt. Shared
    /// across cloned Gateway handles so all UI tasks observe the same
    /// open-circuit state.
    breakers: Arc<Breakers>,
    /// Reference Instant for breaker timestamp math. All durations are
    /// relative to this so we don't have to wrap SystemTime.
    epoch: Instant,
}

#[derive(Default)]
struct Breakers {
    chat_send: Breaker,
    chat_approval: Breaker,
    voice_stt: Breaker,
    voice_tts: Breaker,
    threads: Breaker,
    thread_new: Breaker,
}

impl Gateway {
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = HttpClient::builder()
            // Voice STT can take a few seconds for a long utterance;
            // TTS streams are open longer still. Long-poll friendly.
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest Client default build cannot fail");
        Self {
            base_url: base_url.into(),
            http,
            token: Arc::new(tokio::sync::RwLock::new(None)),
            breakers: Arc::new(Breakers::default()),
            epoch: Instant::now(),
        }
    }

    /// Run `builder` with auth + 401-retry + circuit-breaker semantics.
    /// `builder` must be callable twice (so we can retry after a token
    /// refresh); it receives the current bearer token each time and
    /// should return a freshly-configured RequestBuilder.
    ///
    /// Behavior:
    ///   - If the breaker is open, fail fast with a clear error (no
    ///     network round trip).
    ///   - On 401, invalidate the cached token + retry once.
    ///   - Record success/failure on the matching breaker.
    ///
    /// Non-2xx responses other than 401 still flow back to the caller
    /// for endpoint-specific error decoding; we only sniff 401 here.
    async fn authed_request<F>(&self, breaker: &Breaker, builder: F) -> Result<reqwest::Response>
    where
        F: Fn(&HttpClient, &str) -> reqwest::RequestBuilder,
    {
        if breaker.is_open(self.epoch) {
            anyhow::bail!("circuit breaker open — backing off after consecutive failures");
        }
        let token = self.ensure_token().await?;
        let resp = match builder(&self.http, &token).send().await {
            Ok(r) => r,
            Err(e) => {
                breaker.record_failure(self.epoch);
                return Err(anyhow::anyhow!("network error: {e}"));
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Refresh token + retry exactly once.
            self.invalidate_token().await;
            let new_token = self.ensure_token().await?;
            let resp = match builder(&self.http, &new_token).send().await {
                Ok(r) => r,
                Err(e) => {
                    breaker.record_failure(self.epoch);
                    return Err(anyhow::anyhow!("network error on retry: {e}"));
                }
            };
            if resp.status().is_success() {
                breaker.record_success();
            } else {
                breaker.record_failure(self.epoch);
            }
            return Ok(resp);
        }
        if resp.status().is_success() {
            breaker.record_success();
        } else if resp.status().is_server_error() {
            // 5xx counts as a failure for breaker purposes. 4xx (other
            // than 401 we already handled) is a "you did it wrong"
            // which won't be fixed by retry — don't trip the breaker.
            breaker.record_failure(self.epoch);
        }
        Ok(resp)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetch the gateway's current auth token from `/api/gateway/token`.
    /// Cached after the first successful fetch; cleared + refetched on
    /// a 401 elsewhere. Retries with a short delay if the gateway hasn't
    /// finished booting yet.
    pub async fn ensure_token(&self) -> Result<String> {
        if let Some(t) = self.token.read().await.clone() {
            return Ok(t);
        }
        // Polling fetch — gateway takes a moment to come up under jarvis_up.
        let url = format!("{}/api/gateway/token", self.base_url);
        let mut last_err = String::new();
        for attempt in 0..30 {
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    #[derive(Deserialize)]
                    struct TokenResp {
                        token: String,
                    }
                    let body: TokenResp = resp.json().await.context("decode token")?;
                    *self.token.write().await = Some(body.token.clone());
                    return Ok(body.token);
                }
                Ok(r) => last_err = format!("status {}", r.status()),
                Err(e) => last_err = format!("net {e}"),
            }
            tokio::time::sleep(Duration::from_millis(500 * (attempt + 1).min(10))).await;
        }
        anyhow::bail!("could not fetch gateway token after retries: {last_err}");
    }

    /// Drop the cached token. Called on a 401 so the next call re-fetches.
    pub async fn invalidate_token(&self) {
        *self.token.write().await = None;
    }

    /// List threads in the user's session — what the sidebar shows.
    pub async fn list_threads(&self) -> Result<ThreadListResponse> {
        let url = format!("{}/api/chat/threads", self.base_url);
        let resp = self
            .authed_request(&self.breakers.threads, |http, tok| {
                http.get(&url).bearer_auth(tok)
            })
            .await
            .context("GET /api/chat/threads")?;
        if !resp.status().is_success() {
            anyhow::bail!("threads returned {}", resp.status());
        }
        resp.json().await.context("decode thread list")
    }

    /// Fetch every turn of the named thread (or the active thread when
    /// `thread_id` is None). Used by `on_select_conversation` to rebuild
    /// the on-screen transcript when McKale clicks a different thread
    /// in the sidebar.
    pub async fn fetch_history(&self, thread_id: Option<&str>) -> Result<HistoryResponse> {
        let url = match thread_id {
            Some(id) => format!("{}/api/chat/history?thread_id={}", self.base_url, id),
            None => format!("{}/api/chat/history", self.base_url),
        };
        let resp = self
            .authed_request(&self.breakers.threads, |http, tok| {
                http.get(&url).bearer_auth(tok)
            })
            .await
            .context("GET /api/chat/history")?;
        if !resp.status().is_success() {
            anyhow::bail!("history returned {}", resp.status());
        }
        resp.json().await.context("decode history response")
    }

    /// Create a new thread and return its info.
    pub async fn new_thread(&self) -> Result<ThreadInfo> {
        let url = format!("{}/api/chat/thread/new", self.base_url);
        let resp = self
            .authed_request(&self.breakers.thread_new, |http, tok| {
                http.post(&url).bearer_auth(tok)
            })
            .await
            .context("POST /api/chat/thread/new")?;
        if !resp.status().is_success() {
            anyhow::bail!("thread/new returned {}", resp.status());
        }
        resp.json().await.context("decode thread info")
    }

    /// Send a user message and a list of attached base64-encoded image
    /// frames (no `data:` prefix). `thread_id` selects which conversation
    /// the message belongs to (None = active thread). Returns when the
    /// gateway has accepted the message; the reply arrives over SSE.
    pub async fn send_chat(
        &self,
        content: &str,
        images: &[String],
    ) -> Result<()> {
        self.send_chat_in_thread(content, images, None).await
    }

    pub async fn send_chat_in_thread(
        &self,
        content: &str,
        images: &[String],
        thread_id: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/api/chat/send", self.base_url);
        // Build body once and serialize to a value so the closure can
        // capture by reference + be called twice on 401 retry.
        let body = serde_json::to_value(SendMessageRequest {
            content,
            thread_id,
            images: if images.is_empty() { None } else { Some(images) },
        })
        .context("serialize send body")?;
        let resp = self
            .authed_request(&self.breakers.chat_send, |http, tok| {
                http.post(&url).bearer_auth(tok).json(&body)
            })
            .await
            .context("POST /api/chat/send")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("chat/send returned {}: {}", status, text);
        }
        Ok(())
    }

    /// Respond to an approval prompt by request_id. `always` true tells
    /// the gateway to auto-approve this tool for the rest of the session.
    pub async fn send_approval(
        &self,
        request_id: &str,
        approved: bool,
        always: bool,
    ) -> Result<()> {
        let url = format!("{}/api/chat/approval", self.base_url);
        let action = if approved && always {
            "always"
        } else if approved {
            "approve"
        } else {
            "deny"
        };
        let body = serde_json::json!({
            "request_id": request_id,
            "action": action,
        });
        let resp = self
            .authed_request(&self.breakers.chat_approval, |http, tok| {
                http.post(&url).bearer_auth(tok).json(&body)
            })
            .await
            .context("POST /api/chat/approval")?;
        if !resp.status().is_success() {
            anyhow::bail!("chat/approval returned {}", resp.status());
        }
        Ok(())
    }

    /// Upload a 16 kHz mono WAV blob to the STT endpoint and return the
    /// transcribed text. Empty string means the STT model returned no
    /// confident transcription (silence or noise).
    pub async fn stt(&self, wav_bytes: Vec<u8>) -> Result<String> {
        let url = format!("{}/api/voice/stt", self.base_url);
        let wav_len = wav_bytes.len();
        let wav = Arc::new(wav_bytes);
        let t0 = std::time::Instant::now();
        tracing::info!("gateway.stt POST start (wav_bytes={wav_len})");
        let resp = self
            .authed_request(&self.breakers.voice_stt, move |http, tok| {
                http.post(&url)
                    .bearer_auth(tok)
                    .header("Content-Type", "audio/wav")
                    .body((*wav).clone())
            })
            .await
            .context("POST /api/voice/stt")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // DEBUG, not WARN: this fires every 2s during the boot
            // probe while parakeet is loading its 600MB model into
            // VRAM, which can take 20+ seconds. The repeated WARN
            // messages looked like hard failures even though the
            // retry-loop was working as designed. Real failures still
            // surface via the bail! below — caller can log WARN
            // itself if it cares (e.g. during a user utterance).
            tracing::debug!(
                "gateway.stt POST {} in {:?}: {}",
                status,
                t0.elapsed(),
                body
            );
            anyhow::bail!("voice/stt returned {}: {}", status, body);
        }
        #[derive(Deserialize)]
        struct SttResp {
            text: String,
        }
        let parsed: SttResp = resp.json().await.context("decode STT response")?;
        tracing::info!(
            "gateway.stt POST 200 in {:?}: text={:?}",
            t0.elapsed(),
            parsed.text
        );
        Ok(parsed.text)
    }

    /// Stream TTS PCM chunks back from the gateway and hand each chunk
    /// to `on_chunk(sr, samples)`. The wire format matches the dashboard's
    /// `post_tts_stream`: a 4-byte LE sample-rate header followed by
    /// float32-LE PCM frames.
    pub async fn tts_stream<F>(&self, text: &str, mut on_chunk: F) -> Result<()>
    where
        F: FnMut(u32, Vec<f32>) + Send + 'static,
    {
        let url = format!("{}/api/voice/tts_stream", self.base_url);
        let body = serde_json::json!({ "text": text });
        let resp = self
            .authed_request(&self.breakers.voice_tts, |http, tok| {
                http.post(&url).bearer_auth(tok).json(&body)
            })
            .await
            .context("POST /api/voice/tts_stream")?;
        if !resp.status().is_success() {
            anyhow::bail!("voice/tts_stream returned {}", resp.status());
        }

        // Convert the reqwest byte stream into a tokio AsyncRead so we
        // can use buffered reads for the sample-rate header + frames.
        let stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
        let mut reader =
            tokio_util::io::StreamReader::new(stream);

        // First four bytes: u32 LE sample rate.
        let mut sr_buf = [0u8; 4];
        reader
            .read_exact(&mut sr_buf)
            .await
            .context("read TTS sample-rate header")?;
        let sample_rate = u32::from_le_bytes(sr_buf);

        // Then a stream of float32-LE samples until EOF. Buffer in 16 KB
        // reads (4096 samples per chunk) so the playback queue gets
        // smooth updates instead of one giant buffer at the end.
        let mut buf = vec![0u8; 16 * 1024];
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => return Err(anyhow::anyhow!("TTS stream read: {e}")),
            };
            // Merge any leftover bytes from the previous chunk that
            // didn't align to f32 boundaries.
            let mut frame = Vec::with_capacity(carry.len() + n);
            frame.extend_from_slice(&carry);
            frame.extend_from_slice(&buf[..n]);
            let full_floats = frame.len() / 4;
            let used = full_floats * 4;
            let samples: Vec<f32> = frame[..used]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            carry = frame[used..].to_vec();
            if !samples.is_empty() {
                on_chunk(sample_rate, samples);
            }
        }
        Ok(())
    }

    /// Subscribe to the SSE stream. `on_event` is called for every
    /// parsed `ChatEvent`. Returns when the stream closes (gateway
    /// restart, network error). Caller is expected to wrap this in a
    /// reconnect loop.
    pub async fn subscribe_events<F>(&self, on_event: F) -> Result<()>
    where
        F: Fn(ChatEvent) + Send + Sync + 'static,
    {
        // SSE auth: gateway accepts `?token=...` on the URL (header auth
        // isn't supported by EventSource clients). Matches the dashboard.
        let token = self.ensure_token().await?;
        let url = format!("{}/api/chat/events?token={token}", self.base_url);
        let client = eventsource_client::ClientBuilder::for_url(&url)
            .context("build SSE client")?
            .reconnect(
                eventsource_client::ReconnectOptionsBuilder::new(true)
                    .retry_initial(false)
                    .delay(Duration::from_secs(3))
                    .build(),
            )
            .build();

        let on_event = Arc::new(on_event);
        let mut stream = client.stream();
        use futures::StreamExt;
        while let Some(item) = stream.next().await {
            match item {
                Ok(SSE::Event(ev)) => {
                    // Named events carry the discriminant. Iron Clad's
                    // gateway emits `event: response`, `event: stream_chunk`,
                    // etc., with the JSON payload in `data`.
                    let event_type = ev.event_type;
                    let data = ev.data;
                    if let Some(parsed) = parse_sse(&event_type, &data) {
                        (on_event)(parsed);
                    }
                }
                Ok(SSE::Comment(_)) | Ok(SSE::Connected(_)) => {}
                Err(e) => {
                    tracing::warn!("SSE error: {e}");
                    // Let the reconnect loop in ClientBuilder handle this.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        Ok(())
    }
}

/// Translate the gateway's `event:` name + JSON body into a typed
/// `ChatEvent`. The dashboard's parse_and_emit! macro shapes the wire
/// format; we reverse it here.
fn parse_sse(event_type: &str, data: &str) -> Option<ChatEvent> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    match event_type {
        "response" => Some(ChatEvent::Response {
            content: value.get("content")?.as_str()?.to_string(),
        }),
        "stream_chunk" => Some(ChatEvent::StreamChunk {
            content: value.get("content")?.as_str()?.to_string(),
        }),
        "status" => Some(ChatEvent::Status {
            message: value.get("message")?.as_str()?.to_string(),
        }),
        "thinking" => Some(ChatEvent::Thinking {
            message: value.get("message")?.as_str()?.to_string(),
        }),
        "tool_started" => Some(ChatEvent::ToolStarted {
            name: value.get("name")?.as_str()?.to_string(),
        }),
        "tool_completed" => Some(ChatEvent::ToolCompleted {
            name: value.get("name")?.as_str()?.to_string(),
            success: value.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
        }),
        "tool_result" => Some(ChatEvent::ToolResult {
            name: value.get("name")?.as_str()?.to_string(),
            preview: value.get("preview")?.as_str()?.to_string(),
        }),
        "approval_needed" => Some(ChatEvent::ApprovalNeeded {
            request_id: value.get("request_id")?.as_str()?.to_string(),
            tool_name: value.get("tool_name")?.as_str()?.to_string(),
            description: value
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            parameters: value
                .get("parameters")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        "error" => Some(ChatEvent::Error {
            message: value.get("message")?.as_str()?.to_string(),
        }),
        "sub_agent_started" => Some(ChatEvent::SubAgentStarted {
            id: value.get("id")?.as_str()?.to_string(),
            label: value.get("label")?.as_str()?.to_string(),
            kind: value
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        }),
        "sub_agent_progress" => Some(ChatEvent::SubAgentProgress {
            id: value.get("id")?.as_str()?.to_string(),
            message: value.get("message")?.as_str()?.to_string(),
        }),
        "sub_agent_completed" => Some(ChatEvent::SubAgentCompleted {
            id: value.get("id")?.as_str()?.to_string(),
            success: value
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            summary: value
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        _ => None,
    }
}
