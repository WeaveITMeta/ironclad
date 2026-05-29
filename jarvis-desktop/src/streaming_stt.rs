//! Native WebTransport client for the parakeet-wt streaming STT
//! sidecar. Conceptual port of `dashboard/src/jarvis/streaming.rs`.
//!
//! Wire protocol (matches `voice/parakeet_wt_server.py`):
//!   - audio out: QUIC datagrams, raw float32-LE PCM @ 16 kHz, ~80 ms each
//!   - control out: unidirectional stream, line-delimited JSON
//!       {"type":"start"} / {"type":"end"} / {"type":"reset"}
//!   - events in: unidirectional stream from server, line-delimited JSON
//!       {"type":"interim","text":"..."}
//!       {"type":"final",  "text":"..."}
//!
//! The cert is self-signed by parakeet-wt at startup; the gateway
//! exposes its SHA-256 hash on `/api/voice/streaming-config` and we
//! pin to it (so Chrome's `serverCertificateHashes` mode + native
//! pinning both work off the same source of truth).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use wtransport::endpoint::ConnectOptions;
use wtransport::tls::Sha256Digest;
use wtransport::{ClientConfig, Endpoint};

/// Streaming events surfaced to the rest of the app.
#[derive(Debug, Clone)]
pub enum StreamingEvent {
    /// Partial transcript while user is still talking. UI shows as
    /// "transcribing: <text>".
    Interim(String),
    /// Final transcript on utterance end. Caller dispatches to the chat
    /// pipeline (POST /api/chat/send).
    Final(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    Interim { text: String },
    Final { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireControl {
    Start,
    End,
    Reset,
}

/// Streaming-config payload from the gateway. `url` is the WebTransport
/// endpoint (`https://localhost:4443`-style); `cert_sha256` is the
/// SHA-256 of the cert's DER, **hex**-encoded — matches the field the
/// gateway actually serializes at `src/channels/web/server.rs:393`.
/// Earlier we deserialized into `cert_hash_b64` and serde silently
/// defaulted to empty, so streaming STT silently fell back to POST on
/// every boot.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamingConfig {
    /// WebTransport URL, e.g. `https://127.0.0.1:4443`.
    pub url: String,
    /// SHA-256 cert hash, **hex** encoded. May be empty when streaming
    /// is disabled (`parakeet-wt` sidecar didn't come up); the caller
    /// should treat empty as "fall back to POST /api/voice/stt".
    #[serde(default)]
    pub cert_sha256: String,
}

/// Outbound commands the audio thread sends to the streaming task.
enum Cmd {
    /// PCM samples to forward as a datagram. 16 kHz float32-LE mono.
    Audio(Vec<f32>),
    /// User started a new utterance.
    Start,
    /// User finished the current utterance; flush + finalize.
    End,
    /// Abandon the in-flight utterance without finalizing. Used when
    /// the second-stage speech gate rejects the buffer as non-speech
    /// (music/TV/noise) — we still need to clear server-side state
    /// since we already streamed audio chunks during onset.
    Reset,
}

/// Handle the rest of the app holds. `send_audio` and lifecycle are
/// non-blocking; the actual WT IO lives on a tokio task.
#[derive(Clone)]
pub struct StreamingStt {
    cmd_tx: mpsc::Sender<Cmd>,
    /// Subscribe channel for interim + final events. Producers fan-out
    /// each event to all subscribers. We currently support a single
    /// subscriber (the mic_pipeline), but the broadcast pattern lets
    /// the UI subscribe later without surgery.
    events: Arc<Mutex<Option<mpsc::UnboundedReceiver<StreamingEvent>>>>,
    /// Set to true while the WT connection is healthy. Goes false on
    /// io_task exit (connection dropped, send error). mic_pipeline
    /// checks this and falls back to POST /api/voice/stt when the
    /// link is dead. Without it, a silently-dropped WT session would
    /// leave us thinking streaming is up and we'd lose audio forever.
    alive: Arc<AtomicBool>,
}

impl StreamingStt {
    /// Fetch the streaming config from the gateway. Returns `None` (and
    /// caller falls back to POST /api/voice/stt) if streaming isn't
    /// configured.
    pub async fn fetch_config(gateway_base: &str, token: &str) -> Result<Option<StreamingConfig>> {
        let url = format!("{}/api/voice/streaming-config", gateway_base);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        let resp = client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("streaming-config returned {}", resp.status());
        }
        let cfg: StreamingConfig = resp.json().await.context("decode streaming-config")?;
        if cfg.url.is_empty() || cfg.cert_sha256.is_empty() {
            return Ok(None);
        }
        Ok(Some(cfg))
    }

    /// Open the WT connection and spawn the IO task. Returns the handle
    /// + an event receiver that yields Interim/Final transcripts.
    pub async fn connect(cfg: StreamingConfig) -> Result<Self> {
        // Decode the hex cert hash and pin it. wtransport's ClientConfig
        // supports `with_server_certificate_hashes` for self-signed
        // server certs (same mechanism Chrome's `serverCertificateHashes`
        // uses). The gateway emits hex (not base64) — match it.
        let hash_bytes = hex_decode(&cfg.cert_sha256).context("decode cert hash")?;
        if hash_bytes.len() != 32 {
            anyhow::bail!("cert hash isn't 32 bytes; got {}", hash_bytes.len());
        }
        let digest = Sha256Digest::new(
            hash_bytes.as_slice().try_into().expect("32-byte slice"),
        );
        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes(vec![digest])
            .build();
        let endpoint = Endpoint::client(client_config).context("build WT client endpoint")?;

        tracing::info!("WT connecting to {}", cfg.url);
        let conn = endpoint
            .connect(ConnectOptions::builder(&cfg.url).build())
            .await
            .context("WT connect")?;
        tracing::info!("WT connected");

        // Bounded cmd channel: drop-oldest backpressure for audio. At
        // 16kHz / 80ms-per-chunk = ~12 chunks/sec; 256 is ~20s of
        // queued audio, more than enough for normal latency variance
        // but bounded so we never grow unbounded if the WT IO task
        // stalls. The Audio variant uses try_send + drop-on-full.
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(256);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<StreamingEvent>();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_for_task = alive.clone();

        tokio::spawn(async move {
            io_task(conn, cmd_rx, ev_tx).await;
            // IO task only returns when WT connection dies or our cmd
            // sender is dropped. Either way, no more streaming.
            alive_for_task.store(false, Ordering::Release);
            tracing::warn!("WT io_task exited; falling back to POST STT on next utterance");
        });

        Ok(Self {
            cmd_tx,
            events: Arc::new(Mutex::new(Some(ev_rx))),
            alive,
        })
    }

    /// True while the WT connection is healthy. mic_pipeline checks
    /// this before deciding to skip the POST fallback.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Take ownership of the event receiver. Returns `None` if already
    /// taken (single-consumer for now).
    pub async fn take_events(&self) -> Option<mpsc::UnboundedReceiver<StreamingEvent>> {
        self.events.lock().await.take()
    }

    pub fn start(&self) {
        tracing::info!("WT cmd → Start");
        match self.cmd_tx.try_send(Cmd::Start) {
            Ok(()) => {}
            Err(e) => tracing::warn!("WT cmd Start drop: {e}"),
        }
    }

    pub fn send_audio(&self, samples: Vec<f32>) {
        let n = samples.len();
        match self.cmd_tx.try_send(Cmd::Audio(samples)) {
            Ok(()) => tracing::debug!("WT cmd → Audio({n} samples)"),
            Err(e) => tracing::warn!("WT cmd Audio({n}) drop: {e}"),
        }
    }

    pub fn end(&self) {
        tracing::info!("WT cmd → End");
        match self.cmd_tx.try_send(Cmd::End) {
            Ok(()) => {}
            Err(e) => tracing::warn!("WT cmd End drop: {e}"),
        }
    }

    pub fn reset(&self) {
        tracing::info!("WT cmd → Reset");
        match self.cmd_tx.try_send(Cmd::Reset) {
            Ok(()) => {}
            Err(e) => tracing::warn!("WT cmd Reset drop: {e}"),
        }
    }
}

async fn io_task(
    conn: wtransport::Connection,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    ev_tx: mpsc::UnboundedSender<StreamingEvent>,
) {
    // Open the control unidirectional stream.
    let mut control_send = match conn.open_uni().await {
        Ok(opener) => match opener.await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("WT open_uni stream failed: {e}");
                return;
            }
        },
        Err(e) => {
            tracing::error!("WT open_uni request failed: {e}");
            return;
        }
    };

    // Spawn the events reader: server opens a unidirectional stream
    // back at us with line-delimited JSON.
    let conn_for_reader = conn.clone();
    let ev_tx_for_reader = ev_tx.clone();
    tokio::spawn(async move {
        events_reader(conn_for_reader, ev_tx_for_reader).await;
    });

    // Per-utterance datagram accounting so we can verify the WT
    // transport actually shipped bytes between Start and End.
    let mut datagrams_sent: u64 = 0;
    let mut bytes_sent: u64 = 0;
    // QUIC datagram max payload is bounded by path-MTU (~1200 bytes
    // typically; even loopback caps lower than people assume). The
    // original 80ms-per-datagram comment was wrong — 1280 f32 samples
    // = 5120 bytes overflows every QUIC datagram, every utterance.
    // 256 samples = 1024 bytes per datagram = ~16ms of audio @ 16kHz.
    // That fits with headroom for any QUIC overhead.
    const SAMPLES_PER_DATAGRAM: usize = 256;
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::Audio(samples) => {
                for sub in samples.chunks(SAMPLES_PER_DATAGRAM) {
                    let bytes: Vec<u8> = bytemuck::cast_slice::<f32, u8>(sub).to_vec();
                    let len = bytes.len();
                    match conn.send_datagram(bytes) {
                        Ok(()) => {
                            datagrams_sent += 1;
                            bytes_sent += len as u64;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "WT send_datagram FAILED ({len} bytes, datagram #{}): {e}",
                                datagrams_sent + 1
                            );
                        }
                    }
                }
            }
            Cmd::Start => {
                // Reset utterance counters at every start so the
                // per-utterance summary at End reflects only this
                // utterance's traffic.
                datagrams_sent = 0;
                bytes_sent = 0;
                let line = serde_json::to_string(&WireControl::Start).unwrap() + "\n";
                match control_send.write_all(line.as_bytes()).await {
                    Ok(()) => tracing::info!("WT wire: Start frame sent"),
                    Err(e) => tracing::warn!("WT control start write failed: {e}"),
                }
            }
            Cmd::End => {
                let line = serde_json::to_string(&WireControl::End).unwrap() + "\n";
                match control_send.write_all(line.as_bytes()).await {
                    Ok(()) => tracing::info!(
                        "WT wire: End frame sent — utterance shipped {} datagrams, {} bytes ({:.1} KB)",
                        datagrams_sent,
                        bytes_sent,
                        bytes_sent as f32 / 1024.0
                    ),
                    Err(e) => tracing::warn!("WT control end write failed: {e}"),
                }
            }
            Cmd::Reset => {
                let line = serde_json::to_string(&WireControl::Reset).unwrap() + "\n";
                match control_send.write_all(line.as_bytes()).await {
                    Ok(()) => tracing::info!(
                        "WT wire: Reset frame sent — abandoning utterance ({} datagrams, {} bytes were in flight)",
                        datagrams_sent,
                        bytes_sent
                    ),
                    Err(e) => tracing::warn!("WT control reset write failed: {e}"),
                }
                datagrams_sent = 0;
                bytes_sent = 0;
            }
        }
    }
    // Signal the server that we're closing cleanly.
    let _ = control_send.finish().await;
}

async fn events_reader(
    conn: wtransport::Connection,
    ev_tx: mpsc::UnboundedSender<StreamingEvent>,
) {
    // The server opens exactly one events unidirectional stream toward
    // the client per session. Wait for it, then read line by line.
    let mut recv = match conn.accept_uni().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("WT accept_uni for events failed: {e}");
            return;
        }
    };
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 4096];
    loop {
        match recv.read(&mut chunk).await {
            Ok(Some(n)) if n > 0 => {
                buf.extend_from_slice(&chunk[..n]);
                // Drain any complete lines.
                while let Some(idx) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=idx).collect();
                    let trimmed = std::str::from_utf8(&line)
                        .unwrap_or_default()
                        .trim_end_matches(|c| c == '\n' || c == '\r');
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<WireEvent>(trimmed) {
                        Ok(WireEvent::Interim { text }) => {
                            tracing::info!("WT event ← Interim({text:?})");
                            let _ = ev_tx.send(StreamingEvent::Interim(text));
                        }
                        Ok(WireEvent::Final { text }) => {
                            tracing::info!("WT event ← Final({text:?})");
                            let _ = ev_tx.send(StreamingEvent::Final(text));
                        }
                        Ok(WireEvent::Other) => {
                            tracing::debug!("WT event ← Other: {trimmed}");
                        }
                        Err(e) => tracing::warn!("WT events decode: {e} (line={trimmed})"),
                    }
                }
            }
            Ok(_) => break,
            Err(e) => {
                tracing::warn!("WT events read failed: {e}");
                break;
            }
        }
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        anyhow::bail!("hex string has odd length");
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let pair = &s[i..i + 2];
        out.push(u8::from_str_radix(pair, 16).context("invalid hex digit")?);
    }
    Ok(out)
}
