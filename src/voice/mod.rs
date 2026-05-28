//! Voice gateway: STT only. TTS lives in the web gateway, which proxies
//! `/api/voice/tts_stream` straight to ElevenLabs (see
//! `channels/web/server.rs::voice_tts_stream_handler`).
//!
//! STT goes through whisper.cpp via CLI shell-out for the legacy path, or
//! through the Parakeet HTTP / WebTransport sidecars for the live one.
//! `VoiceConfig` (env vars `WHISPER_PATH`, `WHISPER_MODEL`) drives the
//! local fallback.

mod stt;

pub use stt::{WhisperStt, transcribe_via_server};

/// Errors from the voice gateway.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("voice backend not configured: {0}")]
    NotConfigured(&'static str),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("subprocess {bin} exited {code}: {stderr}")]
    Subprocess {
        bin: String,
        code: i32,
        stderr: String,
    },

    #[error("could not parse subprocess output: {0}")]
    BadOutput(String),
}
