//! WebTransport bridge for the SSE event stream.
//!
//! Mirrors `/api/chat/events` (SSE), but over QUIC: each broadcast event
//! becomes one unidirectional stream carrying its JSON-serialized
//! payload. The benefit over SSE is two-fold:
//!
//!   1. No HTTP/1.1 head-of-line blocking. A slow TTS-stream chunk can't
//!      stall a fast ToolStarted update — they ride parallel streams.
//!   2. Native reconnect semantics + keepalive on the QUIC layer; we
//!      don't have to invent a heartbeat to keep the socket warm.
//!
//! The legacy SSE endpoint stays in place: the Leptos dashboard still
//! uses EventSource. The Slint native client (jarvis-desktop) prefers
//! this WT endpoint; SSE is its fallback.
//!
//! Auth: same shape as SSE. The client passes the gateway token as a
//! `?token=...` query on the connect URL; we reject the session before
//! accepting if the token doesn't match.
//!
//! Cert: self-signed on first boot, cached at
//! `~/.ironclad/wt-events.{pem,key,hash}`. Hash is also served by the
//! gateway HTTP layer at `GET /api/gateway/wt-events-hash` so
//! jarvis-desktop can self-bootstrap the pinned digest the same way it
//! self-bootstraps the auth token via `/api/gateway/token`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, SanType, PKCS_ECDSA_P256_SHA256,
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info, warn};
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::channels::web::sse::SseManager;

// WebTransport's `serverCertificateHashes` trust mechanism (the same
// one Chrome implements) requires the pinned cert to be valid for
// strictly less than 14 days. wtransport enforces this on the client:
// hashes against a longer-lived cert are silently rejected and the
// connection falls back to OS-store issuer validation, which fails
// for a self-signed cert with `UnknownIssuer (code: 304)`. The
// `parakeet_wt.rs` sidecar already learned this — match its 13-day
// budget. The cert auto-renews on the next gateway boot after expiry.
const CERT_VALID_DAYS: i64 = 13;

/// Resolve paths for the cert PEM, key PEM, and hash file. Cached under
/// `~/.ironclad/wt-events-{pem,key,hash}` so subsequent boots reuse the
/// same identity (jarvis-desktop only has to pin the hash once per
/// install, not every gateway restart).
fn cert_paths() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let home = dirs::home_dir().context("resolve $HOME for WT cert cache")?;
    let dir = home.join(".ironclad");
    std::fs::create_dir_all(&dir).context("mkdir ~/.ironclad")?;
    Ok((
        dir.join("wt-events.pem"),
        dir.join("wt-events.key"),
        dir.join("wt-events.hash"),
    ))
}

/// Ensure the cert + key exist at the cached paths, generating a fresh
/// self-signed pair on first boot. Returns the lowercase hex SHA-256 of
/// the DER cert (what `wtransport`'s `serverCertificateHashes` API on
/// the browser side wants).
fn ensure_cert(cert_path: &Path, key_path: &Path, hash_path: &Path) -> Result<String> {
    // Regenerate if the file is missing OR has been around for more
    // than 12 days. Margin of 1 day before the 13-day max so a long
    // gateway session doesn't get bitten by mid-uptime expiry.
    let needs_fresh = if !cert_path.exists() || !key_path.exists() {
        true
    } else {
        match std::fs::metadata(cert_path).and_then(|m| m.modified()) {
            Ok(t) => {
                t.elapsed()
                    .map(|d| d.as_secs() > 12 * 24 * 60 * 60)
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    };
    if needs_fresh {
        info!(
            target: "wt-events",
            "generating fresh WT events cert (<14 days for WT hash pinning)"
        );
        let mut params =
            CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "ironclad-wt-events");
        params.distinguished_name = dn;
        params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into()?),
            SanType::IpAddress("127.0.0.1".parse()?),
        ];
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(CERT_VALID_DAYS);

        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let cert = params.self_signed(&key)?;

        std::fs::write(cert_path, cert.pem().as_bytes())?;
        std::fs::write(key_path, key.serialize_pem().as_bytes())?;
    }

    let pem_text = std::fs::read_to_string(cert_path)?;
    let der = pem::parse(&pem_text)?.contents().to_vec();
    let mut hasher = Sha256::new();
    hasher.update(&der);
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    std::fs::write(hash_path, &hex)?;
    info!(target: "wt-events", cert_sha256 = %hex, "cert ready");
    Ok(hex)
}

/// Parse the auth token from a WebTransport request path like
/// `/?token=abcd1234`. Returns None if the path doesn't carry one.
fn parse_token(path: &str) -> Option<String> {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or(path);
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "token" {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Spawn the WebTransport events server. Listens on `port`, generating
/// a self-signed cert on first boot. Every accepted connection (after
/// token validation) subscribes to the SseManager's broadcast channel
/// and forwards each event as a unidirectional stream with the JSON
/// payload as the body.
///
/// Returns the hex SHA-256 of the cert so the caller can expose it via
/// the existing HTTP gateway (the dashboard / Slint client polls a
/// gateway endpoint to discover the digest).
pub async fn spawn_wt_events_server(
    port: u16,
    auth_token: String,
    sse: Arc<SseManager>,
) -> Result<String> {
    let (cert_path, key_path, hash_path) = cert_paths()?;
    let cert_hash = ensure_cert(&cert_path, &key_path, &hash_path)?;

    let identity = Identity::load_pemfiles(&cert_path, &key_path)
        .await
        .context("load WT events cert + key")?;

    let config = ServerConfig::builder()
        .with_bind_default(port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .max_idle_timeout(Some(Duration::from_secs(20)))?
        .build();

    let endpoint = Endpoint::server(config).context("bind WT events endpoint")?;
    info!(target: "wt-events", port = port, "listening");

    tokio::spawn(async move {
        loop {
            let incoming = endpoint.accept().await;
            info!(target: "wt-events", "incoming session");
            let sse = Arc::clone(&sse);
            let auth_token = auth_token.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_session(incoming, auth_token, sse).await {
                    // Bumped from debug to warn so genuine handshake
                    // failures aren't silently swallowed at the default
                    // log level. `{e:#}` includes the cause chain.
                    warn!(target: "wt-events", err = %format!("{e:#}"), "session closed with error");
                }
            });
        }
    });
    Ok(cert_hash)
}

async fn handle_session(
    incoming: wtransport::endpoint::IncomingSession,
    auth_token: String,
    sse: Arc<SseManager>,
) -> Result<()> {
    let session_req = incoming
        .await
        .context("await session request")?;

    // Auth via `Authorization: Bearer <token>` request header (preferred)
    // with fallback to `?token=<token>` URL query (legacy clients).
    // Header path avoids the wtransport URL parser sometimes stripping
    // query strings on the client side.
    let header_token = session_req
        .headers()
        .get("authorization")
        .or_else(|| session_req.headers().get("Authorization"))
        .and_then(|raw| raw.strip_prefix("Bearer ").map(|s| s.to_string()));
    let path = session_req.path().to_string();
    let query_token = parse_token(&path);
    let header_present = header_token.is_some();
    let query_present = query_token.is_some();
    let presented = header_token.or(query_token);
    let token_ok = presented.as_deref() == Some(auth_token.as_str());

    info!(
        target: "wt-events",
        path = %path,
        header = header_present,
        query = query_present,
        token_ok = token_ok,
        "session req received"
    );

    if !token_ok {
        warn!(target: "wt-events", path = %path, "rejecting WT session: bad/missing token");
        session_req.forbidden().await;
        return Ok(());
    }

    let conn = session_req
        .accept()
        .await
        .context("accept WT session")?;
    info!(target: "wt-events", "session accepted");

    let mut stream = std::pin::pin!(sse.subscribe_raw());
    while let Some(event) = stream.next().await {
        let bytes = match serde_json::to_vec(&event) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "wt-events", err = %e, "skip event: serialize failed");
                continue;
            }
        };
        // Open a unidirectional stream per event. A failure here means
        // the client went away; bail out of the subscriber loop.
        let mut send = match conn.open_uni().await {
            Ok(opening) => match opening.await {
                Ok(s) => s,
                Err(e) => {
                    info!(target: "wt-events", err = %e, "open_uni handshake failed; closing subscriber");
                    break;
                }
            },
            Err(e) => {
                info!(target: "wt-events", err = %e, "open_uni request failed; closing subscriber");
                break;
            }
        };
        if let Err(e) = send.write_all(&bytes).await {
            info!(target: "wt-events", err = %e, "write failed; closing subscriber");
            break;
        }
        if let Err(e) = send.shutdown().await {
            debug!(target: "wt-events", err = %e, "stream shutdown returned an error (usually fine)");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_finds_query() {
        assert_eq!(parse_token("/?token=abc"), Some("abc".to_string()));
        assert_eq!(parse_token("?token=abc&other=1"), Some("abc".to_string()));
        assert_eq!(parse_token("token=abc"), Some("abc".to_string()));
        assert_eq!(parse_token("/no-query-here"), None);
        assert_eq!(parse_token("?other=1"), None);
    }
}
