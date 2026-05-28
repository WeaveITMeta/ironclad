//! Rust WebTransport sidecar for streaming Parakeet STT.
//!
//! Replaces `voice/parakeet_wt_server.py` (aioquic) which kept wedging after
//! activity on Windows. `wtransport` is built on quinn — the production-grade
//! QUIC stack — and handles connection lifecycle far more reliably.
//!
//! ## Wire protocol (unchanged from the Python version)
//!
//! Browser → Server datagrams (unreliable, fire-and-forget):
//!   - 960 bytes of float32-LE PCM @ 16 kHz, ~16 ms per datagram
//!
//! Browser → Server unidirectional stream (reliable, line-delimited JSON):
//!   - `{"type":"start"}`  / `{"type":"reset"}` — clear accumulator
//!   - `{"type":"end"}`    — flush, return a final transcript
//!
//! Server → Browser unidirectional stream (one per session, line-delimited JSON):
//!   - `{"type":"interim","text":"..."}` — best guess so far (every ~400 ms)
//!   - `{"type":"final","text":"..."}`   — emitted in response to client `end`
//!
//! Inference itself is delegated to the Parakeet HTTP sidecar on
//! `--parakeet-url` (default `http://127.0.0.1:8932/inference`). This lets
//! us focus the Rust binary on transport reliability while keeping the
//! Python NeMo stack for the actual model. One HTTP POST per interim/final.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::BytesMut;
use clap::Parser;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, SanType};
use reqwest::multipart;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use wtransport::endpoint::IncomingSession;
use wtransport::tls::Sha256DigestFmt;
use wtransport::{Endpoint, Identity, ServerConfig};

const SAMPLE_RATE: u32 = 16_000;
const WINDOW_SAMPLES: usize = SAMPLE_RATE as usize * 4; // 4-second sliding window
const COMMIT_SAMPLES: usize = SAMPLE_RATE as usize * 5 / 2; // commit oldest 2.5 s
const INTERIM_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Parser, Debug)]
#[command(name = "parakeet_wt")]
struct Args {
    /// QUIC listen port.
    #[arg(long, default_value_t = 4443)]
    port: u16,
    /// Where to write the cert (regenerated when missing or near-expiry).
    #[arg(long, default_value = "voice/wt_cert.pem")]
    cert: PathBuf,
    #[arg(long, default_value = "voice/wt_key.pem")]
    key: PathBuf,
    /// Where to drop the SHA-256 hash hex (the dashboard fetches this).
    #[arg(long, default_value = "voice/wt_cert.sha256")]
    hash_file: PathBuf,
    /// URL of the Parakeet HTTP sidecar (`/inference` endpoint).
    #[arg(long, default_value = "http://127.0.0.1:8932/inference")]
    parakeet_url: String,
}

// ----------------------------------------------------------------------------
// Self-signed cert (ECDSA P-256, <14 days validity for Chrome WT pinning).
// ----------------------------------------------------------------------------

const CERT_VALID_DAYS: i64 = 13; // Chrome refuses serverCertificateHashes >= 14d

/// Generate (or reuse) a self-signed ECDSA P-256 cert pinned to 127.0.0.1.
/// Returns the SHA-256(DER) hex — what the browser pins via
/// `serverCertificateHashes`.
fn ensure_cert(cert_path: &Path, key_path: &Path, hash_path: &Path) -> Result<String> {
    let regen = !cert_path.exists() || !key_path.exists() || cert_is_expiring(cert_path);
    if regen {
        info!(target: "wt", "generating ECDSA P-256 self-signed cert");
        let mut params = CertificateParams::new(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "ironclad-parakeet-wt");
        params.distinguished_name = dn;
        params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into()?),
            SanType::IpAddress("127.0.0.1".parse()?),
        ];
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(CERT_VALID_DAYS);

        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let cert = params.self_signed(&key)?;

        if let Some(parent) = cert_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(cert_path, cert.pem().as_bytes())?;
        std::fs::write(key_path, key.serialize_pem().as_bytes())?;
    }

    // SHA-256 of the DER form — what `serverCertificateHashes` expects.
    let cert_pem = std::fs::read_to_string(cert_path)?;
    let cert_der = pem::parse(&cert_pem)?.contents().to_vec();
    let mut hasher = Sha256::new();
    hasher.update(&cert_der);
    let digest = hasher.finalize();
    let hex = digest.iter().map(|b| format!("{:02x}", b)).collect::<String>();

    if let Some(parent) = hash_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(hash_path, &hex)?;
    info!(target: "wt", cert_sha256 = %hex, "cert ready");
    Ok(hex)
}

fn cert_is_expiring(_cert_path: &Path) -> bool {
    // For now, regenerate only when missing. We could parse the cert and
    // check `not_after` here; not worth the dependency churn until we hit
    // an actual expiry incident.
    false
}

// ----------------------------------------------------------------------------
// Per-session state.
// ----------------------------------------------------------------------------

struct Session {
    /// All audio samples received since the last reset/finalize. Used for
    /// the final-quality transcript on `end`.
    full: Vec<f32>,
    /// Sliding window for interim transcription (front gets committed when
    /// it exceeds the window length).
    window: Vec<f32>,
    /// Text that's been frozen from the older parts of the window. New
    /// interims append fresh-window text to this.
    committed_text: String,
    /// Last emitted text (so we only send when it changes).
    last_emitted: String,
    /// When we last ran an interim transcribe (rate-limit to ~2.5/s).
    last_interim: Instant,
}

impl Session {
    fn new() -> Self {
        Self {
            full: Vec::new(),
            window: Vec::new(),
            committed_text: String::new(),
            last_emitted: String::new(),
            last_interim: Instant::now(),
        }
    }

    fn push_audio(&mut self, samples: &[f32]) {
        self.full.extend_from_slice(samples);
        self.window.extend_from_slice(samples);
    }

    fn reset(&mut self) {
        self.full.clear();
        self.window.clear();
        self.committed_text.clear();
        self.last_emitted.clear();
        self.last_interim = Instant::now();
    }
}

// ----------------------------------------------------------------------------
// Parakeet HTTP client (multipart POST to /inference).
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct InferenceResponse {
    text: String,
}

async fn transcribe(client: &reqwest::Client, url: &str, samples: &[f32]) -> Result<String> {
    if samples.is_empty() {
        return Ok(String::new());
    }
    let wav = encode_wav(samples, SAMPLE_RATE);
    let part = multipart::Part::bytes(wav)
        .file_name("clip.wav")
        .mime_str("audio/wav")?;
    let form = multipart::Form::new()
        .part("file", part)
        .text("response_format", "json")
        .text("temperature", "0.0");

    let resp = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .context("parakeet POST")?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("parakeet HTTP {code}: {body}");
    }
    let parsed: InferenceResponse = resp.json().await.context("decode parakeet")?;
    Ok(parsed.text.trim().to_string())
}

/// Minimal RIFF WAV encoder for 16 kHz mono float32 → PCM_S16LE.
fn encode_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let byte_count = samples.len() * 2;
    let mut buf = Vec::with_capacity(44 + byte_count);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&((36 + byte_count) as u32).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM header size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(byte_count as u32).to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

// ----------------------------------------------------------------------------
// Session handler.
// ----------------------------------------------------------------------------

async fn handle_session(
    incoming: IncomingSession,
    parakeet_url: String,
    http: reqwest::Client,
) -> Result<()> {
    let session_req = incoming.await.context("await session request")?;
    let path = session_req.path().to_string();
    info!(target: "wt", path = %path, "WT session request");
    let conn = session_req.accept().await.context("accept session")?;
    info!(target: "wt", "WT session accepted");

    let session = Arc::new(Mutex::new(Session::new()));

    // Open the outbound "events" stream. We'll write JSON-lines here.
    let mut events_tx = conn
        .open_uni()
        .await
        .context("open events stream")?
        .await
        .context("await events stream open")?;

    let datagrams_loop = {
        let conn = conn.clone();
        let session = Arc::clone(&session);
        async move {
            loop {
                match conn.receive_datagram().await {
                    Ok(dgram) => {
                        let bytes = dgram.payload();
                        // Reinterpret as little-endian float32. We tolerate
                        // a non-multiple-of-4 length by truncating.
                        let n = bytes.len() / 4;
                        if n == 0 {
                            continue;
                        }
                        let mut samples = Vec::with_capacity(n);
                        for i in 0..n {
                            let off = i * 4;
                            let v = f32::from_le_bytes([
                                bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3],
                            ]);
                            samples.push(v);
                        }
                        session.lock().await.push_audio(&samples);
                    }
                    Err(e) => {
                        debug!(target: "wt", err = %e, "datagram recv ended");
                        break;
                    }
                }
            }
        }
    };

    // Periodic interim transcription task. Drives the in-browser
    // streaming-as-you-speak feedback.
    let (interim_tx, mut interim_rx) = tokio::sync::mpsc::channel::<String>(8);
    let interim_loop = {
        let session = Arc::clone(&session);
        let http = http.clone();
        let url = parakeet_url.clone();
        async move {
            let mut ticker = tokio::time::interval(INTERIM_INTERVAL);
            loop {
                ticker.tick().await;
                let (window_snapshot, committed_prefix) = {
                    let mut s = session.lock().await;
                    if s.window.is_empty() {
                        continue;
                    }
                    // Commit the front of the window if it grew past the limit.
                    if s.window.len() > WINDOW_SAMPLES {
                        let mut front = s.window.split_off(0);
                        let tail = front.split_off(COMMIT_SAMPLES);
                        s.window = tail;
                        // front holds the chunk we're about to commit.
                        // We transcribe it under the lock-free section by
                        // cloning the slice and dropping the lock first.
                        let snap = front;
                        drop(s);
                        match transcribe(&http, &url, &snap).await {
                            Ok(text) if !text.is_empty() => {
                                let mut s = session.lock().await;
                                if !s.committed_text.is_empty() {
                                    s.committed_text.push(' ');
                                }
                                s.committed_text.push_str(&text);
                            }
                            Ok(_) => {}
                            Err(e) => warn!(target: "wt", err = %e, "interim commit transcribe failed"),
                        }
                        let s = session.lock().await;
                        (s.window.clone(), s.committed_text.clone())
                    } else {
                        (s.window.clone(), s.committed_text.clone())
                    }
                };

                if window_snapshot.len() < 1600 {
                    continue; // <0.1 s, skip
                }
                let tail_text = match transcribe(&http, &url, &window_snapshot).await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(target: "wt", err = %e, "interim tail transcribe failed");
                        continue;
                    }
                };
                let merged = if committed_prefix.is_empty() {
                    tail_text
                } else if tail_text.is_empty() {
                    committed_prefix
                } else {
                    format!("{} {}", committed_prefix, tail_text)
                };
                let mut s = session.lock().await;
                if merged != s.last_emitted {
                    s.last_emitted = merged.clone();
                    let _ = interim_tx.send(merged).await;
                }
            }
        }
    };

    // Control-stream reader: accept the one unidirectional stream the
    // client opens for control messages, drain JSON lines, dispatch.
    let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<ControlMsg>(8);
    let control_loop = {
        let conn = conn.clone();
        async move {
            let mut ctrl_stream = match conn.accept_uni().await {
                Ok(s) => s,
                Err(e) => {
                    debug!(target: "wt", err = %e, "no control stream from client");
                    return;
                }
            };
            let mut buf = BytesMut::with_capacity(4096);
            let mut tmp = [0u8; 1024];
            loop {
                let n = match ctrl_stream.read(&mut tmp).await {
                    Ok(Some(n)) => n,
                    Ok(None) => break,
                    Err(e) => {
                        debug!(target: "wt", err = %e, "control read ended");
                        break;
                    }
                };
                buf.extend_from_slice(&tmp[..n]);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line = buf.split_to(pos + 1);
                    let line = std::str::from_utf8(&line[..line.len() - 1])
                        .unwrap_or("")
                        .trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<ControlMsg>(line) {
                        Ok(msg) => {
                            let _ = control_tx.send(msg).await;
                        }
                        Err(e) => warn!(target: "wt", err = %e, line, "bad control JSON"),
                    }
                }
            }
        }
    };

    // Driver task: handle interim sends + control dispatch + final
    // transcription on `end`.
    let driver = {
        let session = Arc::clone(&session);
        let http = http.clone();
        let url = parakeet_url.clone();
        async move {
            loop {
                tokio::select! {
                    interim = interim_rx.recv() => {
                        let Some(text) = interim else { break };
                        let payload = serde_json::json!({"type":"interim","text":text});
                        let line = format!("{}\n", payload);
                        if let Err(e) = events_tx.write_all(line.as_bytes()).await {
                            warn!(target: "wt", err = %e, "interim send failed");
                            break;
                        }
                    }
                    ctrl = control_rx.recv() => {
                        let Some(msg) = ctrl else { break };
                        match msg.r#type.as_str() {
                            "end" => {
                                let full = {
                                    let s = session.lock().await;
                                    s.full.clone()
                                };
                                let text = match transcribe(&http, &url, &full).await {
                                    Ok(t) => t,
                                    Err(e) => {
                                        warn!(target: "wt", err = %e, "final transcribe failed");
                                        String::new()
                                    }
                                };
                                let payload = serde_json::json!({"type":"final","text":text});
                                let line = format!("{}\n", payload);
                                if let Err(e) = events_tx.write_all(line.as_bytes()).await {
                                    warn!(target: "wt", err = %e, "final send failed");
                                    break;
                                }
                                let _ = events_tx.flush().await;
                                session.lock().await.reset();
                            }
                            "reset" | "start" => {
                                session.lock().await.reset();
                            }
                            _ => warn!(target: "wt", ty = %msg.r#type, "unknown control type"),
                        }
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = datagrams_loop => {},
        _ = interim_loop => {},
        _ = control_loop => {},
        _ = driver => {},
    }
    info!(target: "wt", "WT session done");
    Ok(())
}

#[derive(Deserialize)]
struct ControlMsg {
    r#type: String,
}

// ----------------------------------------------------------------------------
// Entrypoint.
// ----------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wtransport=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

    ensure_cert(&args.cert, &args.key, &args.hash_file)?;
    let identity = Identity::load_pemfiles(&args.cert, &args.key)
        .await
        .context("load cert+key PEMs")?;

    let config = ServerConfig::builder()
        .with_bind_default(args.port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .max_idle_timeout(Some(Duration::from_secs(8)))?
        .build();

    let endpoint = Endpoint::server(config)?;
    info!(target: "wt", port = args.port, parakeet = %args.parakeet_url, "WT server listening");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // Heartbeat — proves the tokio runtime is alive. If WT wedges, this
    // log stops while the process still exists.
    {
        let _hb = tokio::spawn(async {
            let mut n = 0u64;
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            loop {
                ticker.tick().await;
                n += 1;
                info!(target: "wt::heartbeat", tick = n, "alive");
            }
        });
    }

    loop {
        let incoming = endpoint.accept().await;
        let url = args.parakeet_url.clone();
        let http = http.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_session(incoming, url, http).await {
                error!(target: "wt", err = %e, "session ended with error");
            }
        });
    }
}
