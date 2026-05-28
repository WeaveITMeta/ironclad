//! HTTP backend for the web-based onboarding wizard.
//!
//! Runs a minimal axum server on `127.0.0.1:<port>` that exposes
//! `/api/onboard/*` endpoints. The Leptos dashboard (served by Trunk on :3000)
//! calls these endpoints to drive the wizard. Once onboarding completes, the
//! server returns `restart_required: true` and the user re-launches Iron Clad
//! to enter full mode.
//!
//! All endpoints are intentionally unauthenticated — the user cannot
//! authenticate until the wizard sets up the master key. The server only binds
//! to localhost, so this is safe for local installs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::secrets::{CreateSecretParams, FjallSecretsStore, SecretsCrypto, SecretsStore};
use crate::settings::{KeySource, Settings};

const WIZARD_USER_ID: &str = "default";

/// OpenAPI schema for the onboarding wizard. Surfaced at `/docs` via Swagger UI.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Iron Clad onboarding wizard API",
        version = "0.1.0",
        description = "Web-driven setup. The Leptos dashboard on :3000 calls these endpoints \
                       to configure Iron Clad on first run.",
    ),
    tags((name = "onboard", description = "First-run wizard endpoints")),
    paths(
        status_handler,
        security_handler,
        anthropic_handler,
        model_handler,
        channels_handler,
        heartbeat_handler,
        complete_handler,
    ),
    components(schemas(
        StatusResponse,
        OkResponse,
        SecurityRequest,
        SecurityResponse,
        AnthropicRequest,
        ModelRequest,
        ChannelsRequest,
        HeartbeatRequest,
        CompleteResponse,
    )),
)]
pub struct OnboardOpenApi;

/// Run Iron Clad in onboarding mode: only the wizard endpoints are live.
///
/// Blocks until Ctrl+C. Once the wizard posts to `/api/onboard/complete`, the
/// next call to `Settings::load().onboard_completed` will be `true`, but this
/// server keeps running until shutdown so the dashboard can show its "restart"
/// notice without the connection dropping.
pub async fn run_onboard_mode(host: &str, port: u16) -> anyhow::Result<()> {
    let state = Arc::new(OnboardState::new());

    let app = Router::new()
        .route("/api/onboard/status", get(status_handler))
        .route("/api/onboard/state", get(state_handler))
        .route("/api/onboard/security", post(security_handler))
        .route("/api/onboard/anthropic", post(anthropic_handler))
        .route("/api/onboard/model", post(model_handler))
        .route("/api/onboard/channels", post(channels_handler))
        .route("/api/onboard/heartbeat", post(heartbeat_handler))
        .route("/api/onboard/complete", post(complete_handler))
        .merge(SwaggerUi::new("/docs").url("/api-doc/openapi.json", OnboardOpenApi::openapi()))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid host:port '{host}:{port}': {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;

    tracing::info!("Onboarding wizard API listening on http://{addr}");
    println!();
    println!("==============================================================");
    println!(" Iron Clad is not configured yet.");
    println!(" Open http://127.0.0.1:3000 (or wherever Trunk serves) to set up.");
    println!(" Onboard API:  http://{addr}/api/onboard/status");
    println!(" Swagger docs: http://{addr}/docs");
    println!("==============================================================");
    println!();

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("Onboarding wizard shutting down");
        })
        .await
        .map_err(|e| anyhow::anyhow!("onboard server error: {e}"))?;

    Ok(())
}

/// Shared mutable state for the onboarding endpoints. Wraps the on-disk
/// `Settings` so concurrent requests don't clobber each other.
struct OnboardState {
    settings: Mutex<Settings>,
}

impl OnboardState {
    fn new() -> Self {
        Self {
            settings: Mutex::new(Settings::load()),
        }
    }

    async fn snapshot(&self) -> Settings {
        self.settings.lock().await.clone()
    }

    async fn update<F: FnOnce(&mut Settings)>(&self, f: F) -> Result<(), String> {
        let mut guard = self.settings.lock().await;
        f(&mut guard);
        guard.save().map_err(|e| e.to_string())
    }
}

// =============================================================================
// Response/request shapes
// =============================================================================

#[derive(Debug, Serialize, ToSchema)]
struct StatusResponse {
    onboard_completed: bool,
    has_anthropic_key: bool,
    has_openai_key: bool,
    has_secrets_master_key: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct OkResponse {
    status: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct SecurityRequest {
    /// "keychain" | "env" | "none"
    source: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct SecurityResponse {
    status: String,
    source: String,
    /// When source == "env", the generated hex key the user must export.
    generated_key: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct AnthropicRequest {
    api_key: String,
    /// Optional override of the base URL (e.g., a proxy). Defaults to the
    /// Anthropic API.
    base_url: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ModelRequest {
    model: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ChannelsRequest {
    tunnel_url: Option<String>,
    http_enabled: bool,
    http_port: Option<u16>,
    /// Names of WASM channels to enable (e.g., ["telegram"]).
    wasm_channels: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct HeartbeatRequest {
    enabled: bool,
    interval_minutes: Option<u64>,
    notify_channel: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct CompleteResponse {
    status: String,
    restart_required: bool,
}

// =============================================================================
// Handlers
// =============================================================================

#[utoipa::path(
    get,
    path = "/api/onboard/status",
    tag = "onboard",
    responses(
        (status = 200, description = "Current onboarding state", body = StatusResponse),
    ),
)]
async fn status_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
) -> Json<StatusResponse> {
    let settings = state.snapshot().await;

    // Anthropic key: either set in env, or we know we stored it during this wizard run.
    let has_anthropic_key =
        std::env::var("ANTHROPIC_API_KEY").is_ok() || dotenv_has_key("ANTHROPIC_API_KEY");
    let has_openai_key =
        std::env::var("OPENAI_API_KEY").is_ok() || dotenv_has_key("OPENAI_API_KEY");
    let has_secrets_master_key = match settings.secrets_master_key_source {
        KeySource::Keychain => crate::secrets::keychain::has_master_key(),
        KeySource::Env => std::env::var("SECRETS_MASTER_KEY").is_ok()
            || dotenv_has_key("SECRETS_MASTER_KEY"),
        KeySource::None => false,
    };

    Json(StatusResponse {
        onboard_completed: settings.onboard_completed,
        has_anthropic_key,
        has_openai_key,
        has_secrets_master_key,
    })
}

/// Returns the current `Settings` snapshot. Schema is opaque in OpenAPI (the
/// Settings type lives outside the wizard module and isn't ToSchema-annotated).
async fn state_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
) -> Json<Settings> {
    Json(state.snapshot().await)
}

#[utoipa::path(
    post,
    path = "/api/onboard/security",
    tag = "onboard",
    request_body = SecurityRequest,
    responses(
        (status = 200, description = "Master-key source stored", body = SecurityResponse),
        (status = 400, description = "Unknown source value"),
        (status = 500, description = "Keychain or disk write failed"),
    ),
)]
async fn security_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
    Json(req): Json<SecurityRequest>,
) -> Result<Json<SecurityResponse>, (StatusCode, String)> {
    match req.source.as_str() {
        "keychain" => {
            let key = crate::secrets::keychain::generate_master_key();
            crate::secrets::keychain::store_master_key(&key).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to store in keychain: {e}"),
                )
            })?;
            state
                .update(|s| s.secrets_master_key_source = KeySource::Keychain)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            Ok(Json(SecurityResponse {
                status: "ok".to_string(),
                source: "keychain".to_string(),
                generated_key: None,
            }))
        }
        "env" => {
            let key_hex = crate::secrets::keychain::generate_master_key_hex();
            write_env_var("SECRETS_MASTER_KEY", &key_hex).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to write .env: {e}"),
                )
            })?;
            state
                .update(|s| s.secrets_master_key_source = KeySource::Env)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            Ok(Json(SecurityResponse {
                status: "ok".to_string(),
                source: "env".to_string(),
                generated_key: Some(key_hex),
            }))
        }
        "none" => {
            state
                .update(|s| s.secrets_master_key_source = KeySource::None)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            Ok(Json(SecurityResponse {
                status: "ok".to_string(),
                source: "none".to_string(),
                generated_key: None,
            }))
        }
        other => Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown source '{other}', expected keychain|env|none"),
        )),
    }
}

#[utoipa::path(
    post,
    path = "/api/onboard/anthropic",
    tag = "onboard",
    request_body = AnthropicRequest,
    responses(
        (status = 200, description = "Anthropic key saved to .env", body = OkResponse),
        (status = 400, description = "Malformed key"),
        (status = 500, description = "Disk write failed"),
    ),
)]
async fn anthropic_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
    Json(req): Json<AnthropicRequest>,
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    let trimmed = req.api_key.trim().to_string();
    if !trimmed.starts_with("sk-ant-") {
        return Err((
            StatusCode::BAD_REQUEST,
            "Anthropic API keys start with sk-ant-".to_string(),
        ));
    }

    // Persist to .env so the next Iron Clad boot picks it up.
    write_env_var("ANTHROPIC_API_KEY", &trimmed)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write .env: {e}")))?;

    if let Some(base) = req.base_url.as_ref().filter(|s| !s.trim().is_empty()) {
        write_env_var("ANTHROPIC_BASE_URL", base.trim())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write .env: {e}")))?;
    }

    // Best-effort: also stash it in the secrets store if one is already
    // configured. This is opportunistic; the .env write above is the source
    // of truth on next boot.
    if let Ok(crypto) = current_crypto(&state).await {
        let _ = save_secret_best_effort(crypto, "anthropic_api_key", &trimmed).await;
    }

    Ok(Json(OkResponse {
        status: "ok".to_string(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/onboard/model",
    tag = "onboard",
    request_body = ModelRequest,
    responses(
        (status = 200, description = "Selected Claude model persisted", body = OkResponse),
        (status = 400, description = "Empty model id"),
        (status = 500, description = "Disk write failed"),
    ),
)]
async fn model_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
    Json(req): Json<ModelRequest>,
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    let model = req.model.trim().to_string();
    if model.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Model cannot be empty".to_string()));
    }
    write_env_var("ANTHROPIC_MODEL", &model)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write .env: {e}")))?;
    state
        .update(|s| s.selected_model = Some(model.clone()))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(OkResponse {
        status: "ok".to_string(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/onboard/channels",
    tag = "onboard",
    request_body = ChannelsRequest,
    responses(
        (status = 200, description = "Channel toggles saved", body = OkResponse),
        (status = 500, description = "Disk write failed"),
    ),
)]
async fn channels_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
    Json(req): Json<ChannelsRequest>,
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    state
        .update(|s| {
            s.tunnel.public_url = req
                .tunnel_url
                .as_ref()
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty());
            s.channels.http_enabled = req.http_enabled;
            s.channels.http_port = req.http_port;
            s.channels.wasm_channels = req.wasm_channels.clone();
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(OkResponse {
        status: "ok".to_string(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/onboard/heartbeat",
    tag = "onboard",
    request_body = HeartbeatRequest,
    responses(
        (status = 200, description = "Heartbeat configured", body = OkResponse),
        (status = 500, description = "Disk write failed"),
    ),
)]
async fn heartbeat_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
    Json(req): Json<HeartbeatRequest>,
) -> Result<Json<OkResponse>, (StatusCode, String)> {
    state
        .update(|s| {
            s.heartbeat.enabled = req.enabled;
            if let Some(mins) = req.interval_minutes {
                s.heartbeat.interval_secs = mins.saturating_mul(60);
            }
            s.heartbeat.notify_channel = req
                .notify_channel
                .as_ref()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty());
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(OkResponse {
        status: "ok".to_string(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/onboard/complete",
    tag = "onboard",
    responses(
        (status = 200, description = "Onboarding flag flipped; restart required", body = CompleteResponse),
        (status = 500, description = "Disk write failed"),
    ),
)]
async fn complete_handler(
    axum::extract::State(state): axum::extract::State<Arc<OnboardState>>,
) -> Result<Json<CompleteResponse>, (StatusCode, String)> {
    state
        .update(|s| s.onboard_completed = true)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(CompleteResponse {
        status: "ok".to_string(),
        restart_required: true,
    }))
}

// =============================================================================
// Helpers
// =============================================================================

/// Path to the project-local `.env` file we update during the wizard.
fn dotenv_path() -> PathBuf {
    // Prefer the .env in the Iron Clad crate dir (where Iron Clad runs from).
    // CARGO_MANIFEST_DIR points at the crate root at compile time.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env")
}

/// True if the on-disk `.env` defines `key=...` (even before we re-load envs).
fn dotenv_has_key(key: &str) -> bool {
    let path = dotenv_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
    let prefix = format!("{key}=");
    contents
        .lines()
        .any(|line| line.trim_start().starts_with(&prefix))
}

/// Write `key=value` to the project `.env`, replacing any existing line that
/// starts with `key=`. Creates the file if missing. The wizard never needs to
/// read these vars in-process — Iron Clad picks them up on its next launch via
/// `dotenvy::dotenv()` — so we don't touch `std::env`.
fn write_env_var(key: &str, value: &str) -> std::io::Result<()> {
    let path = dotenv_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("{key}=");
    let mut found = false;
    let mut out: Vec<String> = existing
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(&prefix) {
                found = true;
                format!("{key}={value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        out.push(format!("{key}={value}"));
    }
    let mut joined = out.join("\n");
    if !joined.ends_with('\n') {
        joined.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, joined)
}

/// Try to construct a `SecretsCrypto` from whatever master key source the
/// wizard has configured so far. Returns Err if the user is on `KeySource::None`
/// or the key cannot be loaded.
async fn current_crypto(state: &OnboardState) -> Result<Arc<SecretsCrypto>, String> {
    let settings = state.snapshot().await;
    let key_hex = match settings.secrets_master_key_source {
        KeySource::Keychain => crate::secrets::keychain::get_master_key()
            .map_err(|e| format!("keychain read failed: {e}"))?
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>(),
        KeySource::Env => std::env::var("SECRETS_MASTER_KEY")
            .map_err(|_| "SECRETS_MASTER_KEY not set".to_string())?,
        KeySource::None => return Err("secrets disabled".to_string()),
    };
    let crypto = SecretsCrypto::new(SecretString::from(key_hex))
        .map_err(|e| format!("crypto init failed: {e}"))?;
    Ok(Arc::new(crypto))
}

/// Best-effort write of a secret to the Fjall store. Returns Ok even on
/// failure so wizard progress isn't blocked by an optional side effect.
async fn save_secret_best_effort(
    crypto: Arc<SecretsCrypto>,
    name: &str,
    value: &str,
) -> Result<(), ()> {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ironclad");
    if std::fs::create_dir_all(&dir).is_err() {
        return Err(());
    }
    let path = dir.join("secrets-index").to_string_lossy().into_owned();
    let store = match FjallSecretsStore::open(&path, crypto) {
        Ok(s) => s,
        Err(_) => return Err(()),
    };
    let params = CreateSecretParams::new(name, value);
    if store.create(WIZARD_USER_ID, params).await.is_err() {
        return Err(());
    }
    Ok(())
}
