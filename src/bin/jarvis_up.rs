//! `cargo run-jarvis` — orchestrates the JARVIS backend stack.
//!
//! Spawns three children and waits for Ctrl+C:
//!   1. Playwright MCP    on :8931  (`npx @playwright/mcp@latest --port 8931`)
//!   2. whisper-server    on :8932  (loaded model, HTTP STT; optional)
//!   3. Iron Clad gateway  on :3030  (`ironclad run` with `GATEWAY_ENABLED=true`)
//!
//! Trunk (dashboard dev server on :3000) is **not** orchestrated here. On
//! Windows trunk dies with a silent exit-1 mid-rebuild under tokio supervision,
//! regardless of stdio config or process-group flags. Run it manually:
//!
//!     # second terminal
//!     cd .ironclad\dashboard
//!     trunk serve
//!
//! Output from each child is streamed inline with a `[name]` prefix. If any
//! child exits on its own, the orchestrator tears the others down too.
//!
//! Whisper-server is the only optional one: if `WHISPER_PATH` /
//! `WHISPER_MODEL` aren't set, it's skipped and `/api/voice/stt` will 502
//! when called. Everything else still works.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{Notify, oneshot};

const PLAYWRIGHT_PORT: u16 = 8931;
const WHISPER_PORT: u16 = 8932;
const GATEWAY_PORT: u16 = 3030;

fn ironclad_root() -> PathBuf {
    // Walk up from target/release/jarvis_up.exe → target/release → target → <root>.
    // Avoids baking CARGO_MANIFEST_DIR into the binary, so the project dir can
    // be renamed without rebuilding.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            return root.to_path_buf();
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Locate the `ironclad` binary sitting alongside this orchestrator in the
/// same target directory (`target/release/` or `target/debug/`). `CARGO_BIN_EXE_*`
/// is only set for integration tests, so we resolve at runtime instead.
fn ironclad_exe() -> anyhow::Result<PathBuf> {
    let me = std::env::current_exe().context("failed to read current_exe")?;
    let dir = me
        .parent()
        .ok_or_else(|| anyhow::anyhow!("current_exe has no parent dir"))?;
    let name = if cfg!(windows) {
        "ironclad.exe"
    } else {
        "ironclad"
    };
    let candidate = dir.join(name);
    if !candidate.exists() {
        anyhow::bail!(
            "ironclad binary not found at {}; run `cargo build --release --bin ironclad` first",
            candidate.display()
        );
    }
    Ok(candidate)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = ironclad_root();

    // Load .env from the crate root, not the process CWD. Cargo sometimes runs
    // the binary from a different working directory and dotenvy's default
    // `dotenv()` only walks up from CWD; we need the file at .ironclad/.env
    // regardless of where the shell is sitting when `cargo run-jarvis` fires.
    let env_path = root.join(".env");
    match dotenvy::from_path(&env_path) {
        Ok(()) => println!("[jarvis-up] loaded env from {}", env_path.display()),
        Err(e) => println!(
            "[jarvis-up] no env at {} ({e}); continuing with process env only",
            env_path.display()
        ),
    }

    let dashboard_dir = root.join("dashboard");
    if !dashboard_dir.exists() {
        anyhow::bail!(
            "dashboard directory not found at {} — is the workspace seeded?",
            dashboard_dir.display()
        );
    }

    let shutdown = Arc::new(Notify::new());

    // Playwright: one MCP server per configured Chrome profile. JARVIS sees
    // them as distinct tool namespaces (`playwright_marketing`,
    // `playwright_personal`, …) and picks based on the account it needs.
    // Falls back to a single anonymous browser on PLAYWRIGHT_PORT if no
    // profiles are configured.
    let (mut h_playwrights, mut playwright_ports) =
        spawn_playwright_profiles(shutdown.clone());

    // CDP-attach Playwright: if Chrome is running with
    // `--remote-debugging-port=9222` (set CHROME_CDP_URL to override),
    // spawn an MCP server that connects to that running instance instead
    // of launching its own. This is what gives JARVIS access to McKale's
    // real interactive tabs (background tabs included), not just
    // Playwright-spawned ones.
    let cdp_endpoint = std::env::var("CHROME_CDP_URL")
        .unwrap_or_else(|_| "http://localhost:9222".to_string());
    if probe_cdp_endpoint(&cdp_endpoint).await {
        let cdp_mcp_port: u16 = std::env::var("PLAYWRIGHT_CDP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8946);
        match spawn_playwright_cdp(cdp_mcp_port, &cdp_endpoint) {
            Ok(child) => {
                println!(
                    "[jarvis-up] starting playwright_cdp on :{cdp_mcp_port} \
                     (attached to {cdp_endpoint} — sees real interactive tabs)"
                );
                h_playwrights.push(supervise("playwright_cdp", child, shutdown.clone()));
                playwright_ports.push(cdp_mcp_port);
                write_cdp_mcp_entry(cdp_mcp_port, &cdp_endpoint);
            }
            Err(e) => {
                eprintln!("[jarvis-up] failed to start playwright_cdp: {e}");
            }
        }
    } else {
        println!(
            "[jarvis-up] no Chrome on {cdp_endpoint}; skipping playwright_cdp \
             (launch Chrome with --remote-debugging-port=9222 to enable)"
        );
    }

    println!();
    println!("[jarvis-up] >>> START TRUNK YOURSELF in another terminal:");
    println!("[jarvis-up] >>>     cd {}", dashboard_dir.display());
    println!("[jarvis-up] >>>     trunk serve");
    println!("[jarvis-up] >>> (trunk under tokio supervision exits 1 mid-rebuild on Windows;");
    println!("[jarvis-up] >>>  this is a known issue. Open http://127.0.0.1:3000 once trunk's up.)");
    println!();

    // STT sidecar. Two options, picked by env:
    //   STT_BACKEND=parakeet  -> NeMo Parakeet TDT via Python sidecar (GPU,
    //                            sub-100ms per chunk, supports true streaming
    //                            via /ws/stream).
    //   anything else / unset -> legacy whisper.cpp HTTP server (CPU-bound,
    //                            ~5s/clip on large-turbo).
    let backend = std::env::var("STT_BACKEND").unwrap_or_else(|_| "parakeet".to_string());
    let h_stt = match backend.as_str() {
        "parakeet" => match spawn_parakeet_server(WHISPER_PORT) {
            Ok(Some(child)) => {
                println!("[jarvis-up] starting parakeet sidecar on :{WHISPER_PORT}");
                Some(supervise("parakeet", child, shutdown.clone()))
            }
            Ok(None) => {
                println!(
                    "[jarvis-up] parakeet sidecar skipped (missing python or parakeet_server.py)"
                );
                None
            }
            Err(e) => {
                eprintln!("[jarvis-up] failed to start parakeet sidecar: {e}");
                None
            }
        },
        _ => match spawn_whisper_server(WHISPER_PORT) {
            Ok(Some(child)) => {
                println!("[jarvis-up] starting whisper-server on :{WHISPER_PORT}");
                Some(supervise("whisper", child, shutdown.clone()))
            }
            Ok(None) => {
                println!(
                    "[jarvis-up] whisper-server skipped (set WHISPER_PATH + WHISPER_MODEL to enable)"
                );
                None
            }
            Err(e) => {
                eprintln!("[jarvis-up] failed to start whisper-server: {e}");
                None
            }
        },
    };

    // The Playwright MCPs take a few seconds to boot (npx + node startup);
    // Iron Clad's MCP loader is unforgiving. Block here until each port
    // accepts TCP so the loader's `list_tools` requests succeed and the
    // `playwright_*` tools actually land in the registry.
    if !playwright_ports.is_empty() {
        println!(
            "[jarvis-up] waiting for Playwright MCPs on {:?} before starting Iron Clad...",
            playwright_ports
        );
        wait_for_ports(&playwright_ports, std::time::Duration::from_secs(20)).await;
    }

    println!("[jarvis-up] starting Iron Clad with gateway on :{GATEWAY_PORT}");
    let ironclad_path = ironclad_exe().context("locating ironclad binary")?;
    let ironclad = spawn_ironclad(&ironclad_path, GATEWAY_PORT, WHISPER_PORT)
        .context("failed to start Iron Clad")?;
    let h_ironclad = supervise("ironclad", ironclad, shutdown.clone());

    // WebTransport sidecar for true streaming STT (interim transcripts).
    // Runs alongside the HTTP /inference sidecar; the dashboard prefers WT
    // when /api/voice/streaming-config returns a non-empty cert hash.
    let h_wt = match spawn_parakeet_wt_server() {
        Ok(Some(child)) => {
            println!("[jarvis-up] starting parakeet-wt-server on :4443 (HTTP/3)");
            Some(supervise("parakeet-wt", child, shutdown.clone()))
        }
        Ok(None) => {
            println!("[jarvis-up] parakeet-wt-server skipped (script missing)");
            None
        }
        Err(e) => {
            eprintln!("[jarvis-up] failed to start parakeet-wt-server: {e}");
            None
        }
    };

    // TTS is ElevenLabs cloud-only. Gateway calls https://api.elevenlabs.io
    // directly from the `/api/voice/tts_stream` handler, so there's no
    // local sidecar to spawn here. ELEVENLABS_API_KEY + ELEVENLABS_VOICE_ID
    // live in .env. No fallback by design — if the cloud's down, the user
    // hears silence instead of a downgraded fake voice.

    let mut handles: Vec<Supervised> = h_playwrights;
    handles.extend(h_stt);
    handles.extend(h_wt);
    handles.push(h_ironclad);

    tokio::select! {
        res = tokio::signal::ctrl_c() => {
            if let Err(e) = res {
                eprintln!("[jarvis-up] ctrl_c handler error: {e}");
            }
            println!("[jarvis-up] Ctrl+C received, shutting down...");
        }
        _ = shutdown.notified() => {
            println!("[jarvis-up] a child exited; tearing down the rest...");
        }
    }

    for h in handles.drain(..) {
        let _ = h.kill_tx.send(());
        let _ = h.join.await;
    }

    println!("[jarvis-up] all children stopped");
    Ok(())
}

fn spawn_playwright(port: u16, user_data_dir: Option<&Path>) -> anyhow::Result<Child> {
    // Previous runs of `cargo run-jarvis` sometimes leave a node child holding
    // the port — `child.kill()` on Windows only kills cmd.exe, not its
    // npx/node descendants. Free the port before binding to avoid
    // `EADDRINUSE`.
    free_port(port);

    let mut cmd = npx_command();
    // `--yes` auto-installs without the "Ok to proceed?" prompt that hangs
    // forever under tokio's piped stdio.
    //
    // `--host 127.0.0.1` forces IPv4-only binding. Without this, Playwright
    // resolves `localhost` to `::1` (IPv6) on Windows, but both our port
    // probe and Iron Clad's reqwest client connect to `127.0.0.1` (IPv4).
    // The IPv4 socket isn't open, so all MCP connections fail with
    // "error sending request" even though Playwright happily prints
    // "Listening on http://localhost:<port>".
    //
    // `--allowed-hosts *` disables Playwright MCP's `Host` header check.
    // The check defaults to "matches the bind host" and rejects anything
    // else with `403 Forbidden - Access is only allowed at localhost:<port>`.
    // We bind to 127.0.0.1 above, but Iron Clad's reqwest URL is also
    // 127.0.0.1, so the check should match — except Playwright's
    // comparison is keyed on the literal bind STRING, and "127.0.0.1"
    // doesn't match "localhost". Disabling the check is the simplest
    // robust path; the loopback bind is the real security boundary.
    cmd.arg("--yes")
        .arg("@playwright/mcp@latest")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--allowed-hosts")
        .arg("*");
    if let Some(dir) = user_data_dir {
        // Playwright MCP forwards `--user-data-dir` to Chromium's launch
        // args, so the browser loads with that profile's cookies, saved
        // logins, and signed-in Gmail session.
        cmd.arg("--user-data-dir").arg(dir);
    }
    spawn(cmd)
}

/// Spawn a Playwright MCP server that attaches to a running Chrome
/// instance over the Chrome DevTools Protocol instead of launching its
/// own. Lets JARVIS see and drive the user's real interactive tabs —
/// background tabs included — once McKale has launched Chrome with
/// `--remote-debugging-port=<port>`.
fn spawn_playwright_cdp(mcp_port: u16, cdp_endpoint: &str) -> anyhow::Result<Child> {
    free_port(mcp_port);
    let mut cmd = npx_command();
    cmd.arg("--yes")
        .arg("@playwright/mcp@latest")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(mcp_port.to_string())
        .arg("--allowed-hosts")
        .arg("*")
        .arg("--cdp-endpoint")
        .arg(cdp_endpoint);
    spawn(cmd)
}

/// Probe whether something is actually listening at `cdp_endpoint`.
/// Used to decide whether to spawn the CDP-attach Playwright; we don't
/// want noisy "connection refused" logs every boot when Chrome isn't
/// running with the debug flag.
async fn probe_cdp_endpoint(cdp_endpoint: &str) -> bool {
    use reqwest::Client;
    let client = match Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Chrome's CDP HTTP endpoint always exposes `/json/version` even
    // before any tabs are open. Cheapest reliable liveness check.
    let url = format!("{}/json/version", cdp_endpoint.trim_end_matches('/'));
    client.get(&url).send().await.map(|r| r.status().is_success()).unwrap_or(false)
}

/// Spawn one Playwright MCP per entry in `PLAYWRIGHT_PROFILES`.
///
/// Each entry is `<label>:<gmail>`, comma-separated. We resolve the Gmail
/// to a Chrome profile directory by reading Chrome's `Local State` JSON,
/// then launch `npx @playwright/mcp` with `--user-data-dir <profile>` on a
/// dedicated port (`PLAYWRIGHT_BASE_PORT + i`, default 8941+).
///
/// Side-effect: rewrites `~/.ironclad/mcp-servers.json` with one entry per
/// successfully-spawned profile (`playwright_<label>`). Iron Clad reads
/// that file at startup and registers each as a distinct MCP tool
/// namespace, so JARVIS picks the right Gmail account by tool name.
///
/// Falls back to a single anonymous Playwright on `PLAYWRIGHT_PORT` if no
/// profiles are configured — keeps the dev loop alive when the user
/// hasn't set up Chrome profile mapping yet.
fn spawn_playwright_profiles(shutdown: Arc<Notify>) -> (Vec<Supervised>, Vec<u16>) {
    let raw = std::env::var("PLAYWRIGHT_PROFILES").unwrap_or_default();
    let entries: Vec<(String, String)> = raw
        .split(',')
        .filter_map(|seg| {
            let seg = seg.trim();
            if seg.is_empty() {
                return None;
            }
            let (label, email) = seg.split_once(':')?;
            Some((label.trim().to_string(), email.trim().to_string()))
        })
        .collect();

    if entries.is_empty() {
        println!("[jarvis-up] starting Playwright MCP on :{PLAYWRIGHT_PORT} (no profiles configured)");
        match spawn_playwright(PLAYWRIGHT_PORT, None) {
            Ok(child) => {
                write_playwright_mcp_config(&[("playwright".into(), PLAYWRIGHT_PORT, None)]);
                return (
                    vec![supervise("playwright", child, shutdown)],
                    vec![PLAYWRIGHT_PORT],
                );
            }
            Err(e) => {
                eprintln!("[jarvis-up] failed to start Playwright MCP: {e}");
                return (vec![], vec![]);
            }
        }
    }

    let base_port: u16 = std::env::var("PLAYWRIGHT_BASE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8941);

    let chrome_profiles = load_chrome_profiles().unwrap_or_else(|e| {
        eprintln!("[jarvis-up] failed to read Chrome profiles: {e}");
        HashMap::new()
    });

    let mut handles: Vec<Supervised> = Vec::new();
    let mut registered: Vec<(String, u16, Option<String>)> = Vec::new();
    for (i, (label, email)) in entries.iter().enumerate() {
        let port = base_port + i as u16;
        let email_lc = email.to_lowercase();
        let Some(profile_dir) = chrome_profiles.get(&email_lc) else {
            eprintln!(
                "[jarvis-up] playwright_{label}: no Chrome profile signed into {email}; skipping. \
                 Sign that account into Chrome once, then restart."
            );
            continue;
        };
        let server_name = format!("playwright_{label}");
        match spawn_playwright(port, Some(profile_dir)) {
            Ok(child) => {
                println!(
                    "[jarvis-up] starting {server_name} on :{port} ({email}, profile=`{}`)",
                    profile_dir
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                );
                handles.push(supervise(
                    Box::leak(server_name.clone().into_boxed_str()),
                    child,
                    shutdown.clone(),
                ));
                registered.push((server_name, port, Some(email.clone())));
            }
            Err(e) => {
                eprintln!("[jarvis-up] {server_name}: spawn failed: {e}");
            }
        }
    }

    if registered.is_empty() {
        // Every configured profile failed (no Chrome data, no Gmail signed
        // in, etc.). Don't silently skip Playwright — fall back to one
        // anonymous browser so the user at least has SOMETHING usable.
        eprintln!(
            "[jarvis-up] all configured Playwright profiles failed; \
             falling back to one anonymous browser on :{PLAYWRIGHT_PORT}"
        );
        if let Ok(child) = spawn_playwright(PLAYWRIGHT_PORT, None) {
            write_playwright_mcp_config(&[("playwright".into(), PLAYWRIGHT_PORT, None)]);
            handles.push(supervise("playwright", child, shutdown));
            return (handles, vec![PLAYWRIGHT_PORT]);
        }
        return (handles, vec![]);
    } else {
        write_playwright_mcp_config(&registered);
    }

    let ports = registered.iter().map(|(_, p, _)| *p).collect();
    (handles, ports)
}

/// Wait for each listed TCP port to accept a connection on `127.0.0.1`.
/// Iron Clad's MCP loader races Playwright's `npx` startup; without this
/// wait, the loader fires `list_tools` against ports that aren't listening
/// yet, logs a `WARN`, and the playwright_* tools never get registered.
/// Polls every 200 ms, gives up after `timeout`, and reports which ports
/// came up so a slow one can be diagnosed.
async fn wait_for_ports(ports: &[u16], timeout: std::time::Duration) {
    use std::time::Instant;
    use tokio::net::TcpStream;
    if ports.is_empty() {
        return;
    }
    let start = Instant::now();
    let mut remaining: Vec<u16> = ports.to_vec();
    while !remaining.is_empty() {
        let mut still_pending = Vec::with_capacity(remaining.len());
        for p in remaining.drain(..) {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                TcpStream::connect(format!("127.0.0.1:{p}")),
            )
            .await
            {
                Ok(Ok(_)) => {
                    println!("[jarvis-up] playwright :{p} listening");
                }
                _ => still_pending.push(p),
            }
        }
        remaining = still_pending;
        if remaining.is_empty() {
            return;
        }
        if start.elapsed() > timeout {
            eprintln!(
                "[jarvis-up] gave up waiting for ports {:?} after {:?}; \
                 Iron Clad may register fewer MCP tools than expected",
                remaining,
                start.elapsed()
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Read Chrome's profile registry and build a `gmail -> profile_dir` map.
///
/// Chrome stashes per-profile metadata in `User Data/Local State` under
/// `profile.info_cache`. Each key is the on-disk profile dir name
/// (`Default`, `Profile 1`, `Profile 2`, …); each value has `user_name`
/// (the Gmail address signed in). We invert the map so callers can look
/// up a profile by Gmail.
///
/// Comparison is case-insensitive — Gmail itself is case-insensitive, and
/// the casing in `.env` rarely matches Chrome's stored casing exactly.
fn load_chrome_profiles() -> anyhow::Result<HashMap<String, PathBuf>> {
    let user_dir: PathBuf = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(PathBuf::from)
        })
        .context("neither USERPROFILE nor HOME set; cannot find Chrome profiles")?;
    let chrome_root = user_dir
        .join("AppData")
        .join("Local")
        .join("Google")
        .join("Chrome")
        .join("User Data");
    let local_state = chrome_root.join("Local State");
    let raw = std::fs::read_to_string(&local_state)
        .with_context(|| format!("reading {}", local_state.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)?;
    let mut out = HashMap::new();
    if let Some(cache) = json
        .pointer("/profile/info_cache")
        .and_then(|v| v.as_object())
    {
        for (dir_name, info) in cache {
            let Some(user_name) = info.get("user_name").and_then(|v| v.as_str()) else {
                continue;
            };
            let profile_dir = chrome_root.join(dir_name);
            out.insert(user_name.to_lowercase(), profile_dir);
        }
    }
    Ok(out)
}

/// Rewrite `~/.ironclad/mcp-servers.json` so Iron Clad picks up the
/// Playwright instances we just spawned. Idempotent; replaces any
/// existing `playwright*` entries.
fn write_playwright_mcp_config(servers: &[(String, u16, Option<String>)]) {
    let user_dir = match std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
    {
        Ok(d) => PathBuf::from(d),
        Err(_) => return,
    };
    let path = user_dir.join(".ironclad").join("mcp-servers.json");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut entries: Vec<serde_json::Value> = servers
        .iter()
        .map(|(name, port, email)| {
            let description = match email {
                Some(e) => format!(
                    "Playwright browser automation (Chrome profile signed into {e}). Opened pages and form fills run inside that account."
                ),
                None => "Playwright browser automation (anonymous Chromium, no logged-in accounts).".to_string(),
            };
            serde_json::json!({
                "name": name,
                "url": format!("http://127.0.0.1:{port}/mcp"),
                "enabled": true,
                "description": description,
            })
        })
        .collect();

    // Eustress stdio MCP. Iron Clad spawns the binary on startup and talks
    // to it over newline-JSON-RPC on stdin/stdout. Only register if the
    // binary actually exists on disk — otherwise it just looks like a
    // misconfiguration in the logs.
    let eustress_exe = PathBuf::from(
        std::env::var("EUSTRESS_MCP_EXE").unwrap_or_else(|_| {
            r"E:\Workspace\EustressEngine\eustress\target\release\eustress-mcp.exe".to_string()
        }),
    );
    if eustress_exe.exists() {
        let mut entry = serde_json::json!({
            "name": "eustress",
            "url": "",
            "enabled": true,
            "description": "Eustress simulation engine (world-model substrate). Use for entity-level scenario simulation, kernel-law validation, and real-time interrogation of simulated worlds.",
            "command": eustress_exe.to_string_lossy(),
        });
        if let Ok(universe) = std::env::var("EUSTRESS_UNIVERSE") {
            entry["env"] = serde_json::json!({ "EUSTRESS_UNIVERSE": universe });
        } else {
            // Reasonable default per the in-repo .mcp.json.
            entry["env"] = serde_json::json!({
                "EUSTRESS_UNIVERSE": r"C:\Users\miksu\Documents\Eustress\Universe1"
            });
        }
        entries.push(entry);
    }

    let doc = serde_json::json!({
        "schema_version": 1,
        "servers": entries,
    });
    let pretty = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string());
    if let Err(e) = std::fs::write(&path, pretty) {
        eprintln!("[jarvis-up] failed to write {}: {e}", path.display());
    }
}

/// Append (or update) the `playwright_cdp` entry in
/// `~/.ironclad/mcp-servers.json` so Iron Clad's MCP loader registers
/// the CDP-attached server alongside the profile-based ones. Preserves
/// every other server already in the file (Playwright per-profile,
/// Eustress, etc.).
fn write_cdp_mcp_entry(mcp_port: u16, cdp_endpoint: &str) {
    let user_dir = match std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
    {
        Ok(d) => PathBuf::from(d),
        Err(_) => return,
    };
    let path = user_dir.join(".ironclad").join("mcp-servers.json");

    let mut doc: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "schema_version": 1, "servers": [] }));

    let Some(servers) = doc.get_mut("servers").and_then(|v| v.as_array_mut()) else {
        return;
    };
    servers.retain(|s| s.get("name").and_then(|n| n.as_str()) != Some("playwright_cdp"));
    servers.push(serde_json::json!({
        "name": "playwright_cdp",
        "url": format!("http://127.0.0.1:{mcp_port}/mcp"),
        "enabled": true,
        "description": format!(
            "Playwright attached to McKale's running Chrome via CDP at {cdp_endpoint}. \
             Sees ALL real interactive tabs (foreground and background), not just \
             Playwright-spawned ones. Use for 'what's on my screen' / multi-tab tasks; \
             use playwright_<profile>_* for fresh sessions in a specific Gmail account."
        ),
    }));

    let pretty = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string());
    if let Err(e) = std::fs::write(&path, pretty) {
        eprintln!("[jarvis-up] failed to update {}: {e}", path.display());
    }
}

/// Best-effort: kill any process listening on `port` so we can bind. On
/// Windows this calls into PowerShell's `Get-NetTCPConnection` (the cmd-line
/// `netstat | findstr` ritual is brittle and percent-escape hell). On other
/// platforms it's a no-op (orphan-port collisions don't happen the same way).
fn free_port(port: u16) {
    #[cfg(windows)]
    {
        use std::process::Command as StdCommand;
        // Free both TCP listeners and UDP endpoints. Playwright uses TCP;
        // parakeet-wt (QUIC/HTTP3) uses UDP. The TCP-only version of this
        // function used to let parakeet-wt's UDP socket survive across
        // restarts, causing `os error 10048` on the next boot which then
        // cascaded into a full sidecar teardown.
        let script = format!(
            "Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue \
               | ForEach-Object {{ Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }}; \
             Get-NetUDPEndpoint -LocalPort {port} -ErrorAction SilentlyContinue \
               | ForEach-Object {{ Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }}"
        );
        let _ = StdCommand::new("powershell")
            .arg("-NoProfile")
            .arg("-Command")
            .arg(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = port;
    }
}


fn spawn_ironclad(exe: &Path, gateway_port: u16, whisper_port: u16) -> anyhow::Result<Child> {
    // The gateway port can be held by a previous Iron Clad instance that
    // wasn't cleanly torn down. Reclaim it before the new instance binds.
    free_port(gateway_port);
    let mut cmd = Command::new(exe);
    cmd.env("GATEWAY_ENABLED", "true")
        .env("GATEWAY_PORT", gateway_port.to_string())
        .env("GATEWAY_HOST", "127.0.0.1")
        // REPL needs a real TTY for crossterm raw-mode; under the orchestrator
        // stdin is piped, which throws `Input error: Incorrect function`. The
        // browser HUD on :3000 is the user surface anyway.
        .env("CLI_ENABLED", "false")
        // Default the whisper server URL to wherever we just launched the
        // daemon so the user doesn't have to set it explicitly.
        .env(
            "WHISPER_SERVER_URL",
            format!("http://127.0.0.1:{whisper_port}"),
        )
        // Force a full backtrace on any Rust panic so the next crash
        // surfaces the offending stack instead of just an exit code.
        // Doesn't help with native SEH access violations, but catches
        // any Rust panic that the catch_unwind wrappers re-raise.
        .env("RUST_BACKTRACE", "1");
    // Force the `Run` subcommand so we don't fall into the first-run wizard
    // path if onboard_completed somehow flips back to false.
    cmd.arg("run");
    spawn(cmd)
}

/// Spawn the Parakeet Python sidecar. Reads `python` from PATH; the script
/// itself lives at `<manifest>/voice/parakeet_server.py`. Returns `Ok(None)`
/// if the script doesn't exist (so the user can opt out by deleting it).
fn spawn_parakeet_server(port: u16) -> anyhow::Result<Option<Child>> {
    free_port(port);
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = manifest.join("voice").join("parakeet_server.py");
    if !script.exists() {
        return Ok(None);
    }
    let python = std::env::var("PARAKEET_PYTHON").unwrap_or_else(|_| "python".to_string());

    let mut cmd = Command::new(&python);
    cmd.arg(&script)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--preload");
    Ok(Some(spawn(cmd)?))
}

/// Spawn the Parakeet WebTransport sidecar.
///
/// Now uses the Rust binary (`target/release/parakeet_wt.exe`) built on
/// the `wtransport` crate (quinn-based QUIC). Replaces the old aioquic
/// Python sidecar which kept wedging the event loop after Chrome reloads
/// on Windows. Production-grade transport; same wire protocol — datagram
/// audio, JSON-line control + events streams.
///
/// Falls back to the Python script only if the Rust binary isn't built.
fn spawn_parakeet_wt_server() -> anyhow::Result<Option<Child>> {
    // A leftover process from a prior `cargo run-jarvis` (or a hard kill
    // that didn't take the child down) can hold UDP:4443. Reclaim it
    // before binding so the new spawn doesn't fall over with WSAEADDRINUSE.
    free_port(4443);
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let voice_dir = manifest.join("voice");

    // Prefer the Rust binary in target/release (or target/debug as a
    // dev convenience). If neither exists, fall through to the Python
    // script for backwards compat.
    let rust_release = manifest.join("target").join("release").join("parakeet_wt.exe");
    let rust_debug = manifest.join("target").join("debug").join("parakeet_wt.exe");
    let rust_bin = if rust_release.exists() {
        Some(rust_release)
    } else if rust_debug.exists() {
        Some(rust_debug)
    } else {
        None
    };

    if let Some(bin) = rust_bin {
        let mut cmd = Command::new(&bin);
        cmd.current_dir(manifest)
            .arg("--port")
            .arg("4443")
            .arg("--cert")
            .arg("voice/wt_cert.pem")
            .arg("--key")
            .arg("voice/wt_key.pem")
            .arg("--hash-file")
            .arg("voice/wt_cert.sha256")
            .arg("--parakeet-url")
            .arg("http://127.0.0.1:8932/inference");
        return Ok(Some(spawn(cmd)?));
    }

    let script = voice_dir.join("parakeet_wt_server.py");
    if !script.exists() {
        return Ok(None);
    }
    let python = std::env::var("PARAKEET_PYTHON").unwrap_or_else(|_| "python".to_string());

    let mut cmd = Command::new(&python);
    cmd.current_dir(&voice_dir)
        .arg(&script)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg("4443")
        .arg("--cert")
        .arg("wt_cert.pem")
        .arg("--key")
        .arg("wt_key.pem")
        .arg("--hash-file")
        .arg("wt_cert.sha256");
    Ok(Some(spawn(cmd)?))
}

/// Spawn `whisper-server` if both `WHISPER_PATH` and `WHISPER_MODEL` are set.
/// Returns `Ok(None)` when not configured (no error; STT just won't work).
fn spawn_whisper_server(port: u16) -> anyhow::Result<Option<Child>> {
    // .env is loaded explicitly at main() startup; std::env::var sees it here.
    let bin = match std::env::var("WHISPER_PATH") {
        Ok(b) if !b.trim().is_empty() => b,
        _ => return Ok(None),
    };
    let model = match std::env::var("WHISPER_MODEL") {
        Ok(m) if !m.trim().is_empty() => m,
        _ => return Ok(None),
    };
    free_port(port);

    let mut cmd = Command::new(&bin);
    cmd.arg("-m")
        .arg(&model)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        // Speed + anti-hallucination knobs. Defaults are tuned for batch
        // YouTube-style transcription, where "Thank you" / "Thanks for
        // watching" hallucinations are common training-data fillers that
        // surface on quiet or short clips. For conversational use we want
        // strict gating: only return text we're confident is speech.
        //
        //   -t 12          : use more CPU threads (default is 4)
        //   -bs 1          : greedy decode, no beam search (faster)
        //   -bo 1          : single best candidate, skip ranking
        //   -nf            : no temperature fallback when a segment fails
        //   -sns           : suppress non-speech tokens (silence/breath)
        //   -nth 0.80      : raise no-speech threshold (default 0.60) so
        //                    quiet clips are returned as empty rather than
        //                    hallucinated as "Thank you"
        //   -lpt -0.4      : tighter logprob floor (default -1.0); only
        //                    accept tokens the model is fairly confident in
        //   -et 2.0        : tighter entropy threshold (default 2.4)
        //   --prompt       : neutral domain prompt — biases the decoder
        //                    away from subtitle-style fillers
        //   -l en          : pin English so we skip lang detection per call
        //   -nt            : drop timestamps (we don't display them)
        .arg("-t").arg("12")
        .arg("-bs").arg("1")
        .arg("-bo").arg("1")
        .arg("-nf")
        .arg("-sns")
        .arg("-nth").arg("0.80")
        .arg("-lpt").arg("-0.4")
        .arg("-et").arg("2.0")
        .arg("--prompt").arg("Voice command for an assistant.")
        .arg("-l").arg("en")
        .arg("-nt");
    Ok(Some(spawn(cmd)?))
}

fn spawn(mut cmd: Command) -> anyhow::Result<Child> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok(cmd.spawn()?)
}

/// Walk a process tree and kill it. Windows-only; on other platforms this is
/// a no-op since `child.kill()` already terminates the process group.
fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::process::Command as StdCommand;
        let _ = StdCommand::new("taskkill")
            .arg("/F")
            .arg("/T")
            .arg("/PID")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
}

#[cfg(windows)]
fn npx_command() -> Command {
    // `npx` on Windows is `npx.cmd`; cmd files require cmd.exe to launch.
    let mut c = Command::new("cmd");
    c.arg("/C").arg("npx");
    c
}

#[cfg(not(windows))]
fn npx_command() -> Command {
    Command::new("npx")
}

/// Handle to a supervised child. Drop the `kill_tx` (or send `()`) to request
/// shutdown; await `join` to wait for the supervisor task to fully exit.
struct Supervised {
    kill_tx: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<()>,
}

/// Wrap a `Child` in a supervisor task that pumps its output and watches for
/// either an early exit (notifies `shutdown`) or a kill request.
fn supervise(name: &'static str, mut child: Child, shutdown: Arc<Notify>) -> Supervised {
    if let Some(out) = child.stdout.take() {
        spawn_stdout_pump(name, out);
    }
    if let Some(err) = child.stderr.take() {
        spawn_stderr_pump(name, err);
    }

    let (kill_tx, kill_rx) = oneshot::channel::<()>();

    let join = tokio::spawn(async move {
        tokio::select! {
            res = child.wait() => {
                match res {
                    Ok(status) => {
                        println!("[jarvis-up] {name} exited with {status}");
                    }
                    Err(e) => {
                        eprintln!("[jarvis-up] {name} wait error: {e}");
                    }
                }
                shutdown.notify_one();
            }
            _ = kill_rx => {
                println!("[jarvis-up] killing {name}...");
                // On Windows, `child.kill()` only kills the immediate process.
                // Tools like npx spawn node descendants that survive — and
                // then hold ports like :8931 across runs. `taskkill /F /T`
                // walks the tree.
                if let Some(pid) = child.id() {
                    kill_tree(pid);
                }
                if let Err(e) = child.kill().await {
                    eprintln!("[jarvis-up] failed to kill {name}: {e}");
                }
                let _ = child.wait().await;
            }
        }
    });

    Supervised { kill_tx, join }
}

fn spawn_stdout_pump(name: &'static str, out: ChildStdout) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(out).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            println!("[{name}] {line}");
        }
    })
}

fn spawn_stderr_pump(name: &'static str, err: ChildStderr) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(err).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            eprintln!("[{name}] {line}");
        }
    })
}
