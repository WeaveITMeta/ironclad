//! Wizard API client. Wraps `/api/onboard/*` calls on Iron Clad's gateway port.
//!
//! Trunk serves the dashboard on :3000; Iron Clad exposes the wizard on :3030.
//! Cross-origin requests are allowed by the gateway's permissive CORS layer
//! during onboarding.

use gloo_net::http::Request;
use serde::{Deserialize, Serialize};

/// Default gateway port the onboard server binds to.
/// (See `setup::onboard_api::run_onboard_mode` and `main::onboard_bind_addr`.)
const GATEWAY_URL: &str = "http://127.0.0.1:3030";

fn url(path: &str) -> String {
    format!("{GATEWAY_URL}{path}")
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatusResponse {
    pub onboard_completed: bool,
    #[allow(dead_code)] // surfaced to the wizard UI in a later iteration
    pub has_anthropic_key: bool,
    #[allow(dead_code)]
    pub has_openai_key: bool,
    #[allow(dead_code)]
    pub has_secrets_master_key: bool,
}

#[derive(Debug, Serialize)]
pub struct SecurityBody {
    pub source: String,
}

#[derive(Debug, Deserialize)]
pub struct SecurityResponse {
    #[allow(dead_code)]
    pub status: String,
    #[allow(dead_code)]
    pub source: String,
    pub generated_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicBody {
    pub api_key: String,
    pub base_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ModelBody {
    pub model: String,
}

#[derive(Debug, Serialize)]
pub struct ChannelsBody {
    pub tunnel_url: Option<String>,
    pub http_enabled: bool,
    pub http_port: Option<u16>,
    pub wasm_channels: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatBody {
    pub enabled: bool,
    pub interval_minutes: Option<u64>,
    pub notify_channel: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompleteResponse {
    #[allow(dead_code)]
    pub status: String,
    #[allow(dead_code)]
    pub restart_required: bool,
}

/// GET /api/onboard/status
pub async fn fetch_status() -> Result<StatusResponse, String> {
    let resp = Request::get(&url("/api/onboard/status"))
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.ok() {
        return Err(format!("status {} from /status", resp.status()));
    }
    resp.json::<StatusResponse>()
        .await
        .map_err(|e| format!("decode error: {e}"))
}

async fn post_json<B: Serialize, R: for<'de> Deserialize<'de>>(
    path: &str,
    body: &B,
) -> Result<R, String> {
    let resp = Request::post(&url(path))
        .header("content-type", "application/json")
        .json(body)
        .map_err(|e| format!("encode error: {e}"))?
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.ok() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        return Err(format!("{path} returned {status}: {body_text}"));
    }
    resp.json::<R>()
        .await
        .map_err(|e| format!("decode error: {e}"))
}

pub async fn post_security(body: SecurityBody) -> Result<SecurityResponse, String> {
    post_json("/api/onboard/security", &body).await
}

pub async fn post_anthropic(body: AnthropicBody) -> Result<serde_json::Value, String> {
    post_json("/api/onboard/anthropic", &body).await
}

pub async fn post_model(body: ModelBody) -> Result<serde_json::Value, String> {
    post_json("/api/onboard/model", &body).await
}

pub async fn post_channels(body: ChannelsBody) -> Result<serde_json::Value, String> {
    post_json("/api/onboard/channels", &body).await
}

pub async fn post_heartbeat(body: HeartbeatBody) -> Result<serde_json::Value, String> {
    post_json("/api/onboard/heartbeat", &body).await
}

pub async fn post_complete() -> Result<CompleteResponse, String> {
    // POST with empty body.
    let resp = Request::post(&url("/api/onboard/complete"))
        .header("content-type", "application/json")
        .body("{}")
        .map_err(|e| format!("body error: {e}"))?
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.ok() {
        return Err(format!("complete returned {}", resp.status()));
    }
    resp.json::<CompleteResponse>()
        .await
        .map_err(|e| format!("decode error: {e}"))
}
