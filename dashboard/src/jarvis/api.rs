//! JARVIS HUD ↔ Iron Clad gateway HTTP client.
//!
//! The dashboard runs on :3000 (Trunk) and the gateway on :3030. CORS is
//! permissive both ways. Token bootstrap is via the public `/api/gateway/token`
//! endpoint; everything else uses the bearer token.

use gloo_net::http::Request;
use serde::{Deserialize, Serialize};

const GATEWAY_URL: &str = "http://127.0.0.1:3030";

fn url(path: &str) -> String {
    format!("{GATEWAY_URL}{path}")
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceStatusResponse {
    pub stt_ready: bool,
    pub tts_ready: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamingConfigResponse {
    pub url: String,
    pub cert_sha256: String,
}

pub async fn fetch_streaming_config() -> Result<StreamingConfigResponse, String> {
    let resp = Request::get(&url("/api/voice/streaming-config"))
        .send()
        .await
        .map_err(|e| format!("net: {e}"))?;
    if !resp.ok() {
        return Err(format!("streaming-config status {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("decode: {e}"))
}

#[derive(Debug, Serialize)]
pub struct SendBody<'a> {
    pub content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
}

pub async fn fetch_token() -> Result<String, String> {
    let resp = Request::get(&url("/api/gateway/token"))
        .send()
        .await
        .map_err(|e| format!("net: {e}"))?;
    if !resp.ok() {
        return Err(format!("token status {}", resp.status()));
    }
    let body: TokenResponse = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    Ok(body.token)
}

pub async fn fetch_voice_status() -> Result<VoiceStatusResponse, String> {
    let resp = Request::get(&url("/api/voice/status"))
        .send()
        .await
        .map_err(|e| format!("net: {e}"))?;
    if !resp.ok() {
        return Err(format!("voice/status {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("decode: {e}"))
}

pub async fn send_chat(token: &str, content: &str) -> Result<(), String> {
    send_chat_with_images(token, content, None).await
}

/// Send a chat message with optional base64 PNG screenshots attached.
/// Each entry must be raw base64 (no `data:image/png;base64,` prefix).
pub async fn send_chat_with_images(
    token: &str,
    content: &str,
    images: Option<Vec<String>>,
) -> Result<(), String> {
    let body = SendBody { content, images };
    let resp = Request::post(&url("/api/chat/send"))
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .json(&body)
        .map_err(|e| format!("encode: {e}"))?
        .send()
        .await
        .map_err(|e| format!("net: {e}"))?;
    if !resp.ok() {
        return Err(format!("send status {}", resp.status()));
    }
    Ok(())
}

/// POST an approval decision for a pending tool call. `action` is one of
/// "approve" | "always" | "deny". The gateway clears the pending approval
/// and resumes the agent loop.
pub async fn post_approval(token: &str, request_id: &str, action: &str) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Body<'a> {
        request_id: &'a str,
        action: &'a str,
    }
    let resp = Request::post(&url("/api/chat/approval"))
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .json(&Body { request_id, action })
        .map_err(|e| format!("encode: {e}"))?
        .send()
        .await
        .map_err(|e| format!("net: {e}"))?;
    if !resp.ok() {
        return Err(format!("approval status {}", resp.status()));
    }
    Ok(())
}

/// POST raw wav bytes to `/api/voice/stt` and read the transcribed text.
pub async fn post_stt(token: &str, wav: &[u8]) -> Result<String, String> {
    let resp = Request::post(&url("/api/voice/stt"))
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "audio/wav")
        .body(js_sys::Uint8Array::from(wav))
        .map_err(|e| format!("body: {e}"))?
        .send()
        .await
        .map_err(|e| format!("net: {e}"))?;
    if !resp.ok() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(format!("stt {}: {txt}", resp.status()));
    }
    #[derive(Deserialize)]
    struct R {
        text: String,
    }
    let parsed: R = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    Ok(parsed.text)
}

/// POST text to `/api/voice/tts_stream` and call `on_chunk` for each
/// raw float32-LE PCM block as it arrives. The first 4 bytes of the stream
/// carry the sample rate as little-endian u32; everything after is
/// contiguous mono samples.
///
/// First chunk lands in ~400ms (XTTS streams as it decodes) instead of the
/// ~1.9s we'd wait for `/api/voice/tts` to return the full WAV. Caller
/// queues each chunk into the AudioQueue for gapless playback as they
/// arrive — perceptually 5x faster JARVIS replies.
pub async fn post_tts_stream<F>(
    token: &str,
    text: &str,
    mut on_chunk: F,
) -> Result<(), String>
where
    F: FnMut(u32, Vec<f32>),
{
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, Request as WebRequest, RequestInit};

    let init = RequestInit::new();
    init.set_method("POST");
    let body = serde_json::json!({ "text": text }).to_string();
    init.set_body(&body.into());
    let headers = Headers::new().map_err(|e| format!("Headers: {:?}", e))?;
    headers
        .set("authorization", &format!("Bearer {token}"))
        .map_err(|e| format!("auth header: {:?}", e))?;
    headers
        .set("content-type", "application/json")
        .map_err(|e| format!("ct header: {:?}", e))?;
    init.set_headers(&headers);

    let req = WebRequest::new_with_str_and_init(&url("/api/voice/tts_stream"), &init)
        .map_err(|e| format!("Request: {:?}", e))?;
    let window = web_sys::window().ok_or("no window")?;
    let resp_js = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(|e| format!("fetch: {:?}", e))?;
    let resp: web_sys::Response = resp_js
        .dyn_into()
        .map_err(|_| "response not Response".to_string())?;
    if !resp.ok() {
        return Err(format!("tts_stream status {}", resp.status()));
    }

    let body_stream = resp.body().ok_or("response body missing")?;
    let reader_js = js_sys::Reflect::get(&body_stream, &"getReader".into())
        .map_err(|e| format!("getReader lookup: {:?}", e))?
        .dyn_into::<js_sys::Function>()
        .map_err(|e| format!("getReader not function: {:?}", e))?
        .call0(&body_stream)
        .map_err(|e| format!("getReader call: {:?}", e))?;

    let mut sample_rate: u32 = 0;
    // Accumulates leftover bytes between reads. TCP delivers arbitrary
    // chunk sizes that almost never align to the 4-byte f32 boundary;
    // without this, any 1-3 trailing bytes were silently discarded and
    // subsequent samples decoded off-by-N. Result: static. Now we carry
    // every unconsumed byte into the next read.
    let mut pending: Vec<u8> = Vec::new();

    loop {
        let read_fn = js_sys::Reflect::get(&reader_js, &"read".into())
            .map_err(|e| format!("read lookup: {:?}", e))?
            .dyn_into::<js_sys::Function>()
            .map_err(|e| format!("read not function: {:?}", e))?;
        let read_promise = read_fn
            .call0(&reader_js)
            .map_err(|e| format!("read() call: {:?}", e))?;
        let result = JsFuture::from(js_sys::Promise::from(read_promise))
            .await
            .map_err(|e| format!("read await: {:?}", e))?;
        let done = js_sys::Reflect::get(&result, &"done".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if done {
            break;
        }
        let value = js_sys::Reflect::get(&result, &"value".into())
            .map_err(|e| format!("value lookup: {:?}", e))?;
        let chunk: js_sys::Uint8Array = value
            .dyn_into()
            .map_err(|_| "chunk not Uint8Array".to_string())?;
        let mut bytes = vec![0u8; chunk.length() as usize];
        chunk.copy_to(&mut bytes);

        // Append the newly-read bytes to whatever leftover we kept from
        // the previous iteration. Everything downstream operates on the
        // combined buffer.
        pending.append(&mut bytes);

        if sample_rate == 0 {
            // First 4 bytes of the whole stream carry sample rate.
            if pending.len() < 4 {
                continue;
            }
            sample_rate = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]);
            // Drop the header from the pending buffer in place.
            pending.drain(..4);
        }

        // Emit as many whole f32 samples as we have; keep the remainder
        // (0..=3 bytes) in `pending` for the next read.
        let aligned_len = (pending.len() / 4) * 4;
        if aligned_len > 0 {
            let samples = pcm_le_to_floats(&pending[..aligned_len]);
            pending.drain(..aligned_len);
            if !samples.is_empty() {
                on_chunk(sample_rate, samples);
            }
        }
    }
    // Final flush: if the stream ended with a stray 1-3 bytes, drop them
    // (a properly-formed stream never gets here, but better silent than
    // panicked).
    Ok(())
}

fn pcm_le_to_floats(bytes: &[u8]) -> Vec<f32> {
    debug_assert!(bytes.len() % 4 == 0, "pcm_le_to_floats requires 4-byte alignment");
    let n = bytes.len() / 4;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * 4;
        out.push(f32::from_le_bytes([
            bytes[off],
            bytes[off + 1],
            bytes[off + 2],
            bytes[off + 3],
        ]));
    }
    out
}

// Deleted: post_tts (non-streaming) pointed at /api/voice/tts which
// doesn't exist on the gateway. The streaming version
// (post_tts_stream → /api/voice/tts_stream) is the only TTS endpoint.

/// SSE URL for the chat event stream (auth via query param since EventSource
/// can't set headers). The gateway issues alphanumeric tokens so no escaping
/// is required.
pub fn events_url(token: &str) -> String {
    format!("{GATEWAY_URL}/api/chat/events?token={token}")
}
