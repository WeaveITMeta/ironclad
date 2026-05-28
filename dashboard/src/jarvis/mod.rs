//! JARVIS HUD shell. Mounts the ARC reactor, the chat transcript, and the
//! telemetry strip. Owns the always-on voice loop: continuous mic with VAD,
//! auto-send on sentence end, TTS playback, mic suspension during JARVIS
//! speaking so we don't echo back into the STT.

mod api;
mod arc;
mod pip;
mod screen;
mod sse;
mod streaming;
mod voice;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlInputElement;

use api::{
    events_url, fetch_streaming_config, fetch_token, fetch_voice_status, post_approval,
    post_stt, post_tts_stream, send_chat, send_chat_with_images,
};
use pip::{install_auto_pip, pin_pip, unpin_pip};
use screen::{
    grab_stream_frame, is_stream_active, start_screen_stream, stop_screen_stream,
};
use streaming::{SttEvent, StreamingStt};
use arc::{ArcReactor, ArcState};
use sse::{ChatEvent, ChatStream, subscribe};
use voice::{AudioQueue, ContinuousMic, MicState, SentenceSplitter, VadConfig};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug)]
struct Message {
    role: Role,
    content: String,
}

/// A tool-approval request from the agent. The user can speak "yes" / "no"
/// or click Approve / Always / Deny. Until one of those resolves it, the
/// agent loop blocks.
#[derive(Clone, Debug)]
struct ApprovalRequest {
    request_id: String,
    tool_name: String,
    description: String,
    parameters: String,
}

/// Match an STT transcript against a spoken-stop intent. Returns true only
/// when the user said a stop word AS THE ENTIRE UTTERANCE (no other
/// content). That conservative match avoids false positives like "stop, I
/// have a different idea" cutting JARVIS off mid-thought when the user
/// actually wanted to redirect, not silence.
///
/// Matches: "stop", "halt", "silence", "enough", "quiet", "shutup".
/// Punctuation and case are normalized away first.
fn parse_stop_intent(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let collapsed: String = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        collapsed.as_str(),
        "stop" | "halt" | "silence" | "enough" | "quiet" | "shutup" | "shut up"
    )
}

/// Match an STT transcript against approve / deny keywords. Returns the
/// action string ("approve" | "always" | "deny") or None if the user said
/// something unrelated — in which case we leave the approval pending and
/// the utterance is just dropped (so we don't accidentally chat-send "yes"
/// to the agent as a new turn).
fn parse_approval_intent(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.iter().any(|w| matches!(*w, "always")) {
        return Some("always");
    }
    let yes_set = [
        "yes", "yeah", "yep", "yup", "sure", "approve", "approved",
        "ok", "okay", "alright", "affirmative", "confirm", "confirmed",
        "go", "proceed", "do", "y",
    ];
    let no_set = [
        "no", "nope", "nah", "deny", "denied", "cancel", "stop",
        "abort", "negative", "n",
    ];
    if words.iter().any(|w| yes_set.contains(w)) {
        return Some("approve");
    }
    if words.iter().any(|w| no_set.contains(w)) {
        return Some("deny");
    }
    None
}

#[component]
pub fn JarvisShell() -> impl IntoView {
    let token = RwSignal::new(String::new());
    let token_ready = RwSignal::new(false);
    let arc_state = RwSignal::new(ArcState::Idle);
    let status_line = RwSignal::new(String::from("booting"));
    let messages: RwSignal<Vec<Message>> = RwSignal::new(Vec::new());
    // In-progress assistant text — fills as `stream_chunk` SSE events land,
    // gets cleared when the final `response` event lands and the assembled
    // text is pushed into `messages`.
    let streaming_buffer: RwSignal<Option<String>> = RwSignal::new(None);
    let tts_enabled = RwSignal::new(true);
    let stt_ready = RwSignal::new(false);
    let tts_ready = RwSignal::new(false);
    let muted = RwSignal::new(false);
    // Flips true after the first successful ContinuousMic::start so the
    // placeholder text + status line can stop nagging the user to tap.
    let mic_initialized = RwSignal::new(false);
    // Screen-capture buffer. When the user clicks "look", a base64 PNG of
    // the captured frame lands here; the very next chat send attaches it
    // and clears the signal. One-shot, never persisted across turns.
    let pending_screenshot: RwSignal<Option<String>> = RwSignal::new(None);
    // Ambient screen-share state. When `screen_streaming` is true, every
    // chat send (typed or voice) auto-grabs a fresh frame off the live
    // stream and attaches it as an image content block. Lets JARVIS see
    // what McKale is looking at without re-prompting on each utterance.
    let screen_streaming = RwSignal::new(false);
    // PiP pin state for background-tab voice. When true, a Picture-in-
    // Picture floating window is open, keeping the dashboard tab's audio
    // pipeline alive even when the user is on another tab/window.
    let pip_pinned = RwSignal::new(false);

    // The continuous mic owns the AudioContext and stream; we keep it in an
    // Rc<RefCell> so closures can flip mute/speaking flags on it.
    let mic_handle: Rc<RefCell<Option<ContinuousMic>>> = Rc::new(RefCell::new(None));
    let mic_state: Rc<RefCell<MicState>> = Rc::new(RefCell::new(MicState::Muted));
    let sse_handle: Rc<RefCell<Option<ChatStream>>> = Rc::new(RefCell::new(None));
    // Anchor the SSE ChatStream to the component's reactive owner. Without
    // this, the only Rc<ChatStream> lives inside the outer `spawn_local`
    // below; that future completes ~50ms after page load, dropping the
    // EventSource and silently breaking the dashboard (all subsequent
    // stream_chunk / response events arrive at the gateway with no
    // subscriber). Effect::new captures the clone for the component lifetime.
    {
        let sse_anchor = Rc::clone(&sse_handle);
        Effect::new(move |_| {
            let _ = &sse_anchor;
        });
    }
    // Streaming TTS plumbing: a sentence splitter consumes stream_chunk text
    // and emits each complete sentence; an audio queue chains the per-sentence
    // wav buffers gaplessly through WebAudio.
    let splitter: Rc<RefCell<SentenceSplitter>> =
        Rc::new(RefCell::new(SentenceSplitter::new()));
    let audio_queue: Rc<RefCell<Option<AudioQueue>>> = Rc::new(RefCell::new(None));

    // Serialize TTS. `queue_sentence` used to `spawn_local` a fresh POST per
    // sentence, all racing in parallel against ElevenLabs. Their PCM chunks
    // landed on the shared AudioQueue in arrival order, scrambling audio
    // between sentences and silently dropping under any rate-limit. Instead:
    // every sentence pushes onto `tts_queue`. A single worker drains it
    // one synth at a time. Audio plays in the order sentences were emitted.
    let tts_queue: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));
    let tts_worker_running: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    {
        let q = Rc::clone(&tts_queue);
        let r = Rc::clone(&tts_worker_running);
        Effect::new(move |_| {
            let _ = (&q, &r);
        });
    }

    // Streaming STT (Parakeet over WebTransport). Opened lazily after the
    // gateway hands us the cert hash; if it never opens, `attach_audio_worklet`
    // is a no-op and we fall back to the legacy post-VAD wav POST.
    let streaming_stt: Rc<RefCell<Option<StreamingStt>>> = Rc::new(RefCell::new(None));
    // Live interim transcript shown in the in-progress row while the user is
    // mid-sentence. Replaces itself each time Parakeet emits a better guess.
    let interim_stt: RwSignal<Option<String>> = RwSignal::new(None);

    // Pending tool-approval request from the agent. When Some, the next
    // utterance is intercepted as a yes/no answer instead of being sent as
    // a chat message, and a banner is rendered with Approve / Always / Deny
    // buttons. When None, the normal chat-send path runs.
    let pending_approval: RwSignal<Option<ApprovalRequest>> = RwSignal::new(None);

    // Bootstrap: fetch token, voice status, subscribe to SSE, start mic.
    let sse_for_init = Rc::clone(&sse_handle);
    let mic_for_init = Rc::clone(&mic_handle);
    let splitter_for_init = Rc::clone(&splitter);
    let audio_for_init = Rc::clone(&audio_queue);
    let streaming_for_init = Rc::clone(&streaming_stt);
    let tts_queue_for_init = Rc::clone(&tts_queue);
    let tts_worker_running_for_init = Rc::clone(&tts_worker_running);
    // Separate clones for the STT-bootstrap inner spawn_local so its
    // utterance handler can trigger spoken-stop (clear tts queue + halt
    // audio + return mic to idle).
    let audio_for_stt = Rc::clone(&audio_queue);
    let tts_queue_for_stt = Rc::clone(&tts_queue);
    let mic_for_stt = Rc::clone(&mic_handle);
    spawn_local(async move {
        match fetch_token().await {
            Ok(t) => {
                web_sys::console::log_1(&"[jarvis] got gateway token".into());
                token.set(t.clone());
                token_ready.set(true);
                let splitter_for_sse = splitter_for_init;
                let audio_for_sse = audio_for_init;
                let tts_queue_for_sse = tts_queue_for_init;
                let tts_worker_running_for_sse = tts_worker_running_for_init;
                match subscribe(&events_url(&t), move |ev| {
                    handle_chat_event(
                        ev,
                        messages,
                        streaming_buffer,
                        arc_state,
                        status_line,
                        tts_enabled,
                        tts_ready,
                        token,
                        pending_approval,
                        Rc::clone(&mic_for_init),
                        Rc::clone(&splitter_for_sse),
                        Rc::clone(&audio_for_sse),
                        Rc::clone(&tts_queue_for_sse),
                        Rc::clone(&tts_worker_running_for_sse),
                    );
                }) {
                    Ok(stream) => {
                        *sse_for_init.borrow_mut() = Some(stream);
                    }
                    Err(e) => {
                        web_sys::console::log_1(
                            &format!("[jarvis] SSE subscribe failed: {e}").into(),
                        );
                        status_line.set(format!("sse failed: {e}"));
                        arc_state.set(ArcState::Error);
                    }
                }
                status_line.set(String::from("ready"));
            }
            Err(e) => {
                status_line.set(format!("token fetch failed: {e}"));
                arc_state.set(ArcState::Error);
            }
        }
        if let Ok(vs) = fetch_voice_status().await {
            stt_ready.set(vs.stt_ready);
            tts_ready.set(vs.tts_ready);
        }

        // Streaming STT bootstrap. The cert hash is empty until the WT
        // sidecar finishes loading Parakeet (~20s on first run), so we
        // retry a few times before giving up. Once connected, the on_event
        // callback updates interim text (in-progress row) and finalizes
        // utterances by pushing them as user messages + dispatching chat.
        spawn_local(async move {
            for attempt in 1..=10 {
                let cfg = match fetch_streaming_config().await {
                    Ok(c) => c,
                    Err(e) => {
                        web_sys::console::log_1(
                            &format!("[stream-stt] config fetch failed (try {attempt}): {e}").into(),
                        );
                        await_ms(1500).await;
                        continue;
                    }
                };
                if cfg.cert_sha256.is_empty() {
                    web_sys::console::log_1(
                        &format!(
                            "[stream-stt] WT sidecar not ready yet (try {attempt}); waiting"
                        )
                        .into(),
                    );
                    await_ms(2000).await;
                    continue;
                }
                let token_for_send = token.get_untracked();
                let audio_for_cb = Rc::clone(&audio_for_stt);
                let tts_queue_for_cb = Rc::clone(&tts_queue_for_stt);
                let mic_for_cb = Rc::clone(&mic_for_stt);
                match StreamingStt::connect(&cfg.url, &cfg.cert_sha256, move |ev| {
                    handle_stt_event(
                        ev,
                        interim_stt,
                        messages,
                        arc_state,
                        status_line,
                        token_for_send.clone(),
                        pending_approval,
                        Rc::clone(&audio_for_cb),
                        Rc::clone(&tts_queue_for_cb),
                        Rc::clone(&mic_for_cb),
                        pending_screenshot,
                    );
                })
                .await
                {
                    Ok(s) => {
                        web_sys::console::log_1(&"[stream-stt] connected".into());
                        *streaming_for_init.borrow_mut() = Some(s);
                        break;
                    }
                    Err(e) => {
                        web_sys::console::log_1(
                            &format!("[stream-stt] connect failed (try {attempt}): {e}").into(),
                        );
                        await_ms(2000).await;
                    }
                }
            }
        });
    });

    // Mic boot is gated on the user clicking the reactor or the mute toggle
    // — browsers require a user gesture before getUserMedia resolves.
    let start_mic = {
        let mic_handle = Rc::clone(&mic_handle);
        let mic_state = Rc::clone(&mic_state);
        let audio_queue_outer = Rc::clone(&audio_queue);
        let streaming_outer = Rc::clone(&streaming_stt);
        move || {
            if mic_handle.borrow().is_some() {
                // Already running; just unmute.
                if let Some(mic) = mic_handle.borrow().as_ref() {
                    mic.set_muted(false);
                }
                muted.set(false);
                return;
            }
            arc_state.set(ArcState::Listening);
            status_line.set(String::from("opening mic..."));
            // Pre-create the AudioQueue synchronously inside the user-gesture
            // window. If we wait until queue_sentence (which runs in spawn_local)
            // Chrome will create the AudioContext in `suspended` state and
            // nothing ever plays. This forces it to be born `running`.
            if audio_queue_outer.borrow().is_none() {
                match AudioQueue::new() {
                    Ok(q) => *audio_queue_outer.borrow_mut() = Some(q),
                    Err(e) => web_sys::console::log_1(
                        &format!("[jarvis-tts] eager AudioQueue init failed: {e}").into(),
                    ),
                }
            }
            let mic_handle = Rc::clone(&mic_handle);
            let mic_state = Rc::clone(&mic_state);
            let audio_queue = Rc::clone(&audio_queue_outer);
            let streaming_for_spawn = Rc::clone(&streaming_outer);
            spawn_local(async move {
                let token_owned = token.get_untracked();
                let mic_handle_for_utt = Rc::clone(&mic_handle);
                let streaming_for_utt = Rc::clone(&streaming_for_spawn);
                let audio_queue_for_utt = Rc::clone(&audio_queue);
                let on_utterance = move |wav: Vec<u8>| {
                    web_sys::console::log_1(
                        &format!("[jarvis-vad] endpoint hit ({} bytes wav available)", wav.len())
                            .into(),
                    );
                    // Silero just accepted this clip as real user speech.
                    // If a prior on_barge_in had soft-halted the queue with
                    // pending chunks buffered, those chunks belong to the
                    // ABOUT-TO-BE-CANCELLED response. Wipe them so the next
                    // response's TTS worker doesn't accidentally drain
                    // stale audio from the prior turn.
                    if let Some(q) = audio_queue_for_utt.borrow_mut().as_mut() {
                        q.halt_immediately();
                    }
                    arc_state.set(ArcState::Thinking);
                    // Streaming path: tell Parakeet to flush + send the final
                    // transcript over its outbound event stream. We check
                    // `is_alive()` because a WT connection that handshakes
                    // OK at boot can die mid-session (server idle timeout,
                    // network blip) — without that check, finalize() writes
                    // to a dead control stream silently and the user never
                    // gets a transcript or reply. When dead, drop into the
                    // legacy POST fallback below.
                    let stream_alive = streaming_for_utt
                        .borrow()
                        .as_ref()
                        .map(|s| s.is_alive())
                        .unwrap_or(false);
                    if stream_alive {
                        if let Some(s) = streaming_for_utt.borrow().as_ref() {
                            web_sys::console::log_1(&"[stream-stt] sending finalize".into());
                            status_line.set(String::from("finalizing transcript..."));
                            s.finalize();
                            if let Some(mic) = mic_handle_for_utt.borrow().as_ref() {
                                mic.return_to_idle();
                            }
                            return;
                        }
                    } else if streaming_for_utt.borrow().is_some() {
                        web_sys::console::log_1(
                            &"[stream-stt] WT is dead; falling back to legacy POST".into(),
                        );
                    }
                    // Legacy fallback: POST the wav.
                    status_line.set(String::from("transcribing..."));
                    let token = token_owned.clone();
                    let mic_for_stt = Rc::clone(&mic_handle_for_utt);
                    spawn_local(async move {
                        match post_stt(&token, &wav).await {
                            Ok(text) => {
                                if let Some(mic) = mic_for_stt.borrow().as_ref() {
                                    mic.return_to_idle();
                                }
                                let trimmed = text.trim().to_string();
                                if trimmed.is_empty() {
                                    arc_state.set(ArcState::Idle);
                                    status_line.set(String::from("(no speech recognized)"));
                                    return;
                                }
                                messages.update(|m| {
                                    m.push(Message {
                                        role: Role::User,
                                        content: trimmed.clone(),
                                    });
                                });
                                let imgs = collect_outgoing_images(pending_screenshot);
                                if let Err(e) = send_chat_with_images(
                                    &token, &trimmed, imgs,
                                ).await {
                                    status_line.set(format!("send failed: {e}"));
                                    arc_state.set(ArcState::Error);
                                } else {
                                    status_line.set(String::from("waiting on JARVIS..."));
                                }
                            }
                            Err(e) => {
                                if let Some(mic) = mic_for_stt.borrow().as_ref() {
                                    mic.return_to_idle();
                                }
                                status_line.set(format!("STT failed: {e}"));
                                arc_state.set(ArcState::Error);
                            }
                        }
                    });
                };
                // Barge-in: the user started speaking while JARVIS was
                // mid-response. Halt the audio queue at the next sentence
                // boundary so we don't cut him off mid-word. The new
                // utterance keeps accumulating; when VAD endpoint fires,
                // it gets sent through the chat path as the next turn.
                // The partial assistant text already lives in the transcript
                // (and in the conversation history server-side) so Claude
                // can weigh both when responding.
                let on_barge_in = move || {
                    // DO NOT halt the audio queue here. Earlier versions
                    // halted on every RMS spike, but phantom barge-ins
                    // from echo / ambient noise / JARVIS's own voice
                    // leaking through the mic would leave the queue
                    // halted and unable to recover (no `endpoint hit`
                    // fires when state oscillates back to Speaking, so
                    // `on_silero_reject` never gets the chance to undo
                    // the halt). JARVIS goes silent forever.
                    //
                    // The real halt now lives in `on_utterance` where
                    // it fires ONLY after Silero has confirmed the audio
                    // is genuine speech. That delays cut-off by ~1–2s
                    // (the Silero scoring window) but is the only design
                    // that survives phantom triggers.
                    //
                    // We still flip the visible state so the UI shows
                    // "interrupted" while we wait for Silero's verdict.
                    web_sys::console::log_1(
                        &"[jarvis-vad] barge-in noted (audio keeps playing until Silero verdict)"
                            .into(),
                    );
                    arc_state.set(ArcState::Listening);
                    status_line.set(String::from("listening (over JARVIS)"));
                };
                // If Silero later decides the utterance was music/noise, undo
                // the barge-in halt so JARVIS's queued sentences can play.
                // Without this, a phantom barge-in from background music
                // silences the rest of JARVIS's response.
                let audio_queue_for_reject = Rc::clone(&audio_queue);
                let on_silero_reject = move || {
                    if let Some(q) = audio_queue_for_reject.borrow_mut().as_mut() {
                        q.resume();
                    }
                    arc_state.set(ArcState::Speaking);
                    status_line.set(String::from("speaking..."));
                };
                match ContinuousMic::start(
                    VadConfig::default(),
                    on_utterance,
                    on_barge_in,
                    on_silero_reject,
                    Rc::clone(&mic_state),
                )
                .await
                {
                    Ok(mic) => {
                        web_sys::console::log_1(&"[jarvis-vad] mic open, always-on".into());
                        // AudioWorklet pump: fan the mic source out to a
                        // worklet node that emits 80ms PCM chunks at 16kHz.
                        // Each chunk goes straight into the Parakeet WT
                        // datagram channel (if connected). The legacy POST
                        // path stays as fallback for the case where WT never
                        // came up.
                        let streaming_for_worklet = Rc::clone(&streaming_for_spawn);
                        let mic_state_for_worklet = Rc::clone(&mic_state);
                        if let Err(e) = mic
                            .attach_audio_worklet(
                                "/audio-worklet.js",
                                move |chunk: Vec<f32>| {
                                    // Only stream while the user is actually
                                    // mid-utterance — avoids transcribing
                                    // background noise between turns.
                                    let st = *mic_state_for_worklet.borrow();
                                    if !matches!(st, MicState::Listening) {
                                        return;
                                    }
                                    if let Some(s) =
                                        streaming_for_worklet.borrow().as_ref()
                                    {
                                        let _ = s.send_audio(&chunk);
                                    }
                                },
                            )
                            .await
                        {
                            web_sys::console::log_1(
                                &format!("[jarvis-mic] worklet attach failed: {e}").into(),
                            );
                        }
                        *mic_handle.borrow_mut() = Some(mic);
                        arc_state.set(ArcState::Idle);
                        status_line.set(String::from("listening"));
                        muted.set(false);
                        mic_initialized.set(true);
                        // Mic open counts as the user gesture that Chrome
                        // requires to allow subsequent PiP requests from
                        // non-gesture handlers like visibilitychange. Install
                        // the auto-PiP listener now so the next time McKale
                        // switches tabs / minimizes, voice stays alive without
                        // him having to click anything.
                        install_auto_pip(move |pinned| {
                            pip_pinned.set(pinned);
                        });
                    }
                    Err(e) => {
                        web_sys::console::log_1(
                            &format!("[jarvis-vad] failed to open mic: {e}").into(),
                        );
                        status_line.set(format!("mic failed: {e}"));
                        arc_state.set(ArcState::Error);
                    }
                }
            });
        }
    };

    let toggle_mute = {
        let mic_handle = Rc::clone(&mic_handle);
        let start_mic = start_mic.clone();
        move || {
            let new_muted = !muted.get();
            muted.set(new_muted);
            if new_muted {
                if let Some(mic) = mic_handle.borrow().as_ref() {
                    mic.set_muted(true);
                }
                arc_state.set(ArcState::Idle);
                status_line.set(String::from("muted"));
            } else if mic_handle.borrow().is_some() {
                if let Some(mic) = mic_handle.borrow().as_ref() {
                    mic.set_muted(false);
                }
                arc_state.set(ArcState::Idle);
                status_line.set(String::from("listening"));
            } else {
                // First unmute: actually open the mic.
                start_mic();
            }
        }
    };

    let send_typed: Callback<String> = Callback::new(move |content: String| {
        if content.trim().is_empty() {
            return;
        }
        let token_val = token.get();
        messages.update(|m| {
            m.push(Message {
                role: Role::User,
                content: content.clone(),
            });
        });
        arc_state.set(ArcState::Thinking);
        status_line.set(String::from("waiting on JARVIS..."));
        let imgs = collect_outgoing_images(pending_screenshot);
        spawn_local(async move {
            if let Err(e) = send_chat_with_images(
                &token_val, &content, imgs,
            ).await {
                status_line.set(format!("send failed: {e}"));
                arc_state.set(ArcState::Error);
            }
        });
    });

    // Reactor click = start mic (first time) OR toggle mute (every time after).
    let reactor_click = {
        let start_mic = start_mic.clone();
        let toggle_mute = toggle_mute.clone();
        let mic_handle = Rc::clone(&mic_handle);
        move || {
            if mic_handle.borrow().is_none() {
                start_mic();
            } else {
                toggle_mute();
            }
        }
    };

    view! {
        <div class="min-h-screen bg-hud-bg text-arc-100 font-sans relative overflow-hidden">
            // Background scan-line + grid
            <div class="pointer-events-none absolute inset-0 opacity-30"
                 style="background-image:
                    linear-gradient(to right, rgba(34,211,238,0.06) 1px, transparent 1px),
                    linear-gradient(to bottom, rgba(34,211,238,0.06) 1px, transparent 1px);
                    background-size: 48px 48px;"></div>
            <div class="pointer-events-none absolute inset-0">
                <div class="w-full h-px bg-gradient-to-r from-transparent via-arc-400/60 to-transparent absolute animate-scan"></div>
            </div>

            <JarvisHeader
                status_line
                tts_enabled
                tts_ready
                muted
                on_toggle_mute={
                    let toggle_mute = toggle_mute.clone();
                    move || toggle_mute()
                }
            />

            <main class="relative max-w-5xl mx-auto px-6 py-8 space-y-8">
                // Centerpiece — arc reactor flanked by Stark-style round
                // toggles: pin-to-bg on the LEFT, share-live on the RIGHT.
                // The "look once" one-shot screenshot button was retired in
                // favor of the always-on live-share toggle.
                <section class="flex items-center justify-center gap-12 pt-8 pb-12">
                    <ArcOrbButton
                        active=pip_pinned
                        on_click=Callback::new(move |_| {
                            spawn_local(async move {
                                if pip_pinned.get_untracked() {
                                    let _ = unpin_pip().await;
                                    pip_pinned.set(false);
                                } else {
                                    match pin_pip().await {
                                        Ok(()) => pip_pinned.set(true),
                                        Err(e) => web_sys::console::log_1(
                                            &format!("[pip] pin failed: {e}").into(),
                                        ),
                                    }
                                }
                            });
                        })
                        title="Pin a floating PiP window so voice + capture survive the tab going to background"
                    >
                        <IconPin />
                    </ArcOrbButton>
                    <ArcReactor
                        state=Signal::derive(move || arc_state.get())
                        on_click=move || reactor_click()
                    />
                    <ArcOrbButton
                        active=screen_streaming
                        on_click=Callback::new(move |_| {
                            spawn_local(async move {
                                if screen_streaming.get_untracked() || is_stream_active() {
                                    stop_screen_stream();
                                    screen_streaming.set(false);
                                    status_line.set(String::from("screen share stopped"));
                                } else {
                                    status_line.set(String::from("waiting for screen pick..."));
                                    match start_screen_stream().await {
                                        Ok(()) => {
                                            screen_streaming.set(true);
                                            status_line.set(String::from(
                                                "screen sharing live — JARVIS sees a fresh frame every voice turn",
                                            ));
                                        }
                                        Err(e) => {
                                            web_sys::console::log_1(
                                                &format!("[screen] live start failed: {e}").into(),
                                            );
                                            status_line.set(format!("screen share: {e}"));
                                        }
                                    }
                                }
                            });
                        })
                        title="Share screen continuously: every voice turn auto-attaches a fresh frame"
                    >
                        <IconLive />
                    </ArcOrbButton>
                </section>

                <ApprovalBanner pending_approval token />

                // Transcript + composer
                <ChatPanel
                    messages
                    streaming_buffer
                    interim_stt
                    arc_state
                    status_line
                    mic_initialized
                    muted
                    on_send=send_typed
                />

                <TelemetryStrip stt_ready tts_ready token_ready muted />
            </main>
        </div>
    }
}

/// Spawn a background task that POSTs the given sentence to `/api/voice/tts`
/// and enqueues the returned wav into the audio queue (gapless playback).
/// Creates the AudioQueue lazily on first use — the dashboard can run muted
/// or text-only without ever creating an AudioContext.
#[allow(clippy::too_many_arguments)]
fn queue_sentence(
    sentence: String,
    tts_queue: &Rc<RefCell<VecDeque<String>>>,
    tts_worker_running: &Rc<RefCell<bool>>,
    audio_queue: &Rc<RefCell<Option<AudioQueue>>>,
    mic_handle: &Rc<RefCell<Option<ContinuousMic>>>,
    token: String,
) {
    let trimmed = sentence.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    // Strip markdown noise before TTS. The transcript still shows the raw
    // markdown; only what gets read aloud is normalized.
    let speakable = voice::strip_markdown_for_tts(&trimmed);
    if speakable.is_empty() {
        return;
    }
    tts_queue.borrow_mut().push_back(speakable);
    if *tts_worker_running.borrow() {
        // A worker is already draining the queue; the new sentence will
        // be picked up on the next loop iteration.
        return;
    }
    *tts_worker_running.borrow_mut() = true;

    let queue = Rc::clone(tts_queue);
    let running = Rc::clone(tts_worker_running);
    let aq_handle = Rc::clone(audio_queue);
    let mic_handle_outer = Rc::clone(mic_handle);
    spawn_local(async move {
        // Lazy-init the AudioQueue once. AudioContext can only be
        // constructed from a user-gesture, but by the time TTS sentences
        // start flowing, start_mic has already done that.
        if aq_handle.borrow().is_none() {
            match AudioQueue::new() {
                Ok(q) => *aq_handle.borrow_mut() = Some(q),
                Err(e) => {
                    web_sys::console::log_1(
                        &format!("[jarvis-tts] AudioQueue init failed: {e}").into(),
                    );
                    queue.borrow_mut().clear();
                    *running.borrow_mut() = false;
                    return;
                }
            }
        }
        // If a prior barge-in halted the queue, every subsequent enqueue
        // gets silently dropped (halted=true). Every new JARVIS turn is
        // something the user intends to hear, so resume once before this
        // worker run.
        if let Some(q) = aq_handle.borrow_mut().as_mut() {
            q.resume();
        }

        loop {
            let next = queue.borrow_mut().pop_front();
            let Some(sentence) = next else {
                break;
            };
            web_sys::console::log_1(
                &format!("[jarvis-tts] sentence: {}", sentence).into(),
            );

            let aq_for_chunks = Rc::clone(&aq_handle);
            let mic_for_chunks = Rc::clone(&mic_handle_outer);
            let result = post_tts_stream(&token, &sentence, move |sr, samples| {
                let mut q = aq_for_chunks.borrow_mut();
                if let Some(q) = q.as_mut() {
                    if let Err(e) = q.enqueue_pcm(&samples, sr) {
                        web_sys::console::log_1(
                            &format!("[jarvis-tts] enqueue_pcm failed: {e}").into(),
                        );
                    }
                }
                drop(q);
                if let Some(mic) = mic_for_chunks.borrow().as_ref() {
                    mic.set_speaking(true);
                }
            })
            .await;
            if let Err(e) = result {
                web_sys::console::log_1(
                    &format!("[jarvis-tts] streaming POST failed: {e}").into(),
                );
                // Don't break the worker — try the next sentence. If the
                // whole gateway is down, every call will fail and the
                // queue drains harmlessly.
            }

            // Inter-sentence breath. ElevenLabs gives us a clean cut at
            // every period — JARVIS sounds like a robot stitching files
            // back-to-back without this. 220ms is roughly a natural
            // English speaker's between-sentence pause; long enough to
            // hear, short enough not to drag.
            if !queue.borrow().is_empty() {
                if let Some(q) = aq_handle.borrow_mut().as_mut() {
                    q.add_gap(0.22);
                }
            }
        }

        *running.borrow_mut() = false;
    });
}

/// Simple async sleep via setTimeout + Promise. We only use it in the
/// streaming-STT bootstrap retry loop; not worth pulling in gloo-timers'
/// future crate just for this.
async fn await_ms(ms: i32) {
    use wasm_bindgen_futures::JsFuture;
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let _ = web_sys::window()
            .and_then(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(
                &resolve, ms,
            ).ok());
    });
    let _ = JsFuture::from(promise).await;
}

/// Handle one event from the streaming Parakeet WebTransport session.
///
/// Interim → updates the in-progress transcript row so the user sees text
/// appear while they're talking.
///
/// Final → the canonical user turn. We push it to the transcript, dispatch
/// it to the chat endpoint, and clear the interim row.
#[allow(clippy::too_many_arguments)]
fn handle_stt_event(
    ev: SttEvent,
    interim_stt: RwSignal<Option<String>>,
    messages: RwSignal<Vec<Message>>,
    arc_state: RwSignal<ArcState>,
    status_line: RwSignal<String>,
    token: String,
    pending_approval: RwSignal<Option<ApprovalRequest>>,
    audio_queue: Rc<RefCell<Option<AudioQueue>>>,
    tts_queue: Rc<RefCell<VecDeque<String>>>,
    mic_handle: Rc<RefCell<Option<ContinuousMic>>>,
    pending_screenshot: RwSignal<Option<String>>,
) {
    match ev {
        SttEvent::Interim(text) => {
            // Only show interim text while the mic is actually listening or
            // we'd surface stale interims from background noise between turns.
            if matches!(arc_state.get_untracked(), ArcState::Listening | ArcState::Thinking) {
                interim_stt.set(Some(text));
            }
        }
        SttEvent::Final(text) => {
            interim_stt.set(None);
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                web_sys::console::log_1(&"[stream-stt] empty final, skipping".into());
                arc_state.set(ArcState::Idle);
                return;
            }
            web_sys::console::log_1(&format!("[stream-stt] FINAL: {}", trimmed).into());

            // Spoken stop. If the user said only a stop word (and no other
            // content), kill audio right now, drop any sentences queued
            // for synth, and drop the transcript so it doesn't get sent
            // as a chat message. Does not run while a tool-approval is
            // pending — in that mode "stop"/"halt" are interpreted as
            // "deny" by parse_approval_intent below.
            if pending_approval.get_untracked().is_none() && parse_stop_intent(&trimmed) {
                web_sys::console::log_1(&"[jarvis-stop] spoken stop".into());
                tts_queue.borrow_mut().clear();
                if let Some(q) = audio_queue.borrow_mut().as_mut() {
                    q.halt_immediately();
                    q.resume();
                }
                if let Some(mic) = mic_handle.borrow().as_ref() {
                    mic.return_to_idle();
                }
                status_line.set(String::from("stopped"));
                arc_state.set(ArcState::Idle);
                return;
            }

            // If the agent is waiting for an approval decision, intercept
            // the utterance as a yes/no answer rather than sending it as a
            // new chat turn. Anything not matching yes/no keywords is
            // logged and dropped — the approval stays pending and the user
            // can answer again or click a button.
            if let Some(req) = pending_approval.get_untracked() {
                match parse_approval_intent(&trimmed) {
                    Some(action) => {
                        web_sys::console::log_1(
                            &format!(
                                "[approval] voice {action} for '{}' (request_id={})",
                                req.tool_name, req.request_id
                            )
                            .into(),
                        );
                        messages.update(|m| {
                            m.push(Message {
                                role: Role::User,
                                content: format!("[approval: {}]", action),
                            });
                        });
                        pending_approval.set(None);
                        status_line.set(format!("approval: {action}"));
                        arc_state.set(ArcState::Thinking);
                        let token = token.clone();
                        let req_id = req.request_id.clone();
                        let action_str = action.to_string();
                        spawn_local(async move {
                            if let Err(e) =
                                post_approval(&token, &req_id, &action_str).await
                            {
                                web_sys::console::log_1(
                                    &format!("[approval] POST failed: {e}").into(),
                                );
                            }
                        });
                        return;
                    }
                    None => {
                        web_sys::console::log_1(
                            &format!(
                                "[approval] heard '{trimmed}' but no yes/no \
                                 keyword; approval still pending"
                            )
                            .into(),
                        );
                        status_line
                            .set(String::from("waiting for approval (say yes or no)"));
                        return;
                    }
                }
            }

            messages.update(|m| {
                m.push(Message {
                    role: Role::User,
                    content: trimmed.clone(),
                });
            });
            arc_state.set(ArcState::Thinking);
            status_line.set(String::from("waiting on JARVIS..."));
            let imgs = collect_outgoing_images(pending_screenshot);
            spawn_local(async move {
                if let Err(e) = send_chat_with_images(
                    &token, &trimmed, imgs,
                ).await {
                    web_sys::console::log_1(
                        &format!("[stream-stt] send_chat failed: {e}").into(),
                    );
                    status_line.set(format!("send failed: {e}"));
                    arc_state.set(ArcState::Error);
                }
            });
        }
    }
}

fn ev_kind(ev: &ChatEvent) -> &'static str {
    match ev {
        ChatEvent::Response { .. } => "Response",
        ChatEvent::StreamChunk { .. } => "StreamChunk",
        ChatEvent::Status(_) => "Status",
        ChatEvent::ToolStarted { .. } => "ToolStarted",
        ChatEvent::ToolCompleted { .. } => "ToolCompleted",
        ChatEvent::ApprovalNeeded { .. } => "ApprovalNeeded",
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_chat_event(
    ev: ChatEvent,
    messages: RwSignal<Vec<Message>>,
    streaming_buffer: RwSignal<Option<String>>,
    arc_state: RwSignal<ArcState>,
    status_line: RwSignal<String>,
    tts_enabled: RwSignal<bool>,
    tts_ready: RwSignal<bool>,
    token: RwSignal<String>,
    pending_approval: RwSignal<Option<ApprovalRequest>>,
    mic_handle: Rc<RefCell<Option<ContinuousMic>>>,
    splitter: Rc<RefCell<SentenceSplitter>>,
    audio_queue: Rc<RefCell<Option<AudioQueue>>>,
    tts_queue: Rc<RefCell<VecDeque<String>>>,
    tts_worker_running: Rc<RefCell<bool>>,
) {
    web_sys::console::log_1(
        &format!("[jarvis] handle_chat_event: {:?}", ev_kind(&ev)).into(),
    );
    match ev {
        ChatEvent::Response { content } => {
            // Flush any trailing partial sentence into the TTS pipeline so
            // the audio matches the transcript.
            if tts_enabled.get() && tts_ready.get() {
                if let Some(tail) = splitter.borrow_mut().finish() {
                    queue_sentence(
                        tail,
                        &tts_queue,
                        &tts_worker_running,
                        &audio_queue,
                        &mic_handle,
                        token.get(),
                    );
                }
            } else {
                splitter.borrow_mut().finish();
            }

            messages.update(|m| {
                m.push(Message {
                    role: Role::Assistant,
                    content: content.clone(),
                });
            });
            // Final assembled text landed — clear the in-progress buffer.
            streaming_buffer.set(None);
            status_line.set(String::from("ready"));
            // Sentence-level TTS already queued audio as each stream chunk
            // landed; nothing more to synthesize here. Just transition the
            // ARC state once playback drains. If TTS is off, no audio was
            // queued and we go straight to Idle.
            let _ = content; // already pushed above; suppress unused warning
            if !tts_enabled.get() || !tts_ready.get() {
                if let Some(mic) = mic_handle.borrow().as_ref() {
                    mic.return_to_idle();
                }
                arc_state.set(ArcState::Idle);
            }
            // The "back to Idle once audio drains" path is handled inside
            // queue_sentence's `onended` hook on the last source.
        }
        ChatEvent::StreamChunk { content } => {
            // Append to the in-progress assistant message.
            streaming_buffer.update(|buf| match buf {
                Some(s) => s.push_str(&content),
                None => *buf = Some(content.clone()),
            });
            // Stream-mode UX: as soon as text starts flowing, JARVIS is
            // "speaking". Mic suspends so the queue's audio doesn't echo back.
            if !matches!(arc_state.get(), ArcState::Speaking) {
                arc_state.set(ArcState::Speaking);
                if let Some(mic) = mic_handle.borrow().as_ref() {
                    mic.set_speaking(true);
                }
            }
            // Feed the sentence splitter; for each completed sentence, kick
            // off a TTS round-trip and queue the resulting audio.
            if tts_enabled.get() && tts_ready.get() {
                let sentences = splitter.borrow_mut().push(&content);
                for sentence in sentences {
                    queue_sentence(
                        sentence,
                        &tts_queue,
                        &tts_worker_running,
                        &audio_queue,
                        &mic_handle,
                        token.get(),
                    );
                }
            }
        }
        ChatEvent::Status(msg) => status_line.set(msg),
        ChatEvent::ToolStarted { name } => status_line.set(format!("tool: {name}")),
        ChatEvent::ToolCompleted { name, success } => {
            let mark = if success { "✓" } else { "✗" };
            status_line.set(format!("tool {name} {mark}"));
        }
        ChatEvent::ApprovalNeeded {
            request_id,
            tool_name,
            description,
            parameters,
        } => {
            web_sys::console::log_1(
                &format!(
                    "[approval] needed for '{tool_name}' (request_id={request_id})"
                )
                .into(),
            );
            pending_approval.set(Some(ApprovalRequest {
                request_id,
                tool_name: tool_name.clone(),
                description,
                parameters,
            }));
            status_line.set(format!("approval needed: {tool_name} (say yes or no)"));
            // Speak the prompt so the user knows out loud that JARVIS is
            // paused. This goes through the same XTTS path as normal
            // replies, so the cloned voice asks for permission.
            if tts_enabled.get_untracked() && tts_ready.get_untracked() {
                queue_sentence(
                    format!("Permission needed for {tool_name}. Say yes or no."),
                    &tts_queue,
                    &tts_worker_running,
                    &audio_queue,
                    &mic_handle,
                    token.get_untracked(),
                );
            }
            arc_state.set(ArcState::Idle);
        }
    }
}

#[component]
fn ApprovalBanner(
    pending_approval: RwSignal<Option<ApprovalRequest>>,
    token: RwSignal<String>,
) -> impl IntoView {
    // Helper that builds an on:click handler for one of the three actions.
    // `pending_approval` and `token` are RwSignals (Copy), so each call
    // produces an independent closure that owns its own copy of them.
    let dispatch = move |action: &'static str| {
        move |_: leptos::ev::MouseEvent| {
            let Some(req) = pending_approval.get_untracked() else {
                return;
            };
            pending_approval.set(None);
            let tok = token.get_untracked();
            let req_id = req.request_id.clone();
            spawn_local(async move {
                if let Err(e) = post_approval(&tok, &req_id, action).await {
                    web_sys::console::log_1(
                        &format!("[approval] POST failed: {e}").into(),
                    );
                }
            });
        }
    };
    view! {
        {move || pending_approval.get().map(|req| {
            // Pull the parameter that actually matters for human review out
            // of the JSON blob so it's not hidden inside a collapsed pre. For
            // file tools we surface the path/from/to; for shell we surface
            // command; for http we surface url. Falls back to the raw JSON
            // header if no known key is present.
            let highlight = extract_key_param(&req.tool_name, &req.parameters);
            view! {
            <section class="border border-amber-400/60 bg-amber-500/10 rounded-lg p-4 font-mono text-sm">
                <div class="flex items-center justify-between mb-2">
                    <span class="text-amber-300 uppercase tracking-[0.3em] text-[10px]">
                        "approval needed"
                    </span>
                    <span class="text-amber-200/80 text-[11px]">"say yes or no"</span>
                </div>
                <div class="text-amber-100 mb-1">
                    <span class="text-amber-300">"tool: "</span>
                    {req.tool_name.clone()}
                </div>
                {highlight.map(|(label, value)| view! {
                    <div class="text-amber-50 mb-1 text-[13px] break-all">
                        <span class="text-amber-300/90 uppercase tracking-[0.2em] text-[10px] mr-2">
                            {label}
                        </span>
                        <span class="font-semibold">{value}</span>
                    </div>
                })}
                <div class="text-amber-100/80 text-[12px] mb-3">
                    {req.description.clone()}
                </div>
                <details class="mb-3">
                    <summary class="cursor-pointer text-amber-300/70 text-[11px]">
                        "full parameters"
                    </summary>
                    <pre class="mt-2 p-2 bg-black/40 rounded text-[11px] overflow-auto whitespace-pre-wrap">
                        {req.parameters.clone()}
                    </pre>
                </details>
                <div class="flex gap-2">
                    <button
                        class="px-3 py-1 bg-green-500/20 border border-green-400/60 text-green-200 rounded hover:bg-green-500/30"
                        on:click=dispatch("approve")
                    >"Approve"</button>
                    <button
                        class="px-3 py-1 bg-blue-500/20 border border-blue-400/60 text-blue-200 rounded hover:bg-blue-500/30"
                        on:click=dispatch("always")
                    >"Always"</button>
                    <button
                        class="px-3 py-1 bg-red-500/20 border border-red-400/60 text-red-200 rounded hover:bg-red-500/30"
                        on:click=dispatch("deny")
                    >"Deny"</button>
                </div>
            </section>
        }})}
    }
}

/// Build the `images` payload for an outgoing chat send. Drains the
/// one-shot `pending_screenshot` (if any) and prepends a fresh frame off
/// the ambient screen-share stream (if active). Returns None when there's
/// nothing to attach so the server stays on the cheap text-only path.
fn collect_outgoing_images(
    pending_screenshot: RwSignal<Option<String>>,
) -> Option<Vec<String>> {
    let mut imgs: Vec<String> = Vec::new();
    if let Some(frame) = grab_stream_frame() {
        imgs.push(frame);
    }
    if let Some(oneshot) = pending_screenshot.get_untracked() {
        imgs.push(oneshot);
        pending_screenshot.set(None);
    }
    if imgs.is_empty() { None } else { Some(imgs) }
}

/// Pick the parameter that most matters for human approval out of the JSON
/// blob the gateway sends. We don't make the user expand a `<details>` to
/// see whether `path` or `command` is something benign. Returns
/// `(LABEL, value)` to display, or None if nothing notable is found.
fn extract_key_param(tool_name: &str, params_json: &str) -> Option<(&'static str, String)> {
    let value: serde_json::Value = serde_json::from_str(params_json).ok()?;
    let obj = value.as_object()?;
    // Try keys most-specific to least, so e.g. vault_move shows from→to,
    // not just the first path-shaped field.
    if let (Some(from), Some(to)) = (
        obj.get("from").and_then(|v| v.as_str()),
        obj.get("to").and_then(|v| v.as_str()),
    ) {
        return Some(("MOVE", format!("{from} → {to}")));
    }
    for key in ["path", "url", "command", "query"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                let label: &'static str = match key {
                    "path" => "PATH",
                    "url" => "URL",
                    "command" => "CMD",
                    "query" => "QUERY",
                    _ => "PARAM",
                };
                return Some((label, s.to_string()));
            }
        }
    }
    None
}

/// Custom SVG icon for the PIN orb. Hand-drawn pushpin in the same
/// hairline-stroke language as the arc reactor: hex-anchored head, thin
/// shaft tapering to a point, tiny "locked" indicator above. Uses
/// `currentColor` so the parent button's text color drives the stroke,
/// and the parent's drop-shadow gives it the Stark luminescence.
#[component]
fn IconPin() -> impl IntoView {
    view! {
        <svg
            class="w-8 h-8"
            viewBox="0 0 32 32"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            stroke-linejoin="round"
        >
            // Head: hexagonal ring with inner accent dot.
            <polygon points="16,5 22,8.5 22,15.5 16,19 10,15.5 10,8.5" />
            <circle cx="16" cy="12" r="1.6" fill="currentColor" stroke="none" />
            // Anchor cross-bars on either side of the head (the "stays put" detail).
            <line x1="6" y1="12" x2="9" y2="12" />
            <line x1="23" y1="12" x2="26" y2="12" />
            // Shaft tapering to a point.
            <line x1="16" y1="19" x2="16" y2="27" />
            <polyline points="13.5,25 16,28 18.5,25" fill="currentColor" stroke="none" />
        </svg>
    }
}

/// Custom SVG icon for the LIVE orb. Monitor outline with a recording dot
/// and three concentric "broadcasting" arcs emanating around it. Reads as
/// "screen + signal going out" — the literal action of share-screen-live.
#[component]
fn IconLive() -> impl IntoView {
    view! {
        <svg
            class="w-8 h-8"
            viewBox="0 0 32 32"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            stroke-linejoin="round"
        >
            // Monitor frame + stand.
            <rect x="5" y="7" width="22" height="14" rx="1.6" />
            <line x1="13" y1="25" x2="19" y2="25" />
            <line x1="16" y1="21" x2="16" y2="25" />
            // Central "recording" dot.
            <circle cx="16" cy="14" r="1.8" fill="currentColor" stroke="none" />
            // Three signal arcs emanating from the dot.
            <path d="M12.5 14 a3.5 3.5 0 0 1 7 0" />
            <path d="M9.5 14 a6.5 6.5 0 0 1 13 0" />
        </svg>
    }
}

/// Stark-style round orb button. Flanks the arc reactor on either side.
/// Two ring states: inert (dim cyan) and active (bright cyan ring with
/// pulsing glow). Children render inside the orb above the label — pass
/// any SVG icon component (`<IconPin />`, `<IconLive />`, etc.).
#[component]
fn ArcOrbButton(
    /// Reactive flag driving the active visual. Read on every render.
    active: RwSignal<bool>,
    on_click: Callback<leptos::ev::MouseEvent>,
    /// Tooltip text (hover only — no visible label on the orb).
    title: &'static str,
    children: Children,
) -> impl IntoView {
    let outer_class = move || {
        let base = "relative w-20 h-20 rounded-full flex items-center justify-center \
                    transition-all duration-300 cursor-pointer select-none group";
        if active.get() {
            format!(
                "{base} bg-arc-500/20 ring-2 ring-arc-300 shadow-[0_0_24px_4px_rgba(34,211,238,0.55)] hover:shadow-[0_0_32px_6px_rgba(34,211,238,0.75)] text-arc-100"
            )
        } else {
            format!(
                "{base} bg-arc-900/40 ring-1 ring-arc-700/60 hover:ring-arc-400/80 hover:bg-arc-700/40 shadow-[0_0_12px_1px_rgba(34,211,238,0.15)] text-arc-300/80 hover:text-arc-100"
            )
        }
    };
    view! {
        <button
            class=outer_class
            on:click=move |ev| on_click.run(ev)
            title=title
        >
            // Inner ring (pulses when active).
            {move || if active.get() {
                view! {
                    <span class="pointer-events-none absolute inset-1 rounded-full border border-arc-300/70 animate-pulse"></span>
                }.into_any()
            } else {
                view! {
                    <span class="pointer-events-none absolute inset-1 rounded-full border border-arc-700/40"></span>
                }.into_any()
            }}
            <span class="drop-shadow-[0_0_6px_rgba(34,211,238,0.65)]">
                {children()}
            </span>
        </button>
    }
}

#[component]
fn JarvisHeader<F: Fn() + 'static + Clone>(
    status_line: RwSignal<String>,
    tts_enabled: RwSignal<bool>,
    tts_ready: RwSignal<bool>,
    muted: RwSignal<bool>,
    on_toggle_mute: F,
) -> impl IntoView {
    let toggle_tts = move |_| tts_enabled.update(|v| *v = !*v);
    view! {
        <header class="border-b border-arc-900/60 backdrop-blur sticky top-0 z-20 bg-hud-bg/80">
            <div class="max-w-5xl mx-auto px-6 py-3 flex items-center justify-between font-mono text-[11px]">
                <div class="flex items-center gap-4">
                    <div class="flex flex-col leading-tight">
                        <span class="text-arc-300 tracking-[0.45em] text-[12px]">
                            "J.A.R.V.I.S"
                        </span>
                        <span class="text-arc-700 tracking-[0.18em] text-[8px] uppercase">
                            "Just A Rather Very Intelligent System"
                        </span>
                    </div>
                    <span class="text-arc-800">"|"</span>
                    <span class="text-arc-400 uppercase text-[10px] tracking-widest">
                        {move || status_line.get()}
                    </span>
                </div>
                <div class="flex items-center gap-3">
                    <button
                        class=move || {
                            let base = "px-2 py-1 border rounded-sm tracking-widest uppercase text-[10px]";
                            if muted.get() {
                                format!("{base} border-rose-400 text-rose-200 bg-rose-900/30")
                            } else {
                                format!("{base} border-arc-400 text-arc-200 bg-arc-900/40")
                            }
                        }
                        on:click={
                            let cb = on_toggle_mute.clone();
                            move |_| cb()
                        }
                    >
                        {move || if muted.get() { "mic ▸ muted" } else { "mic ▸ live" }}
                    </button>
                    <button
                        class=move || {
                            let base = "px-2 py-1 border rounded-sm tracking-widest uppercase text-[10px]";
                            if !tts_ready.get() {
                                format!("{base} border-zinc-700 text-zinc-600 cursor-not-allowed opacity-50")
                            } else if tts_enabled.get() {
                                format!("{base} border-arc-400 text-arc-200 bg-arc-900/40")
                            } else {
                                format!("{base} border-arc-700 text-arc-400 hover:text-arc-200")
                            }
                        }
                        prop:disabled=move || !tts_ready.get()
                        on:click=toggle_tts
                    >
                        {move || if tts_enabled.get() { "voice ▸ on" } else { "voice ▸ off" }}
                    </button>
                    <span class="text-arc-700">"·"</span>
                    <span class="text-arc-500">"runtime: Iron Clad"</span>
                    <span class="text-arc-700">"·"</span>
                    <span class="text-arc-500">"v1.3.20"</span>
                </div>
            </div>
        </header>
    }
}

#[component]
fn ChatPanel(
    messages: RwSignal<Vec<Message>>,
    streaming_buffer: RwSignal<Option<String>>,
    interim_stt: RwSignal<Option<String>>,
    arc_state: RwSignal<ArcState>,
    status_line: RwSignal<String>,
    mic_initialized: RwSignal<bool>,
    muted: RwSignal<bool>,
    on_send: Callback<String>,
) -> impl IntoView {
    let input_text = RwSignal::new(String::new());
    let submit = move || {
        let content = input_text.get();
        if content.trim().is_empty() {
            return;
        }
        on_send.run(content);
        input_text.set(String::new());
    };

    // Sticky-bottom autoscroll. Each time the transcript content changes
    // (a new message, a streaming chunk, an interim STT update), we check
    // if the user is already pinned to the bottom; if so, scroll to the
    // newest content. If the user has manually scrolled up to read older
    // history, we leave them alone. Threshold of 80px catches "near the
    // bottom" so a small natural offset still counts as pinned.
    let transcript_ref = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        // Subscribe to the three signals that change the rendered list.
        let _ = messages.get();
        let _ = streaming_buffer.get();
        let _ = interim_stt.get();
        let Some(el) = transcript_ref.get() else {
            return;
        };
        // El is a HtmlDivElement wrapper. We read scrollTop/scrollHeight/
        // clientHeight directly. If at bottom, schedule scrollTop = max.
        let scroll_top = el.scroll_top();
        let scroll_height = el.scroll_height();
        let client_height = el.client_height();
        let pinned = scroll_height - scroll_top - client_height <= 80;
        if pinned {
            el.set_scroll_top(scroll_height);
        }
    });

    view! {
        <section class="relative border border-arc-900/60 rounded-lg bg-hud-panel/60 backdrop-blur-sm">
            <CornerTicks />

            <div class="px-6 py-4 border-b border-arc-900/60 flex items-center justify-between font-mono text-[11px]">
                <span class="text-arc-400 tracking-[0.35em]">"TRANSCRIPT"</span>
                <span class="text-arc-700">{move || arc_state_label(arc_state.get())}</span>
            </div>

            <div
                node_ref=transcript_ref
                class="px-6 py-4 max-h-[40vh] overflow-y-auto space-y-3 font-mono text-sm scroll-smooth hud-scroll"
            >
                { move || {
                    let m = messages.get();
                    let buf = streaming_buffer.get();
                    let live_user = interim_stt.get();
                    if m.is_empty() && buf.is_none() && live_user.is_none() {
                        let mic_up = mic_initialized.get();
                        let is_muted = muted.get();
                        let (top, bottom) = if !mic_up {
                            ("// READY ON COMMAND.", "Click the reactor to bring the mic online, or type below.")
                        } else if is_muted {
                            ("// SYSTEMS NOMINAL — MIC MUTED.", "Unmute to speak, or type below.")
                        } else {
                            ("// SYSTEMS NOMINAL.", "Listening. Speak or type.")
                        };
                        view! {
                            <div class="space-y-1">
                                <div class="text-arc-400 tracking-[0.3em] text-[10px] uppercase">{top}</div>
                                <div class="text-arc-700 italic">{bottom}</div>
                            </div>
                        }.into_any()
                    } else {
                        let mut nodes = m.into_iter().map(|msg| view! {
                            <MessageRow msg />
                        }.into_any()).collect::<Vec<_>>();
                        if let Some(partial) = live_user {
                            nodes.push(view! { <UserInterimRow text=partial /> }.into_any());
                        }
                        if let Some(partial) = buf {
                            nodes.push(view! { <InProgressRow text=partial /> }.into_any());
                        }
                        nodes.into_iter().collect_view().into_any()
                    }
                }}
            </div>

            <div class="px-6 py-3 border-t border-arc-900/60 flex items-center gap-3">
                <span class="text-arc-500 font-mono text-xs tracking-widest">">"</span>
                <input
                    type="text"
                    class="flex-1 bg-transparent border-none outline-none text-arc-100 font-mono text-sm placeholder:text-arc-700"
                    placeholder="Type a command or transcript..."
                    prop:value=move || input_text.get()
                    on:input=move |ev| {
                        let target: HtmlInputElement = ev.target().unwrap().unchecked_into();
                        input_text.set(target.value());
                    }
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            ev.prevent_default();
                            submit();
                        }
                    }
                />
                <button
                    class="px-3 py-1 border border-arc-500 text-arc-200 font-mono text-[11px] tracking-widest uppercase rounded-sm hover:bg-arc-900/40"
                    on:click=move |_| submit()
                >
                    "send"
                </button>
            </div>
            <div class="px-6 pb-3 text-arc-700 font-mono text-[10px] tracking-widest uppercase">
                {move || status_line.get()}
            </div>
        </section>
    }
}

fn arc_state_label(s: ArcState) -> &'static str {
    match s {
        ArcState::Idle => "standby",
        ArcState::Listening => "listening",
        ArcState::Thinking => "thinking",
        ArcState::Speaking => "speaking",
        ArcState::Error => "error",
    }
}

/// Live user-speech interim row. Updated in-place as Parakeet emits interim
/// transcripts over the WebTransport events stream. Italic / dimmer so it's
/// visually distinct from a finalized message — it's a moving target until
/// the VAD endpoint fires and we promote it to a real Message.
#[component]
fn UserInterimRow(text: String) -> impl IntoView {
    view! {
        <div class="leading-relaxed">
            <span class="font-mono text-[10px] text-arc-500">"["</span>
            <span class="font-mono text-[10px] tracking-[0.3em] uppercase text-arc-500">"you"</span>
            <span class="font-mono text-[10px] text-arc-700">"]"</span>
            <span class="ml-2 italic text-arc-400 whitespace-pre-wrap">{text}</span>
            <span class="ml-1 inline-block w-2 h-3 align-middle bg-arc-500 animate-pulse-slow"></span>
        </div>
    }
}

/// Streaming in-progress assistant message. Same layout as `MessageRow` but
/// with a blinking cursor at the end to signal that text is still arriving.
#[component]
fn InProgressRow(text: String) -> impl IntoView {
    view! {
        <div class="leading-relaxed">
            <span class="font-mono text-[10px] text-arc-300">"["</span>
            <span class="font-mono text-[10px] tracking-[0.3em] uppercase text-arc-300">"jarvis"</span>
            <span class="font-mono text-[10px] text-arc-700">"]"</span>
            <span class="ml-2 text-arc-100 whitespace-pre-wrap">{text}</span>
            <span class="ml-1 inline-block w-2 h-3 align-middle bg-arc-300 animate-pulse-slow"></span>
        </div>
    }
}

#[component]
fn MessageRow(msg: Message) -> impl IntoView {
    let (label, color) = match msg.role {
        Role::User => ("you", "text-arc-200"),
        Role::Assistant => ("jarvis", "text-arc-300"),
    };
    let label_class = format!("font-mono text-[10px] tracking-[0.3em] uppercase {color}");
    let bracket_class = format!("font-mono text-[10px] {color}");
    view! {
        <div class="leading-relaxed">
            <span class=bracket_class>"["</span>
            <span class=label_class>{label}</span>
            <span class="font-mono text-[10px] text-arc-700">"]"</span>
            <span class="ml-2 text-arc-100 whitespace-pre-wrap">{msg.content}</span>
        </div>
    }
}

#[component]
fn CornerTicks() -> impl IntoView {
    let cls = "absolute w-3 h-3 border-arc-400/70 pointer-events-none";
    view! {
        <>
            <div class=format!("{cls} top-0 left-0 border-t border-l")></div>
            <div class=format!("{cls} top-0 right-0 border-t border-r")></div>
            <div class=format!("{cls} bottom-0 left-0 border-b border-l")></div>
            <div class=format!("{cls} bottom-0 right-0 border-b border-r")></div>
        </>
    }
}

#[component]
fn TelemetryStrip(
    stt_ready: RwSignal<bool>,
    tts_ready: RwSignal<bool>,
    token_ready: RwSignal<bool>,
    muted: RwSignal<bool>,
) -> impl IntoView {
    view! {
        <section class="grid grid-cols-2 md:grid-cols-4 gap-4 font-mono text-[11px]">
            <Cell label="GATEWAY"  ok=Signal::derive(move || token_ready.get()) />
            <Cell label="PARAKEET" ok=Signal::derive(move || stt_ready.get() && !muted.get()) />
            <Cell label="11LABS"   ok=Signal::derive(move || tts_ready.get()) />
            <Cell label="HAIKU 4.5" ok=Signal::derive(move || token_ready.get()) />
        </section>
    }
}

#[component]
fn Cell(label: &'static str, ok: Signal<bool>) -> impl IntoView {
    view! {
        <div
            class=move || {
                let base = "relative border bg-hud-panel/40 px-4 py-3 transition-colors duration-300";
                if ok.get() {
                    format!("{base} border-arc-700/80")
                } else {
                    format!("{base} border-zinc-800/80 opacity-60")
                }
            }
        >
            <CornerTicks />
            <div class="flex items-center justify-between gap-3">
                <div class="flex items-center gap-3">
                    // LED dot — solid cyan + pulse when online, dim grey when offline
                    <span
                        class=move || {
                            let base = "inline-block w-2 h-2 rounded-full";
                            if ok.get() {
                                format!("{base} bg-arc-300 animate-pulse-slow shadow-[0_0_8px_2px_rgba(34,211,238,0.6)]")
                            } else {
                                format!("{base} bg-zinc-700")
                            }
                        }
                    ></span>
                    <span
                        class=move || {
                            let base = "tracking-[0.3em] uppercase text-[10px]";
                            if ok.get() { format!("{base} text-arc-300") } else { format!("{base} text-zinc-500") }
                        }
                    >
                        {label}
                    </span>
                </div>
                <span
                    class=move || {
                        let base = "text-[9px] tracking-widest uppercase";
                        if ok.get() { format!("{base} text-arc-400") } else { format!("{base} text-zinc-600") }
                    }
                >
                    {move || if ok.get() { "ONLINE" } else { "OFFLINE" }}
                </span>
            </div>
        </div>
    }
}
