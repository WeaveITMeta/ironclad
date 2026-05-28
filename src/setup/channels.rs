//! Channel-related helpers for the onboarding API.
//!
//! Onboarding now happens entirely in the Leptos dashboard. The CLI helpers
//! (`setup_telegram`, `setup_tunnel`, `setup_http`, `setup_wasm_channel`) lived
//! here while the wizard was stdin-driven; the web wizard reuses just the
//! provider-API helpers below — token validation and secrets persistence.

use std::sync::Arc;

use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::secrets::{CreateSecretParams, FjallSecretsStore, SecretsCrypto, SecretsStore};

/// Context for saving secrets during setup.
pub struct SecretsContext {
    store: FjallSecretsStore,
    user_id: String,
}

impl SecretsContext {
    /// Create a new secrets context backed by the embedded Fjall secret store.
    pub fn new(crypto: Arc<SecretsCrypto>, user_id: &str) -> Result<Self, String> {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".ironclad");
        let _ = std::fs::create_dir_all(&dir);
        let store = FjallSecretsStore::open(&dir.join("secrets-index").to_string_lossy(), crypto)
            .map_err(|e| format!("Failed to open secrets store: {e}"))?;
        Ok(Self {
            store,
            user_id: user_id.to_string(),
        })
    }

    /// Save a secret to the store.
    pub async fn save_secret(&self, name: &str, value: &SecretString) -> Result<(), String> {
        let params = CreateSecretParams::new(name, value.expose_secret());
        self.store
            .create(&self.user_id, params)
            .await
            .map_err(|e| format!("Failed to save secret: {e}"))?;
        Ok(())
    }

    /// Check if a secret exists.
    pub async fn secret_exists(&self, name: &str) -> bool {
        self.store
            .exists(&self.user_id, name)
            .await
            .unwrap_or(false)
    }
}

/// Telegram Bot API response for getMe.
#[derive(Debug, Deserialize)]
struct TelegramGetMeResponse {
    ok: bool,
    result: Option<TelegramUser>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    username: Option<String>,
    #[allow(dead_code)]
    first_name: String,
}

/// Validate a Telegram bot token by calling the getMe endpoint.
///
/// Returns the bot's username on success.
pub async fn validate_telegram_token(token: &SecretString) -> Result<Option<String>, String> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let url = format!(
        "https://api.telegram.org/bot{}/getMe",
        token.expose_secret()
    );

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("API returned status {}", response.status()));
    }

    let body: TelegramGetMeResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if body.ok {
        Ok(body.result.and_then(|u| u.username))
    } else {
        Err("Telegram API returned error".to_string())
    }
}
