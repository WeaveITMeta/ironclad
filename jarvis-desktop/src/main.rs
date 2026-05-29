//! JARVIS desktop overlay — native Slint UI + cpal audio + Iron Clad
//! gateway client. Everything Phase 1-3 + telemetry + approval banner +
//! native screen capture wired into one binary.

mod agent_names;
mod audio;
mod crash_isolation;
mod gateway;
mod hotkeys;
mod icon;
mod markdown;
mod notify;
mod screen;
mod settings;
mod speech_gate;
mod splitter;
mod streaming_stt;
mod vad;
mod voice_intent;
mod window;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// parking_lot::Mutex everywhere now: no poisoning means a panic in
// one task can't permanently lock another out (with std::sync::Mutex,
// any panic-while-holding leaves the guard poisoned, so the next
// .lock().unwrap() also panics — cascading death). Phase 3 audit moved
// `splitter` and `streaming_buffer` off std::sync::Mutex; cpal audio
// path already used parking_lot since Phase 1.
use parking_lot::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use async_channel::Receiver;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tracing_subscriber::EnvFilter;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

use crate::audio::{Mic, MicFrame, Speaker};
use crate::gateway::{ChatEvent, Gateway};
use crate::vad::{downsample_to_16k, encode_wav_16khz, Vad, VadConfig};

slint::include_modules!();

/// Long-running shared state across UI, hotkey, gateway, and audio threads.
#[derive(Clone)]
struct State {
    gateway: Gateway,
    mic_muted: Arc<AtomicBool>,
    screen_share_on: Arc<AtomicBool>,
    /// Last sentence the SSE consumer is streaming; cleared on Response.
    streaming_buffer: Arc<Mutex<String>>,
    /// Splitter — accumulates streamed chunks and emits complete
    /// sentences when punctuation+whitespace lands. Drives sentence-level
    /// TTS playback.
    splitter: Arc<Mutex<SentenceSplitter>>,
    /// Snapshot of telemetry status flags. Refreshed periodically.
    telemetry: Arc<TelemetryState>,
    /// While true, the next assistant Response/StreamChunk replaces the
    /// last transcript row instead of appending. Lets streaming chunks
    /// build a single growing jarvis bubble.
    streaming_active: Arc<AtomicBool>,
    /// Currently-selected conversation thread_id (None = active thread
    /// on the gateway). Updated by the sidebar's "select conversation"
    /// callback; read by every /api/chat/send call.
    current_thread_id: Arc<parking_lot::Mutex<Option<String>>>,
    /// Channel into the TTS worker task. Sentences land here; the
    /// worker drains them one at a time, awaiting each tts_stream +
    /// inter-sentence gap before pulling the next. Replaces the prior
    /// `tokio::spawn` per sentence architecture that ran N TTS tasks
    /// in parallel and overflowed the playback queue with ~30 seconds
    /// of audio per streaming response.
    tts_tx: tokio::sync::mpsc::UnboundedSender<splitter::Sentence>,
    /// The currently-pending tool approval (set when SSE delivers
    /// ApprovalNeeded; cleared when the user approves/denies via click
    /// OR voice). Background tasks read this to decide whether an STT
    /// final is a normal chat turn or a yes/no answer.
    pending_approval: Arc<parking_lot::Mutex<Option<PendingApproval>>>,
    /// WebTransport streaming-STT handle. Starts None; the
    /// voice_startup task fills it in once parakeet-wt reports ready
    /// and a connection succeeds. mic_pipeline reads this at each
    /// utterance boundary so the WT path can be brought online AFTER
    /// the binary already launched (avoids the "voice doesn't work
    /// because the app started before the sidecar" failure mode).
    streaming: Arc<parking_lot::Mutex<Option<streaming_stt::StreamingStt>>>,
    /// True once the voice pipeline has been confirmed ready (parakeet
    /// said stt_ready=true + we either connected WT or accepted POST
    /// fallback). Until then the mic stays muted and the HUD shows a
    /// "warming up" status.
    voice_ready: Arc<AtomicBool>,
    /// Wake-word-only mode. Entered when McKale says "mute" / "go to
    /// sleep" / etc. Mic stays hot, STT still runs, but every utterance
    /// is dropped UNLESS it starts with "Jarvis" (which clears the flag
    /// and optionally treats the trailing text as the next chat turn).
    /// Distinct from `mic_muted`, which fully disables the mic.
    wake_word_only: Arc<AtomicBool>,
    /// Set true by the voice-Stop intent. The tts_worker checks this
    /// before pulling the next sentence and drains the channel (dropping
    /// any in-flight reply) instead of resuming playback. Cleared the
    /// moment the user issues a new chat turn so the next response
    /// flows normally. Without this, halting the speaker queue only
    /// kills the current sentence; the next one from the SSE stream
    /// immediately resumes via `speaker_queue.resume()` in the worker.
    stop_requested: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
struct PendingApproval {
    request_id: String,
    #[allow(dead_code)] // surfaced in logs only for now
    tool_name: String,
}

// The transcript Model is not Send (VecModel<T> uses UnsafeCell). We
// stash a reference in a thread_local that lives on the UI thread; all
// cross-thread mutations route through `slint::invoke_from_event_loop`,
// and inside that closure we reach into this thread_local.
thread_local! {
    static TRANSCRIPT_MODEL: std::cell::RefCell<Option<std::rc::Rc<VecModel<TranscriptEntry>>>> =
        std::cell::RefCell::new(None);
    static CONVERSATIONS_MODEL: std::cell::RefCell<Option<std::rc::Rc<VecModel<ConversationRow>>>> =
        std::cell::RefCell::new(None);
    static SUB_AGENTS_MODEL: std::cell::RefCell<Option<std::rc::Rc<VecModel<SubAgentRow>>>> =
        std::cell::RefCell::new(None);
}

fn with_transcript<R>(f: impl FnOnce(&VecModel<TranscriptEntry>) -> R) -> Option<R> {
    TRANSCRIPT_MODEL.with(|cell| {
        cell.borrow().as_ref().map(|m| f(m))
    })
}

fn with_conversations<R>(f: impl FnOnce(&VecModel<ConversationRow>) -> R) -> Option<R> {
    CONVERSATIONS_MODEL.with(|cell| {
        cell.borrow().as_ref().map(|m| f(m))
    })
}

fn with_sub_agents<R>(f: impl FnOnce(&VecModel<SubAgentRow>) -> R) -> Option<R> {
    SUB_AGENTS_MODEL.with(|cell| {
        cell.borrow().as_ref().map(|m| f(m))
    })
}

struct TelemetryState {
    gateway_up: AtomicBool,
    parakeet_up: AtomicBool,
    elevenlabs_up: AtomicBool,
    claude_up: AtomicBool,
    /// Unix-ms timestamp of the last successful gateway probe. UI uses
    /// this to surface a "stale" state (yellow orb) when the probe has
    /// been timing out for > 15s — different from "down" (immediate
    /// red after first failure) because intermittent network blips
    /// should look distinct from a confirmed outage.
    last_success_ms: std::sync::atomic::AtomicU64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Install the global panic hook BEFORE anything else can panic. The
    // hook writes a JSON crash report under ~/.ironclad/crashes/ so any
    // panic (UI, audio, network) leaves a forensic trail even if the
    // process dies. catch_unwind around cpal callbacks lets the audio
    // thread survive panics in VAD/splitter code without aborting.
    crash_isolation::install_panic_hook();

    tracing::info!("🤖 jarvis-desktop starting");

    let gateway_url = std::env::var("JARVIS_GATEWAY_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3030".to_string());

    // TTS worker channel — sentences land here; one worker task drains
    // them one at a time so the playback queue isn't flooded by N
    // parallel tts_stream tasks during a streaming response.
    let tts_tx_holder = tokio::sync::mpsc::unbounded_channel::<splitter::Sentence>();

    let state = State {
        gateway: Gateway::new(&gateway_url),
        mic_muted: Arc::new(AtomicBool::new(false)),
        screen_share_on: Arc::new(AtomicBool::new(false)),
        streaming_buffer: Arc::new(Mutex::new(String::new())),
        splitter: Arc::new(Mutex::new(SentenceSplitter::new())),
        telemetry: Arc::new(TelemetryState {
            gateway_up: AtomicBool::new(false),
            parakeet_up: AtomicBool::new(false),
            elevenlabs_up: AtomicBool::new(false),
            claude_up: AtomicBool::new(false),
            last_success_ms: std::sync::atomic::AtomicU64::new(0),
        }),
        streaming_active: Arc::new(AtomicBool::new(false)),
        current_thread_id: Arc::new(parking_lot::Mutex::new(None)),
        pending_approval: Arc::new(parking_lot::Mutex::new(None)),
        streaming: Arc::new(parking_lot::Mutex::new(None)),
        voice_ready: Arc::new(AtomicBool::new(false)),
        wake_word_only: Arc::new(AtomicBool::new(false)),
        stop_requested: Arc::new(AtomicBool::new(false)),
        tts_tx: tts_tx_holder.0.clone(),
    };
    let tts_rx_for_worker = tts_tx_holder.1;

    // Initialize the thread_local transcript model on this (UI) thread.
    let transcript_model: std::rc::Rc<VecModel<TranscriptEntry>> =
        std::rc::Rc::new(VecModel::<TranscriptEntry>::default());
    TRANSCRIPT_MODEL.with(|cell| *cell.borrow_mut() = Some(transcript_model.clone()));
    let conversations_model: std::rc::Rc<VecModel<ConversationRow>> =
        std::rc::Rc::new(VecModel::<ConversationRow>::default());
    CONVERSATIONS_MODEL.with(|cell| *cell.borrow_mut() = Some(conversations_model.clone()));
    let sub_agents_model: std::rc::Rc<VecModel<SubAgentRow>> =
        std::rc::Rc::new(VecModel::<SubAgentRow>::default());
    SUB_AGENTS_MODEL.with(|cell| *cell.borrow_mut() = Some(sub_agents_model.clone()));

    // `state.current_thread_id` is shared across all the send paths —
    // alias here for readability in this scope.
    let current_thread_id = state.current_thread_id.clone();

    // Build tokio runtime up front; cpal threads + gateway client live on it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("jarvis-desktop-rt")
        .build()
        .context("tokio runtime")?;
    let runtime = Arc::new(runtime);

    let ui = MainWindow::new().context("create main window")?;
    // Windows 11 applies rounded corners by default — McKale wants
    // square edges so the cyan glow on the frame reads as a single
    // straight band. No-op on Win10 / Linux / macOS.
    window::disable_corner_rounding(&ui);
    // Icon shows in the OS title bar / Alt-Tab thumbnail.
    ui.set_window_icon(build_window_icon_image());
    // Larger icon is the arc-reactor centerpiece in both floating + HUD.
    ui.set_reactor_image(build_reactor_image());
    // SVG icons for gear / minify / monitor — rasterized once at
    // startup from the assets/icons/*.svg files and tinted at draw
    // time by Slint's `colorize` so each callsite can theme them.
    ui.set_gear_icon(icon::build_gear_icon());
    ui.set_minify_icon(icon::build_minify_icon());
    ui.set_monitor_icon(icon::build_monitor_icon());
    ui.set_status("ready".into());
    ui.set_version("v1.4".into());
    ui.set_mic_active(true);
    ui.set_pulse(0.0);
    // Bind the transcript Model so the UI's `for entry in transcript`
    // sees Rust-side pushes immediately.
    ui.set_transcript(ModelRc::from(transcript_model));
    ui.set_conversations(ModelRc::from(conversations_model));
    ui.set_sub_agents(ModelRc::from(sub_agents_model));

    // Seed initial settings from env (matching what jarvis_up reads).
    let anthropic_key_env = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    ui.set_anthropic_key(anthropic_key_env.clone().into());
    ui.set_model(std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_string()).into());
    ui.set_elevenlabs_key(std::env::var("ELEVENLABS_API_KEY").unwrap_or_default().into());
    ui.set_elevenlabs_voice_id(std::env::var("ELEVENLABS_VOICE_ID").unwrap_or_default().into());
    ui.set_heartbeat_enabled(
        std::env::var("HEARTBEAT_ENABLED").as_deref() != Ok("false"),
    );
    ui.set_autonomous_enabled(
        std::env::var("AUTONOMOUS_LOOP_ENABLED").as_deref() == Ok("true"),
    );

    // First-run wizard auto-open: if no Anthropic key is in the
    // environment, JARVIS can't do anything useful — pop the wizard
    // immediately so the user sees the setup flow on first launch.
    // McKale can dismiss with CANCEL if he wants to use the HUD
    // anyway (e.g. testing with a partial config).
    if anthropic_key_env.trim().is_empty() {
        ui.set_wizard_open(true);
        ui.set_expanded(true);
        tracing::info!("no ANTHROPIC_API_KEY in env — auto-opening setup wizard");
    }

    // ----- audio: speaker first, then mic. The Speaker exposes a
    //   PlaybackMonitor side-channel (atomic RMS of recent output) that
    //   we pass to Mic::open so the input DSP can suppress its own TTS
    //   bleed-through (reference-aware echo gating — coarse but cheap
    //   stand-in for proper WebRTC AEC3).
    let speaker = Speaker::open().context("open speaker")?;
    let speaker_queue = speaker.queue.clone();
    let playback_monitor = speaker.monitor.clone();
    // Lifetime: speaker must outlive run loop.
    let _speaker_keep = speaker;

    let (mic_tx, mic_rx) = async_channel::unbounded::<MicFrame>();
    let _mic = Mic::open(mic_tx, Some(playback_monitor.clone())).context("open mic")?;

    // Mic starts muted — voice_startup unmutes once parakeet reports
    // ready. Without this, the VAD would happily ship audio at the
    // POST /api/voice/stt endpoint before parakeet has loaded its
    // model, and every utterance would 500 for the first 5-10 seconds
    // of the user's session.
    state.mic_muted.store(true, Ordering::Release);
    ui.set_mic_active(false);
    // First-launch parakeet model load takes ~25s on a typical GPU
    // (NeMo import, weights download / mmap, CUDA kernel warmup).
    // Telling McKale specifically what to expect — "type while you
    // wait" — turns 25 seconds of vague status into 25 seconds of
    // optional productive work.
    ui.set_status("starting up · TYPE TO CHAT WHILE VOICE LOADS ↓".into());

    // mic_pipeline reads the WT streaming handle from State.streaming
    // on each utterance boundary — no need to thread it through here.
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let ui_weak = ui.as_weak();
        let speaker_queue_for_mic = speaker_queue.clone();
        let playback_monitor_for_mic = playback_monitor.clone();
        runtime.spawn(mic_pipeline(
            mic_rx,
            state,
            ui_weak,
            speaker_queue_for_mic,
            playback_monitor_for_mic,
        ));
    }

    // Voice startup: poll /api/voice/status until parakeet reports
    // stt_ready=true, then attempt the WT streaming-STT connect, then
    // unmute. This replaces the eager block_on attempt at boot, which
    // failed cleanly when parakeet was still loading its model and
    // left voice non-functional until the user restarted.
    {
        let gateway = state.gateway.clone();
        let state_for_startup = state.clone();
        let ui_weak = ui.as_weak();
        let speaker_queue_for_startup = speaker_queue.clone();
        let runtime_for_startup = runtime.clone();
        runtime.spawn(async move {
            voice_startup(
                gateway,
                state_for_startup,
                ui_weak,
                speaker_queue_for_startup,
                runtime_for_startup,
            )
            .await;
        });
    }

    // ----- spawn SSE consumer
    {
        let gateway = state.gateway.clone();
        let ui_weak = ui.as_weak();
        let state_for_sse = state.clone();
        let speaker_queue_for_sse = speaker_queue.clone();
        runtime.spawn(async move {
            sse_consumer(gateway, ui_weak, state_for_sse, speaker_queue_for_sse).await;
        });
    }

    // ----- spawn TTS worker (serialized one-sentence-at-a-time playback)
    {
        let gateway_for_tts = state.gateway.clone();
        let speaker_queue_for_tts = speaker_queue.clone();
        let stop_requested_for_tts = state.stop_requested.clone();
        runtime.spawn(async move {
            tts_worker(
                tts_rx_for_worker,
                speaker_queue_for_tts,
                gateway_for_tts,
                stop_requested_for_tts,
            )
            .await;
        });
    }

    // ----- spawn telemetry poll (status orbs)
    {
        let runtime_ref = runtime.clone();
        let telemetry = state.telemetry.clone();
        let gateway_url = gateway_url.clone();
        let ui_weak = ui.as_weak();
        runtime_ref.spawn(async move {
            telemetry_poll(gateway_url, telemetry, ui_weak).await;
        });
    }

    // ----- UI callbacks
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_expand(move || {
            if let Some(u) = ui_weak.upgrade() {
                let now_expanded = !u.get_expanded();
                u.set_expanded(now_expanded);
                // Slint's `preferred-width` is only consulted on initial
                // window construction — toggling `expanded` updates the
                // property but doesn't actually resize the OS window. We
                // have to push a new size to the window handle so the
                // OS chrome shrinks down to the floating widget OR grows
                // back to the full HUD.
                let new_size = if now_expanded {
                    slint::PhysicalSize::new(960, 780)
                } else {
                    slint::PhysicalSize::new(196, 232)
                };
                u.window().set_size(new_size);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let mic_muted = state.mic_muted.clone();
        ui.on_toggle_mic(move || {
            let next = !mic_muted.load(Ordering::Relaxed);
            mic_muted.store(next, Ordering::Relaxed);
            if let Some(u) = ui_weak.upgrade() {
                u.set_mic_active(!next);
                u.set_status(if next { "muted".into() } else { "listening".into() });
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let share = state.screen_share_on.clone();
        ui.on_toggle_screen_share(move || {
            let next = !share.load(Ordering::Relaxed);
            share.store(next, Ordering::Relaxed);
            if let Some(u) = ui_weak.upgrade() {
                u.set_screen_share_active(next);
                u.set_status(
                    if next {
                        "screen sharing — JARVIS sees the foreground window each turn".into()
                    } else {
                        "screen sharing off".into()
                    },
                );
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_start_drag(move || {
            if let Some(u) = ui_weak.upgrade() {
                start_window_drag(&u);
            }
        });
    }
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        let ui_weak = ui.as_weak();
        let current_thread = current_thread_id.clone();
        let stop_requested_for_text = state.stop_requested.clone();
        ui.on_send_text(move |s| {
            // Any user-initiated chat send clears the stop flag so the
            // tts_worker resumes playing the new response normally.
            stop_requested_for_text.store(false, Ordering::Release);
            let gateway = gateway.clone();
            let text = s.to_string();
            push_user_block(&text);
            if let Some(u) = ui_weak.upgrade() {
                u.set_status("thinking...".into());
            }
            let thread = current_thread.lock().clone();
            runtime.spawn(async move {
                if let Err(e) = gateway
                    .send_chat_in_thread(&text, &[], thread.as_deref())
                    .await
                {
                    tracing::error!("send_chat: {e}");
                }
            });
        });
    }
    ui.on_clear_transcript(|| {
        clear_transcript_rows();
    });

    // Sidebar settings panel toggle.
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_settings(move || {
            if let Some(u) = ui_weak.upgrade() {
                u.set_settings_open(!u.get_settings_open());
            }
        });
    }
    // Save settings: write to .ironclad/.env (additive), surface status.
    {
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        ui.on_save_settings(move || {
            let Some(u) = ui_weak.upgrade() else { return; };
            let anthropic = u.get_anthropic_key().to_string();
            let model = u.get_model().to_string();
            let el_key = u.get_elevenlabs_key().to_string();
            let el_voice = u.get_elevenlabs_voice_id().to_string();
            let heartbeat = u.get_heartbeat_enabled();
            let autonomous = u.get_autonomous_enabled();
            u.set_settings_saving(true);
            u.set_settings_status("writing .env...".into());
            let ui_weak2 = ui_weak.clone();
            runtime.spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    write_settings_to_env(WriteSettings {
                        anthropic_key: anthropic,
                        model,
                        elevenlabs_key: el_key,
                        elevenlabs_voice_id: el_voice,
                        heartbeat,
                        autonomous,
                    })
                })
                .await;
                let status = match res {
                    Ok(Ok(path)) => format!(
                        "saved to {}. Restart `cargo run-jarvis` to apply.",
                        path.display()
                    ),
                    Ok(Err(e)) => format!("save failed: {e}"),
                    Err(e) => format!("save join failed: {e}"),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(u) = ui_weak2.upgrade() {
                        u.set_settings_saving(false);
                        u.set_settings_status(status.into());
                    }
                });
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_heartbeat(move || {
            if let Some(u) = ui_weak.upgrade() {
                u.set_heartbeat_enabled(!u.get_heartbeat_enabled());
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_autonomous(move || {
            if let Some(u) = ui_weak.upgrade() {
                u.set_autonomous_enabled(!u.get_autonomous_enabled());
            }
        });
    }
    // Conversation select / new.
    {
        let current_thread = current_thread_id.clone();
        let gateway_for_select = state.gateway.clone();
        let runtime_for_select = runtime.clone();
        ui.on_select_conversation(move |id| {
            let id = id.to_string();
            *current_thread.lock() = Some(id.clone());
            // Reflect selection in the model.
            let _ = with_conversations(|m| {
                for i in 0..m.row_count() {
                    let mut row = m.row_data(i).unwrap_or_default();
                    let was_active = row.is_active;
                    row.is_active = row.id == id;
                    if was_active != row.is_active {
                        m.set_row_data(i, row);
                    }
                }
            });
            // Clear the on-screen transcript and rebuild it from the
            // selected thread's history. Without this, switching to a
            // prior thread shows an empty chat panel even though the
            // gateway side has all the turns recorded.
            clear_transcript_rows();
            let gateway = gateway_for_select.clone();
            let id_for_fetch = id.clone();
            runtime_for_select.spawn(async move {
                match gateway.fetch_history(Some(&id_for_fetch)).await {
                    Ok(resp) => {
                        let turns = resp.turns;
                        let _ = slint::invoke_from_event_loop(move || {
                            for t in turns {
                                if !t.user_input.is_empty() {
                                    push_user_block(&t.user_input);
                                }
                                if let Some(reply) = t.response {
                                    if !reply.is_empty() {
                                        append_jarvis_blocks(&reply);
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            "fetch_history failed for thread {}: {}",
                            id_for_fetch,
                            e
                        );
                    }
                }
            });
        });
    }
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        let current_thread = current_thread_id.clone();
        ui.on_new_conversation(move || {
            let gw = gateway.clone();
            let current_thread = current_thread.clone();
            runtime.spawn(async move {
                match gw.new_thread().await {
                    Ok(info) => {
                        *current_thread.lock() = Some(info.id.clone());
                        // Refresh the sidebar.
                        if let Ok(list) = gw.list_threads().await {
                            refresh_conversation_model(&list);
                        }
                        let _ = slint::invoke_from_event_loop(|| {
                            clear_transcript_rows();
                        });
                    }
                    Err(e) => tracing::warn!("new thread failed: {e}"),
                }
            });
        });
    }
    // Export the on-screen transcript as Markdown to the user's
    // Downloads directory (falls back to ironclad root). Status is
    // Sub-agent X button: removes the card immediately (cancellation
    // signal). The work continues server-side — we don't have a real
    // tool-call cancellation API — but the user gets the card out of
    // their view either way.
    {
        ui.on_dismiss_sub_agent(move |id| {
            let id = id.to_string();
            tracing::info!("dismiss_sub_agent: removing card id={id}");
            prune_sub_agent_row(&id);
        });
    }
    // Double-click on a sub-agent card → copy its full label+status to
    // the OS clipboard. The status string in the UI is elided so a
    // truncated "Tool error: Tool memory_r..." line still hides the
    // actual cause. Copying the unfried full text lets McKale paste it
    // back into chat or a debug query without having to expand the row.
    {
        let ui_weak = ui.as_weak();
        ui.on_copy_sub_agent(move |text| {
            let text = text.to_string();
            tracing::info!(
                "copy_sub_agent: copying {} chars to clipboard",
                text.chars().count()
            );
            match arboard::Clipboard::new() {
                Ok(mut cb) => {
                    if let Err(e) = cb.set_text(text.clone()) {
                        tracing::warn!("arboard set_text failed: {e}");
                        return;
                    }
                    let ui_weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(u) = ui_weak.upgrade() {
                            let preview: String = text.chars().take(40).collect();
                            u.set_status(format!("copied: {preview}").into());
                        }
                    });
                }
                Err(e) => tracing::warn!("arboard open failed: {e}"),
            }
        });
    }
    // surfaced via set_status so the user sees the path.
    {
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let current_thread = current_thread_id.clone();
        ui.on_export_conversation(move || {
            let Some(u) = ui_weak.upgrade() else { return; };
            u.set_status("exporting...".into());
            let thread_id = current_thread.lock().clone();
            let markdown = transcript_to_markdown(thread_id.as_deref());
            let ui_weak2 = ui_weak.clone();
            runtime.spawn(async move {
                let res = tokio::task::spawn_blocking(move || -> anyhow::Result<std::path::PathBuf> {
                    let dir = dirs::download_dir()
                        .or_else(dirs::home_dir)
                        .unwrap_or_else(std::env::temp_dir);
                    // Filename uses unix-ts so multiple exports in the
                    // same session sort + don't collide.
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let path = dir.join(format!("jarvis-conversation-{ts}.md"));
                    std::fs::write(&path, markdown)?;
                    Ok(path)
                })
                .await;
                let status = match res {
                    Ok(Ok(path)) => format!("exported: {}", path.display()),
                    Ok(Err(e)) => format!("export failed: {e}"),
                    Err(e) => format!("export join failed: {e}"),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(u) = ui_weak2.upgrade() {
                        u.set_status(status.into());
                    }
                });
            });
        });
    }
    // Periodic refresh of the conversation list.
    {
        let gateway = state.gateway.clone();
        let runtime_for_poll = runtime.clone();
        runtime_for_poll.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                if let Ok(list) = gateway.list_threads().await {
                    let _ = slint::invoke_from_event_loop(move || {
                        refresh_conversation_model(&list);
                    });
                }
            }
        });
    }

    // Control commands — gateway accepts slash forms via /api/chat/send
    // exactly like the Leptos dashboard does.
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        ui.on_undo_turn(move || {
            let gw = gateway.clone();
            runtime.spawn(async move {
                if let Err(e) = gw.send_chat("/undo", &[]).await {
                    tracing::warn!("undo failed: {e}");
                }
            });
        });
    }
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        ui.on_redo_turn(move || {
            let gw = gateway.clone();
            runtime.spawn(async move {
                if let Err(e) = gw.send_chat("/redo", &[]).await {
                    tracing::warn!("redo failed: {e}");
                }
            });
        });
    }
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        ui.on_compact_thread(move || {
            let gw = gateway.clone();
            runtime.spawn(async move {
                if let Err(e) = gw.send_chat("/compact", &[]).await {
                    tracing::warn!("compact failed: {e}");
                }
            });
        });
    }
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        ui.on_new_thread(move || {
            let gw = gateway.clone();
            runtime.spawn(async move {
                if let Err(e) = gw.send_chat("/new", &[]).await {
                    tracing::warn!("new thread failed: {e}");
                }
            });
            clear_transcript_rows();
        });
    }

    // SETUP button opens the native 5-step wizard. No more browser
    // bridge — wizard_open swaps the right column to WizardPanel.
    {
        let ui_weak = ui.as_weak();
        ui.on_open_wizard(move || {
            if let Some(u) = ui_weak.upgrade() {
                u.set_wizard_status("".into());
                u.set_wizard_open(true);
                // If settings was open, close it so the wizard owns the
                // panel cleanly.
                u.set_settings_open(false);
            }
        });
    }
    // FINISH writes to .env atomically and reports the path. Same
    // write_settings_to_env the SettingsPanel SAVE uses, but the
    // wizard pre-seeds the toggles to sane first-run defaults
    // (heartbeat on, autonomous off).
    {
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        ui.on_finish_wizard(move || {
            let Some(u) = ui_weak.upgrade() else { return; };
            let anthropic = u.get_anthropic_key().to_string();
            let model = u.get_model().to_string();
            let el_key = u.get_elevenlabs_key().to_string();
            let el_voice = u.get_elevenlabs_voice_id().to_string();
            u.set_wizard_saving(true);
            u.set_wizard_status("writing .env...".into());
            let ui_weak2 = ui_weak.clone();
            runtime.spawn(async move {
                let res = tokio::task::spawn_blocking(move || {
                    write_settings_to_env(WriteSettings {
                        anthropic_key: anthropic,
                        model,
                        elevenlabs_key: el_key,
                        elevenlabs_voice_id: el_voice,
                        heartbeat: true,
                        autonomous: false,
                    })
                })
                .await;
                let status = match res {
                    Ok(Ok(path)) => format!(
                        "wrote {} — restart `cargo run-jarvis` to apply.",
                        path.display()
                    ),
                    Ok(Err(e)) => format!("save failed: {e}"),
                    Err(e) => format!("save join failed: {e}"),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(u) = ui_weak2.upgrade() {
                        u.set_wizard_saving(false);
                        u.set_wizard_status(status.into());
                    }
                });
            });
        });
    }
    // CANCEL just closes the wizard without writing.
    {
        let ui_weak = ui.as_weak();
        ui.on_cancel_wizard(move || {
            if let Some(u) = ui_weak.upgrade() {
                u.set_wizard_open(false);
                u.set_wizard_status("".into());
            }
        });
    }
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        let ui_weak = ui.as_weak();
        let pending = state.pending_approval.clone();
        ui.on_approve(move |req_id, always| {
            let gateway = gateway.clone();
            let req_id = req_id.to_string();
            let ui_weak = ui_weak.clone();
            let pending = pending.clone();
            runtime.spawn(async move {
                if let Err(e) = gateway.send_approval(&req_id, true, always).await {
                    tracing::error!("approval send: {e}");
                }
                *pending.lock() = None;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(u) = ui_weak.upgrade() {
                        u.set_approval_request_id("".into());
                        u.set_approval_tool_name("".into());
                        u.set_approval_description("".into());
                    }
                });
            });
        });
    }
    {
        let gateway = state.gateway.clone();
        let runtime = runtime.clone();
        let ui_weak = ui.as_weak();
        let pending = state.pending_approval.clone();
        ui.on_deny(move |req_id| {
            let gateway = gateway.clone();
            let req_id = req_id.to_string();
            let ui_weak = ui_weak.clone();
            let pending = pending.clone();
            runtime.spawn(async move {
                if let Err(e) = gateway.send_approval(&req_id, false, false).await {
                    tracing::error!("deny send: {e}");
                }
                *pending.lock() = None;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(u) = ui_weak.upgrade() {
                        u.set_approval_request_id("".into());
                        u.set_approval_tool_name("".into());
                        u.set_approval_description("".into());
                    }
                });
            });
        });
    }

    // ----- global hotkeys
    //
    // Both default bindings can collide with existing system / app
    // assignments. Ctrl+Alt+M in particular shows up on McKale's box
    // (probably bound by an audio driver or Discord). Registration
    // failures are LOGGED and skipped instead of crashing the binary —
    // the UI still works via clicks; only the global keyboard shortcut
    // is unavailable. McKale can override defaults via env to dodge
    // conflicts: JARVIS_HOTKEY_EXPAND / JARVIS_HOTKEY_MUTE accept any
    // KeyCode name (e.g. "F9").
    let hotkey_manager = GlobalHotKeyManager::new().context("hotkey manager")?;
    let toggle_key = parse_key_env("JARVIS_HOTKEY_EXPAND", Code::KeyJ);
    let mute_key = parse_key_env("JARVIS_HOTKEY_MUTE", Code::Semicolon);
    let focus_key = parse_key_env("JARVIS_HOTKEY_FOCUS", Code::KeyK);
    let toggle_hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), toggle_key);
    let mute_hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), mute_key);
    // Bring-to-focus: show + expand + raise the window from any
    // workspace. The single shortcut that gets JARVIS in front of you
    // no matter what you're doing.
    let focus_hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), focus_key);
    let toggle_hk_id = toggle_hk.id();
    let mute_hk_id = mute_hk.id();
    let focus_hk_id = focus_hk.id();
    let mut toggle_ok = false;
    let mut mute_ok = false;
    match hotkey_manager.register(toggle_hk) {
        Ok(()) => {
            toggle_ok = true;
            tracing::info!("hotkey registered: Ctrl+Alt+{:?} = expand HUD", toggle_key);
        }
        Err(e) => tracing::warn!(
            "hotkey Ctrl+Alt+{:?} (expand) refused — already bound? {e} (\
             override via JARVIS_HOTKEY_EXPAND=<KeyName>)",
            toggle_key
        ),
    }
    match hotkey_manager.register(mute_hk) {
        Ok(()) => {
            mute_ok = true;
            tracing::info!("hotkey registered: Ctrl+Alt+{:?} = mute mic", mute_key);
        }
        Err(e) => tracing::warn!(
            "hotkey Ctrl+Alt+{:?} (mute) refused — already bound? {e} (\
             override via JARVIS_HOTKEY_MUTE=<KeyName>)",
            mute_key
        ),
    }
    let mut focus_ok = false;
    match hotkey_manager.register(focus_hk) {
        Ok(()) => {
            focus_ok = true;
            tracing::info!(
                "hotkey registered: Ctrl+Alt+{:?} = bring JARVIS to focus",
                focus_key
            );
        }
        Err(e) => tracing::warn!(
            "hotkey Ctrl+Alt+{:?} (focus) refused — already bound? {e} (\
             override via JARVIS_HOTKEY_FOCUS=<KeyName>)",
            focus_key
        ),
    }
    let _ = (toggle_ok, mute_ok, focus_ok);
    {
        let ui_weak = ui.as_weak();
        let mic_muted = state.mic_muted.clone();
        std::thread::spawn(move || {
            let receiver = GlobalHotKeyEvent::receiver();
            loop {
                if let Ok(event) = receiver.recv() {
                    if event.state != global_hotkey::HotKeyState::Pressed {
                        continue;
                    }
                    if event.id == toggle_hk_id {
                        let ui_weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(u) = ui_weak.upgrade() {
                                u.set_expanded(!u.get_expanded());
                            }
                        });
                    } else if event.id == mute_hk_id {
                        let ui_weak = ui_weak.clone();
                        let mic_muted = mic_muted.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let next = !mic_muted.load(Ordering::Relaxed);
                            mic_muted.store(next, Ordering::Relaxed);
                            if let Some(u) = ui_weak.upgrade() {
                                u.set_mic_active(!next);
                                u.set_status(
                                    if next { "muted".into() } else { "listening".into() },
                                );
                            }
                        });
                    } else if event.id == focus_hk_id {
                        let ui_weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(u) = ui_weak.upgrade() {
                                bring_to_focus(&u);
                            }
                        });
                    }
                }
            }
        });
    }

    // ----- system tray
    let tray_menu = Menu::new();
    let show_hide_item = MenuItem::new("Show / Hide", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let show_hide_id = show_hide_item.id().0.clone();
    let quit_id = quit_item.id().0.clone();
    tray_menu.append(&show_hide_item)?;
    tray_menu.append(&PredefinedMenuItem::separator())?;
    tray_menu.append(&quit_item)?;
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("JARVIS")
        .with_icon(build_tray_icon())
        .build()
        .context("build tray")?;
    {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let receiver = MenuEvent::receiver();
            loop {
                if let Ok(event) = receiver.recv() {
                    let id_str = event.id.0.clone();
                    if id_str == quit_id {
                        let _ = slint::invoke_from_event_loop(|| {
                            let _ = slint::quit_event_loop();
                        });
                    } else if id_str == show_hide_id {
                        let ui_weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(u) = ui_weak.upgrade() {
                                if u.window().is_visible() {
                                    let _ = u.window().hide();
                                } else {
                                    let _ = u.window().show();
                                }
                            }
                        });
                    }
                }
            }
        });
    }

    // ----- pulse + spin animation
    //
    // Two driving properties so the arc reactor stops looking like a
    // wobble: `pulse` is a sin oscillation 0→1→0 used for breathing
    // (opacity, scale), while `spin` is a monotonic 0→1 that wraps,
    // used as the rotation source. Multiply spin by 360deg in Slint
    // for full rotation per period; multiply by negative numbers for
    // counter-rotation. Periods chosen so the reactor reads as a
    // steady-state instrument, not an emergency siren.
    {
        let ui_weak = ui.as_weak();
        let mut breath_t: f32 = 0.0;
        let mut spin_t: f32 = 0.0;
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(33),
            move || {
                breath_t += 0.06;
                if breath_t > std::f32::consts::TAU {
                    breath_t -= std::f32::consts::TAU;
                }
                // ~8s per full rotation at 33ms/tick: 30tps * 8s = 240
                // steps → step = 1/240 ≈ 0.00417.
                spin_t += 0.00417;
                if spin_t >= 1.0 {
                    spin_t -= 1.0;
                }
                let breath = (breath_t.sin() * 0.5 + 0.5).clamp(0.0, 1.0);
                if let Some(u) = ui_weak.upgrade() {
                    u.set_pulse(breath);
                    u.set_spin(spin_t);
                }
            },
        );
        std::mem::forget(timer);
    }

    // Close button = full teardown. Slint's default for `X` is
    // CloseRequestResponse::HideWindow which leaves the binary
    // running invisibly, so jarvis-up's supervisor thinks the
    // desktop is still alive and keeps the gateway + parakeet up.
    // McKale wants X = Ctrl+C: process exits, jarvis-up sees the
    // supervised child die, triggers shutdown across the rest of
    // the stack.
    {
        let window = ui.window();
        window.on_close_requested(|| {
            tracing::info!("close requested — exiting (jarvis-up will tear down siblings)");
            // process::exit instead of ExitEventLoop so we close fast
            // even if a background tokio task is mid-await. The
            // supervisor on the parent side handles the rest.
            std::process::exit(0);
            #[allow(unreachable_code)]
            slint::CloseRequestResponse::HideWindow
        });
    }

    tracing::info!(
        "UI up. Hotkeys: Ctrl+Alt+J expand, Ctrl+Alt+M mute. Gateway = {}",
        gateway_url
    );
    ui.run().context("Slint event loop")?;
    Ok(())
}

/// Mic → VAD → STT → /api/chat/send.
///
/// Two STT paths:
///   1. **WebTransport streaming** (preferred when `streaming` is Some):
///      every 40ms frame ships as a QUIC datagram of 16 kHz f32 PCM.
///      The server returns interim partials + a final transcript via
///      the events stream, which the caller already drains into the
///      transcript UI + chat send. The pipeline just calls `start()`
///      on speech onset and `end()` on utterance close.
///   2. **POST /api/voice/stt** fallback: accumulate full utterance,
///      encode as WAV, POST when VAD endpoint hits. Original Phase 2 path.
/// Background task that brings voice online when parakeet-wt is
/// actually ready. Replaces the eager block_on attempt that ran at
/// boot, which would silently fail if the sidecar hadn't finished
/// loading its model — the user would launch the app, see a "ready"
/// status, talk into the mic, and get nothing back.
///
/// Flow:
///   1. Poll `/api/voice/status` every 1s until `stt_ready == true`.
///   2. Attempt the WebTransport streaming-STT connect; on success
///      store the handle in `state.streaming` and spawn an events
///      drainer task (interim/final → handle_stt_final).
///   3. Either way (WT or POST fallback) flip mic_muted off + flip
///      voice_ready on + flip the status to "listening".
///
/// If parakeet never becomes ready (sidecar failed, gateway down),
/// the loop keeps polling and the mic stays muted. The user sees a
/// persistent "warming up" status, which is more honest than the
/// previous behavior of pretending everything was fine.
async fn voice_startup(
    gateway: Gateway,
    state: State,
    ui_weak: slint::Weak<MainWindow>,
    speaker_queue: Arc<audio::PlaybackQueue>,
    runtime: Arc<tokio::runtime::Runtime>,
) {
    let probe_client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("voice_startup probe client build failed: {e}");
            return;
        }
    };

    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let url = format!("{}/api/voice/status", gateway.base_url());
        let stt_ready = match probe_client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match resp
                .json::<serde_json::Value>()
                .await
            {
                Ok(v) => v
                    .get("stt_ready")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                Err(_) => false,
            },
            _ => false,
        };

        if stt_ready {
            tracing::info!(
                "gateway /api/voice/status reports stt_ready after {} probes",
                attempts
            );
            break;
        }

        if attempts == 1 || attempts % 3 == 0 {
            let msg = format!(
                "step 1/2 · gateway warming up ({}s) · TYPE TO CHAT \u{2193}",
                attempts
            );
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_status(msg.into());
                }
            });
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // End-to-end probe: the gateway's stt_ready is a static `true`
    // (see ironclad/src/config.rs::stt_ready), so it flips green the
    // instant the gateway boots — BEFORE parakeet has loaded its
    // ~600MB model into VRAM. McKale's log showed the gateway saying
    // ready at T+0s, parakeet finishing model load at T+21s, and
    // every STT request in that window hitting 503/500.
    //
    // Real readiness check: actually POST a tiny silent WAV to
    // /api/voice/stt and confirm 200 OK. Only then unmute the mic.
    // We try forever with progressive status so the user knows
    // *which* layer is the holdup.
    let probe_wav = encode_wav_16khz(&vec![0.0_f32; 8000], 16_000);
    let mut probe_attempts: u32 = 0;
    loop {
        probe_attempts += 1;
        match state.gateway.stt(probe_wav.clone()).await {
            Ok(_) => {
                tracing::info!(
                    "parakeet model confirmed loaded (end-to-end probe ok after {} attempts)",
                    probe_attempts
                );
                break;
            }
            Err(e) => {
                tracing::debug!("parakeet not yet serving (probe {probe_attempts}): {e}");
            }
        }
        if probe_attempts == 1 || probe_attempts % 2 == 0 {
            let elapsed = probe_attempts * 2;
            let msg = format!(
                "step 2/2 · parakeet AI model loading ({elapsed}s, usually ~25s first launch) · TYPE TO CHAT \u{2193}"
            );
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_status(msg.into());
                }
            });
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Parakeet is ready. Attempt WebTransport streaming STT. Failure
    // here is fine — we fall back to the POST /api/voice/stt path,
    // which is what we'd be doing if streaming were disabled.
    let token = match gateway.ensure_token().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("voice_startup token fetch failed: {e}");
            String::new()
        }
    };
    if !token.is_empty() {
        match streaming_stt::StreamingStt::fetch_config(gateway.base_url(), &token).await {
            Ok(Some(cfg)) => match streaming_stt::StreamingStt::connect(cfg).await {
                Ok(s) => {
                    tracing::info!("WebTransport streaming STT online");
                    // Spawn the events drainer BEFORE installing the
                    // handle in state, so any interim events that
                    // arrive immediately don't get dropped.
                    spawn_wt_events_drainer(
                        s.clone(),
                        state.clone(),
                        ui_weak.clone(),
                        speaker_queue.clone(),
                        runtime.clone(),
                    );
                    *state.streaming.lock() = Some(s);
                }
                Err(e) => {
                    tracing::warn!("WT connect failed; using POST STT: {e}");
                }
            },
            Ok(None) => {
                tracing::info!("streaming STT not configured; using POST /api/voice/stt");
            }
            Err(e) => {
                tracing::warn!("streaming-config fetch failed: {e}");
            }
        }
    }

    // Voice is usable either way (WT preferred, POST fallback). Set
    // up a fresh conversation thread so the user can speak immediately
    // without first clicking + NEW CONVERSATION. If a thread is
    // already selected (e.g. reopened a saved conversation), leave it
    // alone.
    let need_thread = state.current_thread_id.lock().is_none();
    if need_thread {
        match state.gateway.new_thread().await {
            Ok(info) => {
                *state.current_thread_id.lock() = Some(info.id.clone());
                if let Ok(list) = state.gateway.list_threads().await {
                    let _ = slint::invoke_from_event_loop(move || {
                        refresh_conversation_model(&list);
                    });
                }
                tracing::info!("auto-created thread {} for first-run voice", info.id);
            }
            Err(e) => tracing::warn!("auto-create thread failed: {e}"),
        }
    }

    state.voice_ready.store(true, Ordering::Release);
    state.mic_muted.store(false, Ordering::Release);
    tracing::info!(
        "voice_startup complete — mic UNMUTED, voice_ready=true. \
         If JARVIS still doesn't hear you: (1) check Windows mic permission \
         for jarvis-desktop.exe, (2) confirm the Yeti input gain knob isn't \
         at zero, (3) watch for `mic frames` lines below."
    );
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(u) = ui_weak.upgrade() {
            u.set_mic_active(true);
            u.set_status("ready · speak or type".into());
        }
    });
}

/// Spawn a task that drains WT interim/final events into the chat
/// pipeline. Factored out so voice_startup can call it after the WT
/// connection comes up — the same drainer that originally ran at
/// boot, but now decoupled from the boot path.
fn spawn_wt_events_drainer(
    s: streaming_stt::StreamingStt,
    state: State,
    ui_weak: slint::Weak<MainWindow>,
    speaker_queue: Arc<audio::PlaybackQueue>,
    runtime: Arc<tokio::runtime::Runtime>,
) {
    runtime.spawn(async move {
        let Some(mut rx) = s.take_events().await else {
            return;
        };
        while let Some(ev) = rx.recv().await {
            match ev {
                streaming_stt::StreamingEvent::Interim(text) => {
                    let ui_weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(u) = ui_weak.upgrade() {
                            u.set_status(format!("hearing: {}", text).into());
                        }
                    });
                }
                streaming_stt::StreamingEvent::Final(text) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    tracing::info!("WT final: {text}");
                    handle_stt_final(
                        text,
                        state.clone(),
                        ui_weak.clone(),
                        speaker_queue.clone(),
                    )
                    .await;
                }
            }
        }
    });
}

async fn mic_pipeline(
    mic_rx: Receiver<MicFrame>,
    state: State,
    ui_weak: slint::Weak<MainWindow>,
    speaker_queue: Arc<audio::PlaybackQueue>,
    playback_monitor: Arc<audio::PlaybackMonitor>,
) {
    let mut vad: Option<Vad> = None;
    let mut frame_buffer: Vec<f32> = Vec::with_capacity(8192);
    let mut sample_rate = 48_000_u32;
    let mut target_frame: usize = (sample_rate as f32 * 0.04) as usize;
    let mut prev_state = vad::VadState::Listening;
    // Heartbeat + mute-change diagnostics so we can see whether the
    // mic is actually delivering samples and whether the VAD is
    // muted at the moment we expect it to be hot.
    let mut heartbeat_frames: u64 = 0;
    // Smoothed playback RMS — EMA over ~200ms so the comparison to
    // mic RMS isn't fooled by the instant-by-instant jitter of a tiny
    // cpal output frame (which dips into the 0.01 range during quiet
    // sub-syllables even when overall playback is loud).
    let mut playback_rms_ema: f32 = 0.0;
    // Sustained-barge-in counter. We require N consecutive 40ms
    // frames where the barge-in condition holds before actually
    // halting — kills false positives from the leading edge of a
    // syllable arriving at the mic ~150ms after stream open.
    let mut barge_streak: u32 = 0;
    // Ring buffer of the most recent ~1s of raw mic input at native
    // rate (48 kHz). Fed to Silero VAD when barge_streak fires so we
    // can confirm the trigger is real human speech (and not music or
    // JARVIS's own voice leaking through the half-duplex mute).
    let mut recent_mic: std::collections::VecDeque<f32> =
        std::collections::VecDeque::with_capacity(48_000);
    const RECENT_MIC_MAX: usize = 48_000;
    // Silero is also used as a second-stage gate on every utterance
    // shipped to parakeet. We score the buffered utterance audio at
    // endpoint and discard non-speech (music, TV, ambient chatter)
    // before the WT stream fires its Final.
    let mut mic_muted_last_seen: bool = true;
    // Per-utterance stats accumulated between onset and end so we can
    // tell whether parakeet's empty-final responses are because we sent
    // it too little audio or too quiet audio.
    let mut utt_start_samples: usize = 0;
    let mut utt_sum_sq: f32 = 0.0;
    tracing::info!("mic_pipeline: started — waiting for first mic frame");

    loop {
        let Ok(frame) = mic_rx.recv().await else {
            tracing::warn!("mic_pipeline: mic_rx closed — exiting");
            break;
        };

        if vad.is_none() {
            sample_rate = frame.sample_rate;
            target_frame = (sample_rate as f32 * 0.04) as usize;
            vad = Some(Vad::new(VadConfig::default(), sample_rate));
            tracing::info!(
                "mic_pipeline: first frame received — sr={} target_frame={} samples; \
                 VAD active. Heartbeat logs every ~5s show RMS + state.",
                sample_rate,
                target_frame
            );
        }
        let v = vad.as_mut().unwrap();
        let muted_by_user = state.mic_muted.load(Ordering::Relaxed);
        let tts_playing = speaker_queue.is_tts_active();

        // Slide the raw mic frame into the 1s ring buffer so Silero
        // has recent context when the barge-in heuristic asks.
        for &s in frame.samples.iter() {
            if recent_mic.len() >= RECENT_MIC_MAX {
                recent_mic.pop_front();
            }
            recent_mic.push_back(s);
        }

        // Barge-in detector. Software AEC's first cousin: compare mic
        // RMS to a smoothed estimate of how loud the speaker is right
        // now. When mic energy substantially exceeds what the speaker
        // alone could put through the echo path, the user is talking
        // over JARVIS — halt the queue, unmute the VAD, capture the
        // utterance start.
        //
        // Lessons from the false-positive sweep on the first build:
        //   - Raw `recent_rms()` is per-cpal-frame and dips into 0.01
        //     during quiet sub-syllables; smooth with an EMA over
        //     ~200ms so the comparison is fair (mic side is already
        //     averaged over a 40ms frame).
        //   - First syllable of JARVIS's voice reaching the mic spikes
        //     above the echo baseline; require 3 consecutive frames
        //     (~120ms) of barge-in condition before halting so a
        //     transient mic spike can't self-halt.
        //   - Calibrate hard: a real shout over JARVIS lands at 0.20+,
        //     echo of his own voice tops out around 0.10. Threshold
        //     0.15 absolute + 4x ratio keeps a comfortable margin.
        let mut bargein = false;
        if tts_playing && !muted_by_user {
            let frame_sum_sq: f32 = frame.samples.iter().map(|s| s * s).sum();
            let frame_rms = if !frame.samples.is_empty() {
                (frame_sum_sq / frame.samples.len() as f32).sqrt()
            } else {
                0.0
            };
            let instant_playback = playback_monitor.recent_rms();
            // 40ms frame, ~200ms time constant → alpha ≈ 0.2.
            playback_rms_ema = playback_rms_ema * 0.8 + instant_playback * 0.2;

            // Relaxed: normal-volume "stop" lands around 0.08-0.12 RMS;
            // the old 0.15 absolute floor required shouting. Silero
            // remains the rigor — JARVIS echo confused with speech
            // gets rejected at the second stage.
            let condition = frame_rms > 0.08 && frame_rms > playback_rms_ema * 2.5;
            if condition {
                barge_streak += 1;
            } else {
                barge_streak = 0;
            }
            if barge_streak >= 3 {
                // Second-stage: ask Silero whether the last second of
                // mic actually looks like human speech. Music, TV
                // audio, JARVIS's own voice playing through the
                // speakers, and pure noise all score below 0.5 — so
                // they no longer false-halt the queue. Real shouted
                // "STOP" sits well above 0.8.
                let recent: Vec<f32> = recent_mic.iter().copied().collect();
                let silero = speech_gate::score(&recent, sample_rate);
                if silero >= 0.5 {
                    tracing::info!(
                        "barge-in confirmed (mic_rms={:.4}, playback_ema={:.4}, streak={}, silero={:.2}) — halting TTS",
                        frame_rms,
                        playback_rms_ema,
                        barge_streak,
                        silero
                    );
                    speaker_queue.halt();
                    speaker_queue.mark_tts_idle();
                    bargein = true;
                    barge_streak = 0;
                } else {
                    tracing::debug!(
                        "barge-in candidate rejected by Silero (mic_rms={:.4}, playback_ema={:.4}, silero={:.2})",
                        frame_rms,
                        playback_rms_ema,
                        silero
                    );
                    barge_streak = 0;
                }
            }
        } else {
            // Reset streak whenever we're not in the barge-in window.
            barge_streak = 0;
            playback_rms_ema = 0.0;
        }

        // Half-duplex: while TTS is actively playing, the speaker is
        // pumping JARVIS's own voice into the room — the Yeti picks it
        // up, VAD treats it as a new utterance, parakeet transcribes
        // it, JARVIS responds to itself in an infinite loop. Mute the
        // VAD whenever the playback queue has more than ~50ms of
        // pending audio. Browser-side getUserMedia AEC would solve
        // this cleanly; cpal has no equivalent. Bypass the mute on
        // detected barge-in so the VAD captures the interrupting
        // utterance.
        let muted_now = muted_by_user || (tts_playing && !bargein);
        if muted_now != mic_muted_last_seen {
            tracing::info!(
                "mic_pipeline: mic effective-mute changed → {} (user_mute={}, tts_playing={})",
                muted_now,
                muted_by_user,
                tts_playing
            );
            mic_muted_last_seen = muted_now;
        }
        v.set_muted(muted_now);

        frame_buffer.extend(frame.samples);
        while frame_buffer.len() >= target_frame {
            let chunk: Vec<f32> = frame_buffer.drain(..target_frame).collect();

            // Heartbeat diagnostic: log RMS + state every ~5s so we can
            // see what the VAD is actually seeing without flooding.
            let sum_sq: f32 = chunk.iter().map(|s| s * s).sum();
            let rms = (sum_sq / chunk.len() as f32).sqrt();
            heartbeat_frames += 1;
            if heartbeat_frames % 125 == 0 {
                // 125 frames × 40ms = 5s
                tracing::info!(
                    "mic heartbeat: frames={} rms={:.4} muted={} vad_state={:?}",
                    heartbeat_frames,
                    rms,
                    muted_now,
                    v.state()
                );
            }

            let utterance = v.feed(&chunk);
            let cur_state = v.state();
            if prev_state == vad::VadState::Listening
                && cur_state == vad::VadState::Utterance
            {
                tracing::info!("VAD speech onset (rms={:.4})", rms);
                utt_start_samples = 0;
                utt_sum_sq = 0.0;
                speaker_queue.halt();
                if let Some(s) = state.streaming.lock().as_ref() {
                    s.start();
                    // Flush the VAD's pre-roll into the streaming
                    // STT first. Without this, parakeet only sees
                    // audio AFTER the VAD has committed (400 ms of
                    // sustained voice) and the first part of the
                    // user's utterance — usually the most important
                    // verb — is missing. The pre_roll buffer holds
                    // ~600 ms of audio that pushed the VAD over its
                    // threshold; piping it through here means
                    // parakeet sees the full first word.
                    if let Some(pre) = v.take_streaming_preroll() {
                        let mut resampled = if sample_rate == 16_000 {
                            pre
                        } else {
                            downsample_to_16k(&pre, sample_rate)
                        };
                        if !resampled.is_empty() {
                            const STT_GAIN: f32 = 4.0;
                            for sample in resampled.iter_mut() {
                                *sample = (*sample * STT_GAIN).clamp(-1.0, 1.0);
                            }
                            tracing::debug!(
                                "VAD pre-roll → streaming STT: {} samples ({:.0} ms)",
                                resampled.len(),
                                resampled.len() as f32 * 1000.0 / 16_000.0
                            );
                            s.send_audio(resampled);
                        }
                    }
                }
            }
            // Track utterance audio stats so we can log a summary when
            // it ends — tells us exactly what parakeet received.
            if cur_state == vad::VadState::Utterance {
                utt_start_samples += chunk.len();
                utt_sum_sq += sum_sq;
            }
            if prev_state == vad::VadState::Utterance
                && cur_state == vad::VadState::Listening
            {
                let utt_secs = utt_start_samples as f32 / sample_rate as f32;
                let utt_rms = if utt_start_samples > 0 {
                    (utt_sum_sq / utt_start_samples as f32).sqrt()
                } else {
                    0.0
                };
                tracing::info!(
                    "utterance summary: {:.2}s ({} samples) avg_rms={:.4}",
                    utt_secs,
                    utt_start_samples,
                    utt_rms
                );
            }
            // While in-utterance, forward downsampled mono 16k to the
            // WT datagram channel for interim transcription. Apply a
            // 4× STT-bound gain AFTER VAD has decided this is speech
            // — VAD still sees the raw signal for accurate noise-floor
            // learning + silence detection, while parakeet receives a
            // healthy ~RMS-0.10 signal even from quiet mics. This
            // mirrors the dashboard's browser-AGC behavior without
            // contaminating the VAD's adaptive threshold.
            const STT_GAIN: f32 = 4.0;
            if cur_state == vad::VadState::Utterance {
                if let Some(s) = state.streaming.lock().as_ref() {
                    let mut resampled = if sample_rate == 16_000 {
                        chunk.clone()
                    } else {
                        downsample_to_16k(&chunk, sample_rate)
                    };
                    if !resampled.is_empty() {
                        for sample in resampled.iter_mut() {
                            *sample = (*sample * STT_GAIN).clamp(-1.0, 1.0);
                        }
                        s.send_audio(resampled);
                    }
                }
            }
            prev_state = cur_state;

            if let Some(utterance) = utterance {
                // Second-stage speech gate — Silero ONNX when present,
                // fail-OPEN (1.0) when not. Mirrors the dashboard's
                // silero.js shim, which returns 1.0 if the model
                // failed to load so the gate "never silences the user
                // because the gate is broken."
                let gate_sr = sample_rate;
                let gate_score = speech_gate::score(&utterance, gate_sr);
                if gate_score < 0.5 {
                    tracing::info!(
                        "speech_gate rejected utterance: score={:.2} ({} samples @ {} Hz)",
                        gate_score,
                        utterance.len(),
                        gate_sr
                    );
                    if let Some(s) = state.streaming.lock().as_ref() {
                        if s.is_alive() {
                            s.reset();
                        }
                    }
                    speaker_queue.resume();
                    let ui_weak2 = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(u) = ui_weak2.upgrade() {
                            u.set_status("listening".into());
                        }
                    });
                    continue;
                }

                // If WT is up AND healthy, tell it the utterance is
                // over and let the events stream deliver the final
                // transcript. If the WT side has died (server crash,
                // connection drop), fall back to POST /api/voice/stt
                // automatically — without this check we'd silently
                // lose every utterance until the user restarted.
                if let Some(s) = state.streaming.lock().as_ref() {
                    if s.is_alive() {
                        s.end();
                        continue;
                    }
                    tracing::warn!(
                        "WT marked dead; falling back to POST /api/voice/stt for this utterance"
                    );
                }
                tracing::info!(
                    "utterance closed ({} samples @ {} Hz; ~{:.2}s)",
                    utterance.len(),
                    sample_rate,
                    utterance.len() as f32 / sample_rate as f32
                );
                // Same STT-bound gain as the WT path so parakeet sees
                // a consistent signal level regardless of which path
                // was used.
                let mut boosted = utterance.clone();
                for s in boosted.iter_mut() {
                    *s = (*s * STT_GAIN).clamp(-1.0, 1.0);
                }
                let wav = encode_wav_16khz(&boosted, sample_rate);
                let state_clone = state.clone();
                let ui_weak2 = ui_weak.clone();
                let speaker_queue_for_intent = speaker_queue.clone();
                tokio::spawn(async move {
                    // Status: heard you.
                    let _ = slint::invoke_from_event_loop({
                        let ui_weak2 = ui_weak2.clone();
                        move || {
                            if let Some(u) = ui_weak2.upgrade() {
                                u.set_status("transcribing...".into());
                            }
                        }
                    });
                    match state_clone.gateway.stt(wav).await {
                        Ok(text) if !text.trim().is_empty() => {
                            tracing::info!("STT: {text}");
                            // Voice-intent dispatch (stop/halt, voice
                            // approval, normal chat). Same surface as
                            // WT-final — both audio paths share it.
                            handle_stt_final(
                                text,
                                state_clone.clone(),
                                ui_weak2.clone(),
                                speaker_queue_for_intent.clone(),
                            )
                            .await;
                        }
                        Ok(_) => {
                            let _ = slint::invoke_from_event_loop({
                                let ui_weak2 = ui_weak2.clone();
                                move || {
                                    if let Some(u) = ui_weak2.upgrade() {
                                        u.set_status("listening".into());
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("STT failed: {e}");
                            let _ = slint::invoke_from_event_loop({
                                let ui_weak2 = ui_weak2.clone();
                                move || {
                                    if let Some(u) = ui_weak2.upgrade() {
                                        u.set_status(format!("STT error: {e}").into());
                                    }
                                }
                            });
                        }
                    }
                });
            }
        }
    }
}

/// SSE consumer — drives transcript + TTS playback + status + approval banner.
async fn sse_consumer(
    gateway: Gateway,
    ui_weak: slint::Weak<MainWindow>,
    state: State,
    speaker_queue: Arc<audio::PlaybackQueue>,
) {
    loop {
        let ui_weak = ui_weak.clone();
        let state = state.clone();
        let speaker_queue = speaker_queue.clone();
        let gw_for_tts = gateway.clone();
        let result = gateway
            .subscribe_events(move |event| {
                handle_chat_event(
                    event,
                    ui_weak.clone(),
                    state.clone(),
                    speaker_queue.clone(),
                    gw_for_tts.clone(),
                );
            })
            .await;
        if let Err(e) = result {
            tracing::warn!("SSE stream closed: {e}; reconnecting in 3s");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn handle_chat_event(
    event: ChatEvent,
    ui_weak: slint::Weak<MainWindow>,
    state: State,
    speaker_queue: Arc<audio::PlaybackQueue>,
    gateway: Gateway,
) {
    match event {
        ChatEvent::Status { message } | ChatEvent::Thinking { message } => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_status(message.into());
                }
            });
        }
        ChatEvent::StreamChunk { content } => {
            // Append to streaming buffer and feed splitter; on each
            // complete sentence, kick off a TTS stream.
            let sentences = {
                let mut buf = state.streaming_buffer.lock();
                buf.push_str(&content);
                let mut splitter = state.splitter.lock();
                splitter.push(&content)
            };
            for sentence in sentences {
                spawn_tts(sentence, &state);
            }
            let buf_snapshot = state.streaming_buffer.lock().clone();
            let was_streaming = state.streaming_active.swap(true, Ordering::Relaxed);
            let _ = slint::invoke_from_event_loop(move || {
                update_streaming_jarvis(&buf_snapshot, was_streaming);
            });
        }
        ChatEvent::Response { content } => {
            // Was this a streamed response (chunks already flushed) or
            // a single-shot control-command response? Mirror the
            // dashboard's logic.
            let was_streamed = !state.streaming_buffer.lock().is_empty();
            // Flush splitter; if it had content, queue any tail sentence.
            let tail = {
                let mut splitter = state.splitter.lock();
                splitter.finish()
            };
            if !was_streamed {
                let mut sentences = {
                    let mut splitter = state.splitter.lock();
                    splitter.push(&content)
                };
                if let Some(t) = state.splitter.lock().finish() {
                    sentences.push(t);
                }
                for s in sentences {
                    spawn_tts(s, &state);
                }
            } else if let Some(t) = tail {
                spawn_tts(t, &state);
            }
            state.streaming_buffer.lock().clear();
            state.streaming_active.store(false, Ordering::Relaxed);
            let final_text = content.clone();
            let ui_weak_for_notify = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                replace_streaming_with_final(&final_text, was_streamed);
                if let Some(u) = ui_weak_for_notify.upgrade() {
                    u.set_status("ready".into());
                    if !u.window().is_visible() {
                        notify::show_native_notification(
                            "JARVIS",
                            &truncate_for_toast(&final_text, 120),
                        );
                    }
                }
            });
        }
        ChatEvent::ToolStarted { name } => {
            let name_clone = name.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_status(format!("tool: {name_clone}").into());
                }
                // Push a placeholder tool row that subsequent events can
                // update with the result. Marks the start of one tool
                // call in the transcript.
                push_tool_row(&format!("→ {}", name), TOOL_TONE_PENDING);
            });
        }
        ChatEvent::ToolCompleted { name, success } => {
            let name_clone = name.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_status(
                        format!(
                            "tool {} {}",
                            name_clone,
                            if success { "ok" } else { "FAILED" }
                        )
                        .into(),
                    );
                }
                update_last_tool_completion(&name, success);
            });
        }
        ChatEvent::ToolResult { name, preview } => {
            let _ = slint::invoke_from_event_loop(move || {
                update_last_tool_preview(&name, &preview);
            });
        }
        ChatEvent::ApprovalNeeded {
            request_id,
            tool_name,
            description,
            parameters: _,
        } => {
            // Track on the State so background STT tasks can interpret
            // "yes" / "no" / "always" / "deny" utterances as voice
            // answers instead of new chat turns.
            *state.pending_approval.lock() = Some(PendingApproval {
                request_id: request_id.clone(),
                tool_name: tool_name.clone(),
            });
            let req_id_for_ui = request_id;
            let tool_name_for_ui = tool_name;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_approval_request_id(req_id_for_ui.into());
                    u.set_approval_tool_name(tool_name_for_ui.into());
                    u.set_approval_description(description.into());
                    u.set_expanded(true); // pop the HUD so user sees it
                }
            });
        }
        ChatEvent::Error { message } => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_status(format!("error: {message}").into());
                }
            });
        }
        ChatEvent::SubAgentStarted { id, label, kind } => {
            let _ = slint::invoke_from_event_loop(move || {
                push_sub_agent_row(&id, &label, &kind);
            });
        }
        ChatEvent::SubAgentProgress { id, message } => {
            let _ = slint::invoke_from_event_loop(move || {
                update_sub_agent_status(&id, &message);
            });
        }
        ChatEvent::SubAgentCompleted {
            id,
            success,
            summary,
        } => {
            let _ = slint::invoke_from_event_loop(move || {
                mark_sub_agent_done(&id, success, &summary);
            });
            // Don't auto-prune — sub-agent cards are persistent
            // named identities (Memory Diver, Vault Keeper, …) that
            // accumulate turn_count across the session. Removing them
            // on completion would discard the accumulated context the
            // name represents. X-button on the card is the only path
            // out for the user.
        }
    }
}

/// True if `text` looks like a parakeet hallucination from non-speech
/// audio (music, fan noise, mouse clicks, breath). Empirical list of
/// single-token outputs parakeet commonly emits for ambient noise.
/// Callers should ALWAYS check `has_pending_approval` before applying
/// this filter — when an approval banner is up, "yeah/yes/no" mean
/// actual choices and must not be dropped.
fn is_likely_parakeet_hallucination(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let collapsed: String = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        collapsed.as_str(),
        // Filler interjections
        "yeah" | "yes" | "yep" | "yup" | "uh" | "uhh" | "umm" | "um"
        | "hmm" | "hm" | "mm" | "mmm" | "ah" | "ahh" | "oh" | "ohh"
        | "huh" | "ha" | "haha" | "hey"
        // Common 1-2 word non-speech transcriptions
        | "you" | "it" | "the" | "and" | "but" | "so" | "well"
        | "right" | "okay" | "ok" | "bye" | "thanks" | "thank you"
        | "thank" | "i" | "a" | "is" | "no" | "now" | "to"
        // Music-lyric style misfires
        | "yeah yeah" | "la la" | "na na" | "oh yeah" | "ah ha"
    )
}

/// Non-blocking. Pushes the sentence to the TTS worker channel. The
/// worker task drains the channel one sentence at a time, awaiting
/// each `tts_stream` + breath gap before pulling the next. This
/// replaces the N-parallel-spawn architecture that overflowed the
/// playback queue with thousands of dropped samples per response.
fn spawn_tts(sentence: splitter::Sentence, state: &State) {
    let speakable = strip_markdown(&sentence.text);
    if speakable.trim().is_empty() {
        return;
    }
    if let Err(e) = state.tts_tx.send(sentence) {
        tracing::warn!("tts_tx send failed (worker may have exited): {e}");
    }
}

/// Background worker that owns the speaker_queue serialization. One
/// sentence at a time: resume the queue, stream from ElevenLabs +
/// push chunks to the playback queue, await the natural breath gap,
/// then pull the next sentence.
async fn tts_worker(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<splitter::Sentence>,
    speaker_queue: Arc<audio::PlaybackQueue>,
    gateway: Gateway,
    stop_requested: Arc<AtomicBool>,
) {
    // ~0.5s leftover. The cpal output thread drains queue→speaker at
    // device rate; once we're this close to empty, the speaker is
    // about to underrun and we can safely shove the next sentence
    // without overwriting in-flight audio.
    const DRAIN_THRESHOLD_SAMPLES: usize = 24_000; // ~0.5s @ 48kHz
    // Cap drain wait so a stuck output device never wedges the worker.
    const MAX_DRAIN_WAIT_MS: u64 = 8_000;

    while let Some(sentence) = rx.recv().await {
        // Voice-Stop drain: if McKale said "stop" since the prior
        // sentence finished, dump every queued sentence instead of
        // resuming through them. Without this, halt() only cuts the
        // current sentence and the next one resumes via resume() in
        // the worker — the user perceives "stop" as not working.
        if stop_requested.load(Ordering::Acquire) {
            let mut dropped = 1; // this sentence
            while rx.try_recv().is_ok() {
                dropped += 1;
            }
            tracing::info!(
                "tts_worker: stop_requested honored, dropped {} sentence(s)",
                dropped
            );
            continue;
        }
        let speakable = strip_markdown(&sentence.text);
        if speakable.trim().is_empty() {
            continue;
        }
        let ends_paragraph = sentence.ends_paragraph;
        // Mark TTS active for the whole sentence + breath cycle so the
        // mic stays muted through any momentary pending_samples→0
        // gaps between chunks.
        speaker_queue.mark_tts_active();
        speaker_queue.resume();
        let q = speaker_queue.clone();
        let on_chunk = move |sr: u32, samples: Vec<f32>| {
            q.enqueue_pcm(sr, &samples);
        };
        if let Err(e) = gateway.tts_stream(&speakable, on_chunk).await {
            tracing::warn!("tts_stream failed for sentence: {e}");
        }

        // Wait for the previous sentence to ACTUALLY DRAIN through the
        // output device before pushing the next one. Without this, a
        // run of fast LLM tokens or a list render pumps sentences into
        // the queue faster than cpal can play them — overflowing the
        // 10s cap, dropping samples, and audibly clobbering sentence N
        // with sentence N+1 ("says one thing then it gets replaced"
        // bug). Threshold-based wait lets the next stream START while
        // the tail of the current one is still playing (smooth
        // crossover) but never lets a backlog accumulate.
        let drain_start = std::time::Instant::now();
        while speaker_queue.pending_samples() > DRAIN_THRESHOLD_SAMPLES {
            if drain_start.elapsed().as_millis() as u64 > MAX_DRAIN_WAIT_MS {
                tracing::warn!(
                    "tts drain wait hit {}ms cap, pending={} — forcing through",
                    MAX_DRAIN_WAIT_MS,
                    speaker_queue.pending_samples()
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Now lay down the inter-sentence breath. Longer gaps make
        // JARVIS sound less robotic and give the listener a beat to
        // think. Paragraph ends get a real pause.
        let gap = if ends_paragraph { 0.75 } else { 0.45 };
        speaker_queue.add_gap(gap);

        // If we're done with the burst, hold the TTS-active flag a
        // touch longer to cover speaker decay echo before releasing
        // the mic.
        if rx.is_empty() {
            while speaker_queue.pending_samples() > 2400 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            speaker_queue.mark_tts_idle();
        }
    }
}

/// Decide what to do with an STT final transcript, and execute. Wraps:
///
///   1. `voice_intent::classify` against the current approval state
///   2. If Stop: halt the speaker queue + drop the utterance
///   3. If Approval: POST to /api/chat/approval, clear pending state
///   4. If SendChat: optional screen-frame grab + send_chat_in_thread
///
/// Centralizes the previously-duplicated logic that lived inline in the
/// WT-final drainer AND the POST-STT branch of mic_pipeline. Both call
/// sites now share this one function so a fix to the dispatch logic
/// covers both audio paths.
async fn handle_stt_final(
    text: String,
    state: State,
    ui_weak: slint::Weak<MainWindow>,
    speaker_queue: Arc<audio::PlaybackQueue>,
) {
    let trimmed = text.trim();
    tracing::info!(
        "handle_stt_final ENTRY: text={:?} (trimmed_len={})",
        text,
        trimmed.len()
    );
    if trimmed.is_empty() {
        tracing::warn!("handle_stt_final: empty after trim — dropping");
        return;
    }
    let has_pending = state.pending_approval.lock().is_some();

    // Hallucination filter. When parakeet gets non-speech audio
    // (music, fan noise, mouse clicks, breath, room tone amplified
    // by AGC), it commonly emits one of a small set of filler
    // tokens — "Yeah", "Uh", "Hmm", "Bye", "Thanks". These flow
    // through as real chat sends and JARVIS dutifully responds to
    // them, creating a noise-driven conversation. Drop any
    // utterance that's a single short word from this list UNLESS
    // an approval banner is pending (where "yeah/yes" mean
    // approve) or the user explicitly said a stop word ("stop"
    // → voice_intent::Stop handled below). Real-Silero VAD is the
    // permanent fix; this filter is a tight workaround that costs
    // us nothing in normal conversation.
    if !has_pending && is_likely_parakeet_hallucination(trimmed) {
        tracing::info!(
            "handle_stt_final: dropping likely parakeet hallucination from background noise: {:?}",
            trimmed
        );
        return;
    }

    let is_wake_word_only = state.wake_word_only.load(Ordering::Relaxed);
    let intent = voice_intent::classify(trimmed, has_pending, is_wake_word_only);
    tracing::info!(
        "handle_stt_final: intent={:?} has_pending_approval={} wake_word_only={}",
        intent,
        has_pending,
        is_wake_word_only
    );
    match intent {
        voice_intent::VoiceIntent::Stop => {
            tracing::info!("voice-stop: '{}' — halting TTS and draining pipeline", trimmed);
            speaker_queue.halt();
            state.splitter.lock().reset();
            // Tell the tts_worker to drop any sentences already in
            // its mpsc channel instead of resuming through them.
            // Cleared on the next user-initiated chat send so the
            // following response flows normally.
            state.stop_requested.store(true, Ordering::Release);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_status("stopped".into());
                }
            });
        }
        voice_intent::VoiceIntent::Mute => {
            tracing::info!("voice-mute: '{}' — entering wake-word-only mode", trimmed);
            speaker_queue.halt();
            state.splitter.lock().reset();
            state.wake_word_only.store(true, Ordering::Release);
            // Flip the reactor visual to MUTED so the front end matches
            // the audible state. We DON'T touch `state.mic_muted` —
            // doing so would disable the VAD entirely and we'd never
            // hear the "Jarvis" wake word. Visual state is purely UX;
            // the mic stays hot under the hood.
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_mic_active(false);
                    u.set_status("muted · say \"Jarvis\" to wake".into());
                }
            });
        }
        voice_intent::VoiceIntent::Wake(rest) => {
            tracing::info!("voice-wake: '{}' — clearing wake-word-only mode (trailing={:?})", trimmed, rest);
            state.wake_word_only.store(false, Ordering::Release);
            speaker_queue.resume();
            // Restore the reactor visual to LIVE so the front end
            // matches. Symmetric with the Mute branch above — we
            // didn't touch `state.mic_muted` there, so we don't here.
            let _ = slint::invoke_from_event_loop({
                let ui_weak = ui_weak.clone();
                move || {
                    if let Some(u) = ui_weak.upgrade() {
                        u.set_mic_active(true);
                        u.set_status("listening".into());
                    }
                }
            });
            // If the wake word carried a trailing command ("Jarvis,
            // what's open?"), send the rest as a normal chat turn so
            // the user doesn't have to wake-then-speak in two breaths.
            if !rest.is_empty() {
                state.stop_requested.store(false, Ordering::Release);
                let text_for_push = rest.clone();
                let _ = slint::invoke_from_event_loop({
                    let ui_weak = ui_weak.clone();
                    let text = text_for_push.clone();
                    move || {
                        if let Some(u) = ui_weak.upgrade() {
                            u.set_status("thinking...".into());
                        }
                        push_user_block(&text);
                    }
                });
                let images: Vec<String> = if state.screen_share_on.load(Ordering::Relaxed) {
                    match tokio::task::spawn_blocking(screen::capture_foreground).await {
                        Ok(Ok(b64)) => vec![b64],
                        _ => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                let thread = state.current_thread_id.lock().clone();
                if let Err(e) = state
                    .gateway
                    .send_chat_in_thread(&text_for_push, &images, thread.as_deref())
                    .await
                {
                    tracing::warn!("send_chat after wake failed: {e}");
                }
            }
        }
        voice_intent::VoiceIntent::Ignore => {
            tracing::debug!(
                "wake-word-only: dropping non-wake utterance: {:?}",
                trimmed
            );
        }
        voice_intent::VoiceIntent::Approval(action) => {
            let Some(pending) = state.pending_approval.lock().clone() else {
                // Race: approval was already resolved between classify()
                // and now. Drop the utterance.
                return;
            };
            tracing::info!(
                "voice-approval: '{}' → {} for request_id={}",
                trimmed,
                action,
                pending.request_id
            );
            let approved = action != "deny";
            let always = action == "always";
            if let Err(e) = state
                .gateway
                .send_approval(&pending.request_id, approved, always)
                .await
            {
                tracing::error!("voice approval POST failed: {e}");
            }
            *state.pending_approval.lock() = None;
            let action_owned = action.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_approval_request_id("".into());
                    u.set_approval_tool_name("".into());
                    u.set_approval_description("".into());
                    u.set_status(format!("approval: {action_owned}").into());
                }
            });
        }
        voice_intent::VoiceIntent::SendChat => {
            // If an approval was pending and the user said something
            // unrelated, classify returned SendChat but we should DROP
            // (not chat-send) the utterance — matching Leptos behavior.
            // This avoids accidentally sending "hmm" as a new turn.
            if has_pending {
                tracing::debug!(
                    "voice ignored during pending approval: '{}'",
                    trimmed
                );
                return;
            }
            // New voice turn → clear any pending Stop so the response
            // plays normally.
            state.stop_requested.store(false, Ordering::Release);
            let text_for_push = trimmed.to_string();
            let _ = slint::invoke_from_event_loop({
                let ui_weak = ui_weak.clone();
                let text = text_for_push.clone();
                move || {
                    if let Some(u) = ui_weak.upgrade() {
                        u.set_status("thinking...".into());
                    }
                    push_user_block(&text);
                }
            });
            let images: Vec<String> = if state.screen_share_on.load(Ordering::Relaxed) {
                match tokio::task::spawn_blocking(screen::capture_foreground).await {
                    Ok(Ok(b64)) => vec![b64],
                    Ok(Err(e)) => {
                        tracing::warn!("screen capture failed: {e}");
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("screen capture join: {e}");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let thread = state.current_thread_id.lock().clone();
            tracing::info!(
                "handle_stt_final: POST /api/chat/send text={:?} images={} thread_id={:?}",
                text_for_push,
                images.len(),
                thread
            );
            match state
                .gateway
                .send_chat_in_thread(&text_for_push, &images, thread.as_deref())
                .await
            {
                Ok(()) => tracing::info!("handle_stt_final: send_chat OK"),
                Err(e) => tracing::error!("handle_stt_final: send_chat ERROR: {e}"),
            }
        }
    }
}

/// Health check poller. Hits the gateway's /api/voice/status endpoint
/// every 5 seconds and refreshes the telemetry orbs. The dashboard's
/// TelemetryStrip is its inspiration.
async fn telemetry_poll(
    gateway_url: String,
    telemetry: Arc<TelemetryState>,
    ui_weak: slint::Weak<MainWindow>,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("build telemetry client");
    loop {
        let gateway_up = client
            .get(format!("{gateway_url}/api/voice/status"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        telemetry.gateway_up.store(gateway_up, Ordering::Relaxed);
        if gateway_up {
            // Record success timestamp so the UI can detect stale
            // probes (gateway running but unreachable from us).
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            telemetry
                .last_success_ms
                .store(now_ms, std::sync::atomic::Ordering::Release);
        }

        // The voice/status payload reports STT + TTS backends. Decode if
        // available.
        let (parakeet_up, elevenlabs_up) = if gateway_up {
            let resp = client
                .get(format!("{gateway_url}/api/voice/status"))
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let v = r
                        .json::<serde_json::Value>()
                        .await
                        .unwrap_or(serde_json::Value::Null);
                    (
                        v.get("stt_ready").and_then(|x| x.as_bool()).unwrap_or(false),
                        v.get("tts_ready").and_then(|x| x.as_bool()).unwrap_or(false),
                    )
                }
                Err(_) => (false, false),
            }
        } else {
            (false, false)
        };
        telemetry.parakeet_up.store(parakeet_up, Ordering::Relaxed);
        telemetry.elevenlabs_up.store(elevenlabs_up, Ordering::Relaxed);
        telemetry.claude_up.store(gateway_up, Ordering::Relaxed);

        let _ = slint::invoke_from_event_loop({
            let telemetry = telemetry.clone();
            let ui_weak = ui_weak.clone();
            move || {
                if let Some(u) = ui_weak.upgrade() {
                    u.set_gateway_up(telemetry.gateway_up.load(Ordering::Relaxed));
                    u.set_parakeet_up(telemetry.parakeet_up.load(Ordering::Relaxed));
                    u.set_elevenlabs_up(telemetry.elevenlabs_up.load(Ordering::Relaxed));
                    u.set_claude_up(telemetry.claude_up.load(Ordering::Relaxed));
                }
            }
        });

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// Strip markdown + SentenceSplitter + find_sentence_boundary live in
// `splitter.rs` now. Re-export the names the rest of main.rs still uses.
use crate::splitter::{strip_markdown, SentenceSplitter};

// -------------------- transcript helpers -----------------------------------

/// Push a single user-side block onto the transcript model. Must be
/// called from the UI thread (i.e. inside `invoke_from_event_loop`).
fn push_user_block(text: &str) {
    let ts = short_clock();
    let entry = TranscriptEntry {
        kind: "user".into(),
        body: text.into(),
        header_rank: 0,
        is_code: false,
        is_bullet: false,
        is_bold: false,
        timestamp: ts.into(),
        continues_prev: false,
    };
    let _ = with_transcript(|m| m.push(entry));
}

/// Render `text` through the markdown parser into 1+ jarvis-kind blocks
/// and append them. Each markdown block (header / bullet / paragraph /
/// code) becomes one row in the transcript. UI-thread only.
fn append_jarvis_blocks(text: &str) {
    let blocks = markdown::parse(text);
    let ts = short_clock();
    let n = blocks.len();
    let _ = with_transcript(|m| {
        for (i, b) in blocks.iter().enumerate() {
            m.push(TranscriptEntry {
                kind: "jarvis".into(),
                body: b.body.clone().into(),
                header_rank: b.header_rank,
                is_code: b.is_code,
                is_bullet: b.is_bullet,
                is_bold: b.is_bold,
                timestamp: if i + 1 == n {
                    ts.clone().into()
                } else {
                    "".into()
                },
                continues_prev: b.continues_prev,
            });
        }
    });
}

/// First streaming chunk pushes a placeholder; subsequent chunks
/// rewrite that row's body. UI-thread only.
fn update_streaming_jarvis(full_body: &str, already_streaming: bool) {
    let ts = short_clock();
    let _ = with_transcript(|m| {
        let entry = TranscriptEntry {
            kind: "jarvis".into(),
            body: full_body.into(),
            header_rank: 0,
            is_code: false,
            is_bullet: false,
            is_bold: false,
            timestamp: ts.into(),
            continues_prev: false,
        };
        if already_streaming {
            let n = m.row_count();
            if n > 0 {
                m.set_row_data(n - 1, entry);
                return;
            }
        }
        m.push(entry);
    });
}

/// On finalize, swap the streaming placeholder row for markdown-rendered
/// blocks; or just append fresh blocks if no streaming happened. UI-thread only.
fn replace_streaming_with_final(final_text: &str, was_streamed: bool) {
    if was_streamed {
        let _ = with_transcript(|m| {
            let n = m.row_count();
            if n > 0 {
                m.remove(n - 1);
            }
        });
    }
    append_jarvis_blocks(final_text);
}

/// Snapshot the on-screen transcript as a Markdown document. Each
/// row contributes one section; tool rows render as fenced code blocks
/// so they survive paste into Linear/Slack/etc. without re-flowing.
/// Used by the sidebar EXPORT button.
fn transcript_to_markdown(thread_id: Option<&str>) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(8 * 1024);
    let _ = writeln!(out, "# JARVIS conversation");
    if let Some(id) = thread_id {
        let _ = writeln!(out, "\n_Thread: `{id}`_\n");
    }
    let _ = with_transcript(|m| {
        for i in 0..m.row_count() {
            let Some(row) = m.row_data(i) else { continue };
            let body = row.body.to_string();
            let ts = row.timestamp.to_string();
            let kind = row.kind.to_string();
            match kind.as_str() {
                "user" => {
                    let _ = writeln!(out, "\n## User · {ts}\n\n{body}");
                }
                "jarvis" => {
                    let _ = writeln!(out, "\n## JARVIS · {ts}\n\n{body}");
                }
                "tool" => {
                    let _ = writeln!(out, "\n### tool · {ts}\n\n```\n{body}\n```");
                }
                "status" => {
                    let _ = writeln!(out, "\n> {body}");
                }
                _ => {
                    let _ = writeln!(out, "\n{body}");
                }
            }
        }
    });
    out
}

fn clear_transcript_rows() {
    let _ = with_transcript(|m| m.set_vec(Vec::new()));
}

// -------------------- sub-agents panel helpers ----------------------------
//
// All four functions must be called from the UI thread (via
// invoke_from_event_loop) since they mutate the VecModel<SubAgentRow>
// that's tied to the Slint render thread.

/// Append a new sub-agent row triggered by SubAgentStarted. Idempotent:
/// if a row with the same id already exists (rare: server sends a
/// duplicate started event), it's left alone — the UI shouldn't show
/// a duplicate.
/// Push a sub-agent card. Looked up by the FRIENDLY NAME so repeat
/// invocations of the same tool (e.g. 5 memory_search calls in one
/// session) all flow into the same "Memory Diver" card — turn_count
/// increments, the active call's id replaces the prior one, and the
/// card pulses back to "running" state. This is the client-side
/// expression of name-attributed context preservation: McKale can
/// see that "Memory Diver" has been on 5 dives this session instead
/// of seeing 5 anonymous task-id cards.
fn push_sub_agent_row(id: &str, tool_name: &str, _kind: &str) {
    let identity = agent_names::identify(tool_name);
    let (r, g, b) = identity.rgb;
    let _ = with_sub_agents(|m| {
        // Look up by friendly name (persistent identity), not id.
        for i in 0..m.row_count() {
            if let Some(mut row) = m.row_data(i) {
                if row.label.as_str() == identity.name {
                    row.id = id.into();
                    row.tool_name = tool_name.into();
                    row.status = "running".into();
                    row.state = "running".into();
                    row.done = false;
                    row.turn_count += 1;
                    m.set_row_data(i, row);
                    return;
                }
            }
        }
        // First time we've seen this named identity this session.
        m.push(SubAgentRow {
            id: id.into(),
            label: identity.name.into(),
            tool_name: tool_name.into(),
            category: identity.category.into(),
            status: "running".into(),
            state: "running".into(),
            done: false,
            turn_count: 1,
            tint_r: r as i32,
            tint_g: g as i32,
            tint_b: b as i32,
        });
    });
}

/// Update the most recent status text for a running sub-agent.
fn update_sub_agent_status(id: &str, message: &str) {
    let _ = with_sub_agents(|m| {
        for i in 0..m.row_count() {
            if let Some(mut row) = m.row_data(i) {
                if row.id.as_str() == id && !row.done {
                    row.status = message.into();
                    m.set_row_data(i, row);
                    return;
                }
            }
        }
    });
}

/// Mark a sub-agent row done with success/fail + summary. Renderer
/// switches the indicator dot from cyan to green/red.
fn mark_sub_agent_done(id: &str, success: bool, summary: &str) {
    let _ = with_sub_agents(|m| {
        for i in 0..m.row_count() {
            if let Some(mut row) = m.row_data(i) {
                if row.id.as_str() == id {
                    row.done = true;
                    row.state = if success { "ok".into() } else { "fail".into() };
                    row.status = if summary.is_empty() {
                        if success { "ok".into() } else { "failed".into() }
                    } else {
                        summary.into()
                    };
                    m.set_row_data(i, row);
                    return;
                }
            }
        }
    });
}

/// Remove a sub-agent row by id. Called after the auto-prune delay so
/// the panel stays scoped to in-flight + recently-completed work.
fn prune_sub_agent_row(id: &str) {
    let _ = with_sub_agents(|m| {
        for i in 0..m.row_count() {
            if let Some(row) = m.row_data(i) {
                if row.id.as_str() == id {
                    m.remove(i);
                    return;
                }
            }
        }
    });
}

/// Settings payload passed from the UI to the .env writer task.
// Settings persistence lives in `settings.rs` now. Re-export the names
// the on_save_settings callback uses.
use crate::settings::{write_settings_to_env, WriteSettings};

fn refresh_conversation_model(list: &gateway::ThreadListResponse) {
    let active = list.active_thread.clone().unwrap_or_default();
    let rows: Vec<ConversationRow> = list
        .threads
        .iter()
        .map(|t| {
            // Title: short id + turn count
            let short = t.id.chars().take(8).collect::<String>();
            let title = format!("Thread {}", short);
            let subtitle = format!(
                "{} turn{} • {}",
                t.turn_count,
                if t.turn_count == 1 { "" } else { "s" },
                short_date(&t.updated_at)
            );
            ConversationRow {
                id: t.id.clone().into(),
                title: title.into(),
                subtitle: subtitle.into(),
                is_active: t.id == active,
            }
        })
        .collect();
    let _ = with_conversations(|m| m.set_vec(rows));
}

fn short_date(rfc3339: &str) -> String {
    // Slice `T` to get the HH:MM. Cheap and good enough for sidebar UI.
    if let Some((_, rest)) = rfc3339.split_once('T') {
        rest.chars().take(5).collect()
    } else {
        rfc3339.chars().take(10).collect()
    }
}

/// Three semantic tones for tool rows. The body string carries them
/// embedded as a prefix glyph so the Slint side picks the colour based
/// on body.starts_with("→" | "✓" | "✗") — keeps the Slint model schema
/// stable (no new "tone" property column needed).
const TOOL_TONE_PENDING: u8 = 0;
const TOOL_TONE_OK: u8 = 1;
const TOOL_TONE_FAIL: u8 = 2;

fn push_tool_row(body: &str, _tone: u8) {
    let _ = with_transcript(|m| {
        m.push(TranscriptEntry {
            kind: "tool".into(),
            body: body.into(),
            header_rank: 0,
            is_code: false,
            is_bullet: false,
            is_bold: false,
            timestamp: short_clock().into(),
            continues_prev: false,
        });
    });
}

/// Walk the transcript backwards to find the most-recent tool row that
/// matches `name` (the prefix-matched `→ name`), update its body to
/// reflect completion. Used when ToolCompleted fires.
fn update_last_tool_completion(name: &str, success: bool) {
    let arrow = format!("→ {}", name);
    let glyph = if success { "✓" } else { "✗" };
    let _ = with_transcript(|m| {
        for idx in (0..m.row_count()).rev() {
            let row = m.row_data(idx).unwrap_or_default();
            if row.kind == "tool" && row.body.starts_with(arrow.as_str()) {
                let new_body = format!("{} {}", glyph, name);
                let timestamp = row.timestamp.clone();
                m.set_row_data(
                    idx,
                    TranscriptEntry {
                        kind: "tool".into(),
                        body: new_body.into(),
                        header_rank: 0,
                        is_code: false,
                        is_bullet: false,
                        is_bold: false,
                        timestamp,
                        continues_prev: false,
                    },
                );
                break;
            }
        }
    });
}

/// Append a preview snippet to the most-recent tool row. Aggressively
/// shortened — tool rows are status chrome, not content. Big multi-
/// line JSON dumps that filled the bubble are flattened to a single
/// line ≤100 chars so the row reads as e.g.
/// `✓ memory_search · 10 results`.
fn update_last_tool_preview(name: &str, preview: &str) {
    let _ = with_transcript(|m| {
        // Strip whitespace runs + newlines so the preview is one line.
        let one_line: String = preview
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let preview_trim = if one_line.chars().count() > 100 {
            let head: String = one_line.chars().take(100).collect();
            format!("{}…", head)
        } else {
            one_line
        };
        for idx in (0..m.row_count()).rev() {
            let row = m.row_data(idx).unwrap_or_default();
            if row.kind == "tool"
                && (row.body.contains(name) || row.body.starts_with("→ "))
            {
                let new_body = if preview_trim.is_empty() {
                    row.body.to_string()
                } else {
                    format!("{} · {}", row.body, preview_trim)
                };
                let timestamp = row.timestamp.clone();
                m.set_row_data(
                    idx,
                    TranscriptEntry {
                        kind: "tool".into(),
                        body: new_body.into(),
                        header_rank: 0,
                        is_code: false,
                        is_bullet: false,
                        is_bold: false,
                        timestamp,
                        continues_prev: false,
                    },
                );
                break;
            }
        }
    });
}

fn short_clock() -> String {
    // Minimal HH:MM without pulling chrono. SystemTime → local
    // time-of-day via a quick mod-86400.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Local offset isn't tracked here — for an HH:MM clock that's fine
    // in most use, but a proper local-time pass is in the deferred
    // wizard-port milestone.
    let secs_of_day = secs % 86400;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    format!("{:02}:{:02}", h, m)
}

fn truncate_for_toast(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let head: String = chars.iter().take(max).collect();
    format!("{}…", head)
}

// -------------------- native Windows toast notification --------------------

// `notify` and `hotkeys::parse_key_env` live in their own modules now.
use crate::hotkeys::parse_key_env;

// Icon rasterization (build_tray_icon / build_window_icon_image /
// build_reactor_image / rasterize_jarvis_icon) lives in `icon.rs`.
// Window operations (bring_to_focus / start_window_drag) live in
// `window.rs`. Both are imported via `use crate::*` below.
use crate::icon::{
    build_reactor_image, build_tray_icon, build_window_icon_image, rasterize_jarvis_icon,
};
use crate::window::{bring_to_focus, start_window_drag};

