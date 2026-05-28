//! Whisper.cpp speech-to-text via the `whisper-server` daemon.
//!
//! `cargo run-jarvis` launches `whisper-server.exe` with the model loaded into
//! memory at startup. This client POSTs audio bytes to `/inference` and reads
//! the transcription back. Per-request latency lands in the low hundreds of
//! milliseconds instead of the 18s the cli-per-request shape required.
//!
//! Daemon API: <https://github.com/ggerganov/whisper.cpp/tree/master/examples/server>

use reqwest::{Client, multipart};
use serde::Deserialize;

use crate::config::VoiceConfig;
use crate::voice::VoiceError;

const PROBE_TIMEOUT_SECS: u64 = 60;

/// Convenience wrapper bound to a `VoiceConfig`.
pub struct WhisperStt<'a> {
    cfg: &'a VoiceConfig,
}

impl<'a> WhisperStt<'a> {
    pub fn new(cfg: &'a VoiceConfig) -> Self {
        Self { cfg }
    }

    /// Transcribe an audio blob. The daemon decodes wav/mp3/ogg/webm itself.
    pub async fn transcribe(&self, audio: &[u8]) -> Result<String, VoiceError> {
        let base = self.cfg.whisper_url_or_default();
        transcribe_via_server(&base, audio).await
    }
}

#[derive(Deserialize)]
struct InferenceResponse {
    text: String,
}

/// POST `audio` to `{base}/inference`, return the transcribed text.
pub async fn transcribe_via_server(base: &str, audio: &[u8]) -> Result<String, VoiceError> {
    let url = format!("{}/inference", base.trim_end_matches('/'));

    let part = multipart::Part::bytes(audio.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| VoiceError::BadOutput(format!("mime: {e}")))?;
    let form = multipart::Form::new()
        .part("file", part)
        .text("response_format", "json")
        .text("temperature", "0.0");

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
        .map_err(|e| VoiceError::BadOutput(format!("client build: {e}")))?;

    let resp = client
        .post(&url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| VoiceError::Subprocess {
            bin: "whisper-server".into(),
            code: -1,
            stderr: format!("POST {url}: {e}"),
        })?;

    if !resp.status().is_success() {
        let code = resp.status().as_u16() as i32;
        let body = resp.text().await.unwrap_or_default();
        return Err(VoiceError::Subprocess {
            bin: "whisper-server".into(),
            code,
            stderr: body,
        });
    }

    let parsed: InferenceResponse =
        resp.json().await.map_err(|e| VoiceError::BadOutput(format!("decode: {e}")))?;
    Ok(parsed.text.trim().to_string())
}
