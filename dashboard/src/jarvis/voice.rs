//! Voice plumbing in the browser.
//!
//! Two surfaces:
//! - `ContinuousMic`: always-on listening pipeline with RMS-based VAD,
//!   automatic sentence-end detection (configurable silence timeout), and a
//!   mute switch. Fires a callback with a wav blob each time an utterance
//!   ends. Stays on across utterances; the caller suspends it during TTS.
//! - `blob_to_wav_16k` + `play_wav`: glue used by callers that want to
//!   transcode a raw `Blob` (eg from `MediaRecorder`) or play a wav blob.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use js_sys::{Array, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    AnalyserNode, AudioBuffer, AudioContext, Blob, BlobPropertyBag, HtmlAudioElement,
    MediaStream, MediaStreamAudioSourceNode, MediaStreamConstraints, OfflineAudioContext, Url,
};

fn console_log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

/// Visible state for the UI to drive the ARC reactor and headers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MicState {
    /// User has the mic muted; not listening at all.
    Muted,
    /// Mic open, AnalyserNode running, no speech detected yet.
    Idle,
    /// Speech detected; accumulating samples.
    Listening,
    /// Endpoint hit; encoding + waiting on STT (caller drives this).
    Processing,
    /// JARVIS is playing TTS; mic suspended so it doesn't loop back.
    Speaking,
}

/// Tunables for the VAD endpoint detector.
#[derive(Clone, Copy, Debug)]
pub struct VadConfig {
    /// RMS above this counts as "voice present" (0.0 - 1.0). Used in Idle /
    /// Listening states.
    pub speech_threshold: f32,
    /// RMS above this counts as a barge-in *while JARVIS is speaking*. Set
    /// higher than `speech_threshold` so background TTS playback echoing
    /// through the room doesn't false-trigger.
    pub barge_in_threshold: f32,
    /// Minimum continuous voice duration before we declare an utterance has
    /// started (filters out keyboard clicks, taps, etc).
    pub min_speech_ms: u32,
    /// Silence duration after speech to count as "sentence ended".
    /// 5000ms by default per the user's pause-tolerance ask.
    pub endpoint_silence_ms: u32,
    /// How often we tick the VAD (lower = more responsive).
    pub frame_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            // Static floor under the adaptive 2x-noise-floor calculation.
            // The adaptive threshold dominates in noisy rooms; this guards
            // against the silly case where the room is dead silent.
            speech_threshold: 0.010,
            barge_in_threshold: 0.05,
            // 2 frames at 80ms = 160ms. Words like "test" or "go" don't
            // sustain 250ms continuous voicing, especially after the pre-utt
            // hangover squeezes through brief silences.
            min_speech_ms: 160,
            // 1.2s of silence ends an utterance. Long enough for a natural
            // breath / mid-sentence pause but short enough to feel responsive
            // for a one-word command.
            endpoint_silence_ms: 1200,
            frame_ms: 80,
        }
    }
}

/// Always-on listening pipeline. Construct once, drop to tear down.
///
/// Audio flow:
/// 1. `getUserMedia({ audio: true })` opens the mic.
/// 2. `AudioContext` builds a graph: `MediaStreamSource -> AnalyserNode`.
/// 3. A `setInterval` tick reads samples from the analyser, computes RMS,
///    runs the VAD state machine, and accumulates raw 16 kHz mono samples
///    into a Vec<f32>.
/// 4. On endpoint, the buffer is encoded to wav and handed to the callback.
pub struct ContinuousMic {
    ctx: AudioContext,
    stream: MediaStream,
    state: Rc<RefCell<MicState>>,
    inner: Rc<RefCell<VadInner>>,
    /// Mic source node. Kept accessible so `attach_audio_worklet` can fan it
    /// out to an AudioWorkletNode in addition to the analyser.
    source: MediaStreamAudioSourceNode,
    _analyser: AnalyserNode,
    /// Owns the setInterval handle so we can cancel on drop.
    interval_id: Option<i32>,
    /// Closures we need to keep alive for the lifetime of the mic.
    _closures: Vec<Closure<dyn FnMut()>>,
    /// Keep the worklet `onmessage` closure alive if we've attached one.
    _worklet_msg: RefCell<Option<Closure<dyn FnMut(web_sys::MessageEvent)>>>,
}

struct VadInner {
    config: VadConfig,
    /// Samples accumulated since the current utterance started.
    /// We keep them at the AudioContext's native sample rate (typically
    /// 44100 / 48000); the wav encoder resamples to 16 kHz on emit.
    samples: Vec<f32>,
    /// ms of voice frames accumulated toward `min_speech_ms`. Survives brief
    /// silent gaps (see `pre_utt_silence_ms` hangover) so the natural pauses
    /// inside a word like "testing" don't reset the count.
    voice_run_ms: u32,
    /// ms of consecutive silence frames since speech started.
    silence_run_ms: u32,
    /// ms of consecutive silence since the *last* voiced frame, used to
    /// decide when to give up on a partial pre-utterance accumulation.
    pre_utt_silence_ms: u32,
    /// Did we declare speech started yet?
    in_utterance: bool,
    /// Native sample rate of the AudioContext (so we can resample on emit).
    sample_rate: f32,
    /// Where we publish the finished utterance.
    /// Rc'd so the async Silero gate can clone it into a spawn_local
    /// future, then call back into the callback only if the audio scored
    /// as speech.
    on_utterance: Rc<RefCell<Box<dyn FnMut(Vec<u8>)>>>,
    /// Fires once when the user begins a barge-in (speech crosses the higher
    /// threshold while JARVIS is in `Speaking` state).
    on_barge_in: Box<dyn FnMut()>,
    /// Fires when Silero rejects an utterance as non-speech. The orchestrator
    /// uses this to undo phantom barge-in halts caused by music tripping the
    /// RMS VAD before Silero has a chance to weigh in.
    on_silero_reject: Rc<RefCell<Box<dyn FnMut()>>>,
    /// Externally-observable mic state (same Rc the caller reads).
    state: Rc<RefCell<MicState>>,
    /// Reusable scratch buffer for the analyser pull.
    analyser_buf: Vec<f32>,
    /// Diagnostic counter: tick number, used to throttle RMS logging to ~1Hz.
    tick_counter: u32,
    /// Adaptive noise floor — exponential moving average of RMS during
    /// silence. The effective speech threshold is `max(config_threshold,
    /// noise_floor * 3.0)` so when AGC pumps ambient noise up we still detect
    /// real speech relative to the floor.
    noise_floor: f32,
}

impl VadInner {
    fn tick(&mut self, analyser: &AnalyserNode) {
        let state = *self.state.borrow();
        // Muted: don't even look at the audio. Processing: STT is in flight,
        // wait for the orchestrator to flip us back to Idle.
        if matches!(state, MicState::Muted | MicState::Processing) {
            return;
        }

        // Pull the latest float samples from the analyser into our scratch buf.
        let fft_size = analyser.fft_size() as usize;
        if self.analyser_buf.len() != fft_size {
            self.analyser_buf.resize(fft_size, 0.0);
        }
        analyser.get_float_time_domain_data(&mut self.analyser_buf[..]);

        let sum_sq: f32 = self.analyser_buf.iter().map(|s| s * s).sum();
        let rms = (sum_sq / fft_size as f32).sqrt();

        let cfg = self.config;
        // Adaptive noise floor: blend the *quietest* recent frames into the
        // floor. Critically, we DON'T blend in frames above the current
        // threshold — those are likely speech, and treating them as ambient
        // would inflate the floor and make the next utterance harder to
        // catch. (We saw exactly this: a missed "testing" pushed the floor
        // from 0.005 to 0.013, putting the next attempt out of reach.)
        let current_threshold = (self.noise_floor * 2.0).max(cfg.speech_threshold);
        if !self.in_utterance && rms < current_threshold {
            self.noise_floor = self.noise_floor * 0.97 + rms * 0.03;
        }
        // Effective threshold is max(static, 2x noise floor) — speech must
        // measurably exceed background. Speaking-state still uses the higher
        // barge-in threshold so TTS room echo doesn't false-trigger.
        let adaptive = (self.noise_floor * 2.0).max(cfg.speech_threshold);
        let threshold = if state == MicState::Speaking {
            cfg.barge_in_threshold.max(adaptive)
        } else {
            adaptive
        };
        let voiced = rms > threshold;

        // Diagnostic: log VAD state ~1Hz so we can see what's happening.
        self.tick_counter = self.tick_counter.wrapping_add(1);
        let log_every = (1000 / cfg.frame_ms.max(1)).max(1); // ~once per second
        if self.tick_counter.is_multiple_of(log_every) {
            console_log(&format!(
                "[jarvis-vad] rms={:.4} thr={:.4} floor={:.4} voiced={} in_utt={} voice_run={}ms silence_run={}ms state={:?}",
                rms, threshold, self.noise_floor, voiced, self.in_utterance,
                self.voice_run_ms, self.silence_run_ms, state
            ));
        }

        // Pre-utterance hangover: 240ms. Words like "testing" have natural
        // micro-silences between consonants; a single quiet frame must NOT
        // reset the accumulation toward min_speech_ms. We only give up if
        // silence persists past the hangover.
        const PRE_UTT_HANGOVER_MS: u32 = 240;

        if voiced {
            self.voice_run_ms = self.voice_run_ms.saturating_add(cfg.frame_ms);
            self.silence_run_ms = 0;
            self.pre_utt_silence_ms = 0;
        } else {
            if self.in_utterance {
                self.silence_run_ms = self.silence_run_ms.saturating_add(cfg.frame_ms);
            } else {
                // Tolerate brief gaps before declaring an utterance.
                self.pre_utt_silence_ms =
                    self.pre_utt_silence_ms.saturating_add(cfg.frame_ms);
                if self.pre_utt_silence_ms > PRE_UTT_HANGOVER_MS {
                    self.voice_run_ms = 0;
                    self.pre_utt_silence_ms = 0;
                }
            }
        }

        // Transition into an utterance once we've crossed the min-speech bar.
        // If we were in Speaking state, this is a barge-in — fire the
        // callback so the orchestrator can halt the audio queue gracefully.
        if !self.in_utterance && self.voice_run_ms >= cfg.min_speech_ms {
            self.in_utterance = true;
            if state == MicState::Speaking {
                console_log("[jarvis-vad] barge-in detected");
                (self.on_barge_in)();
            } else {
                console_log("[jarvis-vad] utterance started");
            }
            *self.state.borrow_mut() = MicState::Listening;
        }

        if self.in_utterance {
            self.samples.extend_from_slice(&self.analyser_buf);
        }

        if self.in_utterance && self.silence_run_ms >= cfg.endpoint_silence_ms {
            console_log(&format!(
                "[jarvis-vad] endpoint hit ({} samples at {} Hz, encoding wav)",
                self.samples.len(),
                self.sample_rate
            ));
            *self.state.borrow_mut() = MicState::Processing;
            // Resample to 16k Float32 once and reuse for both Silero
            // scoring and wav encoding so we don't pay the resample twice.
            let samples_16k = resample_to_16k(&self.samples, self.sample_rate);
            let wav = encode_f32_16k_to_wav(&samples_16k);
            self.samples.clear();
            self.voice_run_ms = 0;
            self.silence_run_ms = 0;
            self.in_utterance = false;

            // Hand off to the async Silero gate. The on_utterance Box
            // lives behind an Rc<RefCell<...>> so we can clone an Rc into
            // the spawn_local future. If the gate passes (or fails-open
            // because Silero couldn't load) we call the callback; if it
            // rejects the audio as non-speech, we log and drop.
            let cb = Rc::clone(&self.on_utterance);
            let reject_cb = Rc::clone(&self.on_silero_reject);
            let state = Rc::clone(&self.state);
            spawn_local(async move {
                let score = silero_score(&samples_16k).await;
                console_log(&format!(
                    "[jarvis-vad] silero score = {:.2} ({} samples @ 16kHz)",
                    score,
                    samples_16k.len()
                ));
                if score < 0.5 {
                    console_log(
                        "[jarvis-vad] silero gated: audio rejected as non-speech (music/noise); \
                         dropping",
                    );
                    *state.borrow_mut() = MicState::Idle;
                    (reject_cb.borrow_mut())();
                    return;
                }
                (cb.borrow_mut())(wav);
            });
        }
    }
}

impl ContinuousMic {
    /// Construct + open the mic. `on_utterance` fires once per detected
    /// utterance with a freshly-encoded 16 kHz mono wav blob. `on_barge_in`
    /// fires once when the user starts speaking while JARVIS is in the
    /// `Speaking` state — the orchestrator should halt the audio queue.
    pub async fn start<F, B, R>(
        config: VadConfig,
        on_utterance: F,
        on_barge_in: B,
        on_silero_reject: R,
        state: Rc<RefCell<MicState>>,
    ) -> Result<Self, String>
    where
        F: FnMut(Vec<u8>) + 'static,
        B: FnMut() + 'static,
        R: FnMut() + 'static,
    {
        let window = web_sys::window().ok_or("no window")?;
        let nav = window.navigator();
        let media = nav
            .media_devices()
            .map_err(|_| "MediaDevices unavailable".to_string())?;
        let constraints = MediaStreamConstraints::new();
        // Ask the browser to apply built-in echo cancellation +
        // noise suppression + auto-gain so TTS playback doesn't feed back
        // into the mic and trigger false barge-ins.
        let audio_opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &audio_opts,
            &JsValue::from_str("echoCancellation"),
            &JsValue::from_bool(true),
        );
        let _ = js_sys::Reflect::set(
            &audio_opts,
            &JsValue::from_str("noiseSuppression"),
            &JsValue::from_bool(true),
        );
        let _ = js_sys::Reflect::set(
            &audio_opts,
            &JsValue::from_str("autoGainControl"),
            &JsValue::from_bool(true),
        );
        constraints.set_audio(&audio_opts.into());
        let promise = media
            .get_user_media_with_constraints(&constraints)
            .map_err(|_| "getUserMedia failed to start".to_string())?;
        let stream_js = JsFuture::from(promise)
            .await
            .map_err(|e| format!("getUserMedia: {:?}", e))?;
        let stream: MediaStream = stream_js
            .dyn_into()
            .map_err(|_| "not a MediaStream".to_string())?;

        let ctx = AudioContext::new().map_err(|e| format!("audioctx: {:?}", e))?;
        let source = ctx
            .create_media_stream_source(&stream)
            .map_err(|e| format!("source: {:?}", e))?;
        let analyser = ctx
            .create_analyser()
            .map_err(|e| format!("analyser: {:?}", e))?;
        analyser.set_fft_size(2048);
        analyser.set_smoothing_time_constant(0.0);
        source
            .connect_with_audio_node(&analyser)
            .map_err(|e| format!("connect: {:?}", e))?;
        // Do NOT connect the analyser to the destination — we don't want to
        // hear ourselves through the speakers.

        let sample_rate = ctx.sample_rate();
        console_log(&format!(
            "[jarvis-vad] audio context up, native sample rate = {} Hz",
            sample_rate
        ));

        let inner = Rc::new(RefCell::new(VadInner {
            config,
            samples: Vec::with_capacity((sample_rate as usize) * 8),
            voice_run_ms: 0,
            silence_run_ms: 0,
            pre_utt_silence_ms: 0,
            in_utterance: false,
            sample_rate,
            on_utterance: Rc::new(RefCell::new(Box::new(on_utterance))),
            on_barge_in: Box::new(on_barge_in),
            on_silero_reject: Rc::new(RefCell::new(Box::new(on_silero_reject))),
            state: Rc::clone(&state),
            analyser_buf: Vec::new(),
            tick_counter: 0,
            // Seed at a typical-room-ambient value. A higher seed (e.g. the
            // static threshold) creates a long ramp-down before the floor
            // matches reality, during which speech may be missed.
            noise_floor: 0.003,
        }));

        // setInterval-driven tick.
        let tick_inner = Rc::clone(&inner);
        let analyser_for_tick = analyser.clone();
        let tick = Closure::wrap(Box::new(move || {
            tick_inner.borrow_mut().tick(&analyser_for_tick);
        }) as Box<dyn FnMut()>);

        let interval_id = window
            .set_interval_with_callback_and_timeout_and_arguments_0(
                tick.as_ref().unchecked_ref(),
                config.frame_ms as i32,
            )
            .map_err(|e| format!("setInterval: {:?}", e))?;

        *state.borrow_mut() = MicState::Idle;

        Ok(Self {
            ctx,
            stream,
            state,
            inner,
            source,
            _analyser: analyser,
            interval_id: Some(interval_id),
            _closures: vec![tick],
            _worklet_msg: RefCell::new(None),
        })
    }

    /// Load the AudioWorklet at `worklet_url` and connect a `pcm-pump`
    /// AudioWorkletNode in parallel with the existing analyser. Every chunk
    /// the worklet posts (1280 samples of 16 kHz mono float32 = 80 ms)
    /// triggers `on_chunk`. This is the audio source for streaming STT.
    ///
    /// Safe to call multiple times only with care — each call attaches a new
    /// worklet node, but the existing analyser pipeline is left untouched.
    pub async fn attach_audio_worklet<F>(
        &self,
        worklet_url: &str,
        on_chunk: F,
    ) -> Result<(), String>
    where
        F: FnMut(Vec<f32>) + 'static,
    {
        let worklet = self.ctx.audio_worklet().map_err(|e| {
            format!("audioContext.audioWorklet not available: {:?}", e)
        })?;
        JsFuture::from(
            worklet
                .add_module(worklet_url)
                .map_err(|e| format!("worklet.addModule: {:?}", e))?,
        )
        .await
        .map_err(|e| format!("worklet load: {:?}", e))?;
        console_log(&format!("[jarvis-mic] worklet loaded from {}", worklet_url));

        let node = web_sys::AudioWorkletNode::new(&self.ctx, "pcm-pump")
            .map_err(|e| format!("AudioWorkletNode(pcm-pump): {:?}", e))?;

        // Fan-in to the worklet — same source feeds analyser + worklet.
        self.source
            .connect_with_audio_node(&node)
            .map_err(|e| format!("source.connect(worklet): {:?}", e))?;

        // Wire the worklet's port for samples coming from the audio thread.
        let cb = Rc::new(RefCell::new(on_chunk));
        let msg_handler = Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
            let data = ev.data();
            // Worklet posts a Float32Array (transferred); read it as a Vec<f32>.
            let arr = match data.dyn_into::<js_sys::Float32Array>() {
                Ok(a) => a,
                Err(_) => return,
            };
            let mut buf = vec![0f32; arr.length() as usize];
            arr.copy_to(&mut buf);
            (cb.borrow_mut())(buf);
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);

        let port = node.port().map_err(|e| format!("worklet.port: {:?}", e))?;
        port.set_onmessage(Some(msg_handler.as_ref().unchecked_ref()));
        *self._worklet_msg.borrow_mut() = Some(msg_handler);

        Ok(())
    }

    /// Stop listening entirely. Drops the AudioContext + MediaStream.
    pub fn close(&mut self) {
        if let Some(id) = self.interval_id.take() {
            if let Some(w) = web_sys::window() {
                w.clear_interval_with_handle(id);
            }
        }
        // Stop the mic LED in the OS by stopping all tracks.
        let tracks = self.stream.get_tracks();
        for i in 0..tracks.length() {
            if let Some(t) = tracks.get(i).dyn_ref::<web_sys::MediaStreamTrack>() {
                t.stop();
            }
        }
        let _ = self.ctx.close();
        *self.state.borrow_mut() = MicState::Muted;
    }

    /// Quick mute: stop accumulating audio but keep the AudioContext alive
    /// for fast resume.
    pub fn set_muted(&self, muted: bool) {
        if muted {
            // Drop any partial utterance and stop accumulating.
            self.inner.borrow_mut().samples.clear();
            self.inner.borrow_mut().in_utterance = false;
            *self.state.borrow_mut() = MicState::Muted;
        } else {
            *self.state.borrow_mut() = MicState::Idle;
        }
    }

    /// Tell the VAD to ignore incoming audio (e.g., while TTS plays).
    /// `false` returns us to Idle.
    pub fn set_speaking(&self, speaking: bool) {
        if speaking {
            self.inner.borrow_mut().samples.clear();
            self.inner.borrow_mut().in_utterance = false;
            *self.state.borrow_mut() = MicState::Speaking;
        } else if *self.state.borrow() == MicState::Speaking {
            *self.state.borrow_mut() = MicState::Idle;
        }
    }

    /// Caller tells the mic it's safe to listen again after STT completes.
    pub fn return_to_idle(&self) {
        let mut s = self.state.borrow_mut();
        if *s == MicState::Processing {
            *s = MicState::Idle;
        }
    }
}

impl Drop for ContinuousMic {
    fn drop(&mut self) {
        self.close();
    }
}

/// Decode any browser audio blob (webm/opus, wav, etc) and re-encode to
/// 16 kHz mono PCM-16 wav. Used by callers that handed us a Blob from
/// MediaRecorder or similar. Currently unused (ContinuousMic encodes raw
/// PCM directly), but kept around for future MediaRecorder-based paths.
#[allow(dead_code)]
pub async fn blob_to_wav_16k(blob: Blob) -> Result<Vec<u8>, String> {
    let buf_promise = blob.array_buffer();
    let array_buffer = JsFuture::from(buf_promise)
        .await
        .map_err(|e| format!("buf: {:?}", e))?;

    let decode_ctx = AudioContext::new().map_err(|e| format!("audioctx: {:?}", e))?;
    let decoded_js = JsFuture::from(
        decode_ctx
            .decode_audio_data(&array_buffer.into())
            .map_err(|e| format!("decode: {:?}", e))?,
    )
    .await
    .map_err(|e| format!("decode-await: {:?}", e))?;
    let _ = decode_ctx.close();
    let decoded: AudioBuffer = decoded_js
        .dyn_into()
        .map_err(|_| "not AudioBuffer".to_string())?;

    let target_rate: u32 = 16_000;
    let length = (decoded.duration() * target_rate as f64).ceil() as u32;
    let offline = OfflineAudioContext::new_with_number_of_channels_and_length_and_sample_rate(
        1,
        length,
        target_rate as f32,
    )
    .map_err(|e| format!("offline: {:?}", e))?;
    let source = offline
        .create_buffer_source()
        .map_err(|e| format!("source: {:?}", e))?;
    source.set_buffer(Some(&decoded));
    source
        .connect_with_audio_node(&offline.destination())
        .map_err(|e| format!("connect: {:?}", e))?;
    source.start().map_err(|e| format!("start: {:?}", e))?;

    let rendered_js = JsFuture::from(
        offline
            .start_rendering()
            .map_err(|e| format!("render: {:?}", e))?,
    )
    .await
    .map_err(|e| format!("render-await: {:?}", e))?;
    let rendered: AudioBuffer = rendered_js
        .dyn_into()
        .map_err(|_| "not AudioBuffer".to_string())?;

    let mut samples = vec![0f32; rendered.length() as usize];
    rendered
        .copy_from_channel(&mut samples, 0)
        .map_err(|e| format!("copy: {:?}", e))?;

    Ok(encode_pcm16_wav(&samples, target_rate))
}

/// Resample to 16 kHz Float32 mono. Linear interpolation — same quality
/// as `encode_resampled_wav` but returns the intermediate Float32 buffer
/// so callers can both encode it as wav AND feed it to Silero VAD without
/// resampling twice.
fn resample_to_16k(samples: &[f32], source_rate: f32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    if (source_rate - 16_000.0).abs() < 1.0 {
        return samples.to_vec();
    }
    let ratio = source_rate / 16_000.0;
    let out_len = ((samples.len() as f32) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f32 * ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f32;
        let a = samples[idx];
        let b = if idx + 1 < samples.len() { samples[idx + 1] } else { a };
        out.push(a + (b - a) * frac);
    }
    out
}

/// Encode 16 kHz Float32 mono PCM into a wav blob. Pairs with
/// `resample_to_16k`; if you already have the f32 buffer, this skips the
/// second resample done by `encode_resampled_wav`.
fn encode_f32_16k_to_wav(samples_16k: &[f32]) -> Vec<u8> {
    encode_pcm16_wav(samples_16k, 16_000)
}

/// Score audio with Silero VAD via the JS shim in `silero.js`. Returns the
/// fraction of audio Silero classified as speech (0.0..1.0). On any
/// failure (model not loaded, JS error, browser issue) returns 1.0 so we
/// fail OPEN — a broken VAD gate must never silence real speech.
async fn silero_score(samples_16k: &[f32]) -> f32 {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let Some(window) = web_sys::window() else {
        return 1.0;
    };
    // window.__sileroScore — set by silero.js once the vad-web library
    // loads. May not exist on very first frame after page load (race);
    // in that case return 1.0 and the utterance passes through.
    let score_fn = match js_sys::Reflect::get(&window, &JsValue::from_str("__sileroScore")) {
        Ok(v) => v,
        Err(_) => return 1.0,
    };
    if score_fn.is_undefined() || score_fn.is_null() {
        return 1.0;
    }
    let Ok(score_fn) = score_fn.dyn_into::<js_sys::Function>() else {
        return 1.0;
    };

    // Build a Float32Array from the Rust slice. Use unsafe view + clone
    // so we don't outlive the borrow.
    let samples_array = js_sys::Float32Array::new_with_length(samples_16k.len() as u32);
    samples_array.copy_from(samples_16k);

    let promise_js = match score_fn.call1(&JsValue::UNDEFINED, &samples_array) {
        Ok(v) => v,
        Err(_) => return 1.0,
    };
    let Ok(promise) = promise_js.dyn_into::<js_sys::Promise>() else {
        return 1.0;
    };
    let result = match JsFuture::from(promise).await {
        Ok(v) => v,
        Err(_) => return 1.0,
    };
    result.as_f64().map(|v| v as f32).unwrap_or(1.0)
}

/// Encode raw PCM-float samples (at `source_rate`) as 16 kHz mono PCM-16 wav.
/// Linear interpolation resampler — good enough for STT.
#[allow(dead_code)]
fn encode_resampled_wav(samples: &[f32], source_rate: f32, target_rate: u32) -> Vec<u8> {
    if samples.is_empty() {
        return encode_pcm16_wav(&[], target_rate);
    }
    if (source_rate - target_rate as f32).abs() < 1.0 {
        return encode_pcm16_wav(samples, target_rate);
    }
    let ratio = source_rate / target_rate as f32;
    let out_len = ((samples.len() as f32) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f32 * ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f32;
        let a = samples[idx];
        let b = if idx + 1 < samples.len() {
            samples[idx + 1]
        } else {
            a
        };
        out.push(a + (b - a) * frac);
    }
    encode_pcm16_wav(&out, target_rate)
}

fn encode_pcm16_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    let data_size: u32 = (samples.len() * 2) as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_size).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let scaled = if clamped < 0.0 {
            (clamped * 0x8000 as f32) as i16
        } else {
            (clamped * 0x7fff as f32) as i16
        };
        out.extend_from_slice(&scaled.to_le_bytes());
    }
    out
}

// =============================================================================
// Sentence splitter
// =============================================================================

/// Streams Claude's incremental text chunks and emits complete sentences as
/// soon as they cross a punctuation boundary followed by whitespace. Skips
/// common abbreviations and decimals so "Dr. Smith" or "3.14" don't trigger
/// a split mid-sentence.
pub struct SentenceSplitter {
    buf: String,
}

impl SentenceSplitter {
    pub fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Append text. Returns any complete sentences that just closed. Buffers
    /// the trailing partial sentence for the next call.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        // Greedy split: scan for a "punct + whitespace" boundary that isn't
        // preceded by an abbreviation or a digit (decimal).
        loop {
            let Some(idx) = find_sentence_boundary(&self.buf) else {
                break;
            };
            let (head, rest) = self.buf.split_at(idx + 1); // include the punct
            let sentence = head.trim().to_string();
            let tail = rest.trim_start().to_string();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            self.buf = tail;
        }
        out
    }

    /// Flush whatever remains in the buffer as a final sentence.
    pub fn finish(&mut self) -> Option<String> {
        let s = self.buf.trim().to_string();
        self.buf.clear();
        if s.is_empty() { None } else { Some(s) }
    }
}

/// Return the byte index of the first `.`, `!`, or `?` followed by
/// whitespace AND not part of an abbreviation/decimal/ellipsis. Returns
/// `None` if no boundary is found in the current buffer.
fn find_sentence_boundary(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'.' || c == b'!' || c == b'?' {
            // Must be followed by whitespace.
            let next = bytes.get(i + 1).copied().unwrap_or(b' ');
            if !(next == b' ' || next == b'\n' || next == b'\t' || next == b'\r') {
                i += 1;
                continue;
            }
            // Skip ellipsis ("..." — wait for the third dot).
            if c == b'.' && i + 2 < bytes.len() && bytes[i + 1] == b'.' {
                i += 1;
                continue;
            }
            // Skip decimals: "3.14" — digit before, digit after.
            if c == b'.' && i > 0 && bytes[i - 1].is_ascii_digit() {
                if let Some(prev_word_end) = bytes.get(i + 1).copied() {
                    if prev_word_end.is_ascii_digit() {
                        i += 1;
                        continue;
                    }
                }
            }
            // Skip common abbreviations: "Mr.", "Mrs.", "Dr.", "etc.", "Inc.", "Co.", "vs."
            if c == b'.' {
                let head = &s[..i];
                let last_word = head
                    .rsplit(|ch: char| ch.is_whitespace())
                    .next()
                    .unwrap_or("");
                if matches!(
                    last_word,
                    "Mr" | "Mrs" | "Ms" | "Dr" | "Prof" | "Sr" | "Jr" | "vs" | "etc"
                        | "Inc" | "Co" | "Ltd" | "Corp" | "St" | "Ave" | "e.g" | "i.e"
                ) {
                    i += 1;
                    continue;
                }
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Strip the markdown noise out of a sentence so TTS reads natural prose.
///
/// The dashboard still displays the original markdown (`**Wednesday**`,
/// bullet lists, etc.); this function only shapes what's sent to ElevenLabs.
/// Without this, JARVIS reads `asterisk asterisk Wednesday asterisk asterisk
/// one period What is the priority` etc.
///
/// Rules:
///   - `**x**` / `__x__` / `~~x~~` / `*x*` / `_x_` / `` `x` ``  → x
///   - Leading `#`/`##`/`###` + space (headings) → stripped
///   - Leading `- ` / `* ` / `+ ` / `N. ` / `N) ` (list markers) → stripped
///   - `[text](url)` → text
///   - Collapse runs of whitespace to a single space.
pub fn strip_markdown_for_tts(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut at_line_start = true;
    while i < chars.len() {
        let c = chars[i];
        // Two-char markers first: ** __ ~~
        if i + 1 < chars.len() {
            let pair = (c, chars[i + 1]);
            if pair == ('*', '*') || pair == ('_', '_') || pair == ('~', '~') {
                i += 2;
                continue;
            }
        }
        // Single-char inline markers: * _ `
        if matches!(c, '*' | '_' | '`') {
            i += 1;
            continue;
        }
        if at_line_start {
            // Heading hashes: ###... + space
            if c == '#' {
                while i < chars.len() && chars[i] == '#' {
                    i += 1;
                }
                while i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                continue;
            }
            // Unordered list markers
            if (c == '-' || c == '+' || c == '*')
                && i + 1 < chars.len()
                && chars[i + 1] == ' '
            {
                i += 2;
                continue;
            }
            // Ordered list markers: "1. " or "1) "
            if c.is_ascii_digit() {
                let mut j = i;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j < chars.len()
                    && (chars[j] == '.' || chars[j] == ')')
                    && j + 1 < chars.len()
                    && chars[j + 1] == ' '
                {
                    i = j + 2;
                    continue;
                }
            }
            // Indentation whitespace at line start
            if c == ' ' || c == '\t' {
                i += 1;
                continue;
            }
        }
        // Markdown link: [label](url) → label
        if c == '[' {
            if let Some(close_off) = chars[i + 1..].iter().position(|&ch| ch == ']') {
                let close_idx = i + 1 + close_off;
                if close_idx + 1 < chars.len() && chars[close_idx + 1] == '(' {
                    if let Some(paren_off) =
                        chars[close_idx + 2..].iter().position(|&ch| ch == ')')
                    {
                        out.extend(chars[i + 1..close_idx].iter());
                        i = close_idx + 2 + paren_off + 1;
                        at_line_start = false;
                        continue;
                    }
                }
            }
        }
        out.push(c);
        at_line_start = c == '\n';
        i += 1;
    }
    // Collapse all whitespace runs to single spaces so the original newlines
    // and indentation don't show up as awkward pauses.
    let mut result = String::with_capacity(out.len());
    let mut last_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !last_space {
                result.push(' ');
                last_space = true;
            }
        } else {
            result.push(c);
            last_space = false;
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod md_tests {
    use super::strip_markdown_for_tts;

    #[test]
    fn strips_bold_italic_code() {
        assert_eq!(strip_markdown_for_tts("**hello**"), "hello");
        assert_eq!(strip_markdown_for_tts("*hello*"), "hello");
        assert_eq!(strip_markdown_for_tts("__hello__"), "hello");
        assert_eq!(strip_markdown_for_tts("`hello`"), "hello");
        assert_eq!(strip_markdown_for_tts("~~hello~~"), "hello");
    }

    #[test]
    fn strips_headings_and_lists() {
        assert_eq!(strip_markdown_for_tts("# Heading"), "Heading");
        assert_eq!(strip_markdown_for_tts("## Heading"), "Heading");
        assert_eq!(strip_markdown_for_tts("- item one"), "item one");
        assert_eq!(strip_markdown_for_tts("1. first thing"), "first thing");
        assert_eq!(
            strip_markdown_for_tts("1. **What's the priority?**"),
            "What's the priority?"
        );
    }

    #[test]
    fn strips_links_keep_label() {
        assert_eq!(
            strip_markdown_for_tts("see [the docs](https://example.com)"),
            "see the docs"
        );
    }

    #[test]
    fn collapses_whitespace_and_newlines() {
        let input = "**Wednesday, May 27, 2026 — 21:43 UTC**\n\nClean slate.";
        assert_eq!(
            strip_markdown_for_tts(input),
            "Wednesday, May 27, 2026 — 21:43 UTC Clean slate."
        );
    }
}

// =============================================================================
// Gapless audio queue
// =============================================================================

/// Plays a stream of wav blobs back-to-back with no gap. New blobs are
/// scheduled at the end of the previously-scheduled blob's playback time so
/// the browser audio thread handles the transition.
pub struct AudioQueue {
    ctx: AudioContext,
    /// When (in AudioContext time) the next blob should start.
    next_start: f64,
    /// (source, scheduled_start_time, scheduled_end_time) so we can decide
    /// which sources are still upcoming when we halt mid-stream, and so we
    /// can rewind `next_start` to the end of the currently-playing source
    /// when a barge-in cancels everything queued after it.
    sources: Vec<(web_sys::AudioBufferSourceNode, f64, f64)>,
    /// If true, new enqueues route into `pending` instead of being
    /// scheduled. Set by `halt_after_current`. Cleared by `resume()`.
    halted: bool,
    /// Buffer for PCM chunks that arrive while halted. Drained on
    /// `resume()`. Discarded on `halt_immediately()` (real user barge-in
    /// confirmed). This is the fix for "JARVIS cuts off mid-sentence and
    /// then starts mid-paragraph" — the in-flight ElevenLabs stream
    /// keeps shipping chunks for ~1–2s after barge-in fires, and dropping
    /// them silently loses sentences 2-N of the current response.
    pending: VecDeque<(Vec<f32>, u32)>,
    /// Cap on pending buffer to keep memory bounded if halted persists.
    /// ~20s of 24kHz mono float32 ≈ 1.9 MB.
    pending_max: usize,
}

impl AudioQueue {
    /// Construct the queue. MUST be called from inside a user-gesture handler
    /// (click, keypress) — otherwise Chrome creates the AudioContext in
    /// `suspended` state and nothing will play. We also fire `.resume()` as a
    /// belt-and-braces measure; on a fresh page that requires the gesture to
    /// land first, but on a stale page it un-suspends a previously-suspended
    /// context.
    pub fn new() -> Result<Self, String> {
        let ctx = AudioContext::new().map_err(|e| format!("audioctx: {:?}", e))?;
        // Fire-and-forget resume; the promise resolves to undefined and the
        // state flips to "running". If the context was created in a suspended
        // state because the gesture hadn't landed yet, this is the only way
        // out of it.
        if let Ok(promise) = ctx.resume() {
            let _ = promise; // Promise is JsValue; we don't need to await.
        }
        console_log(&format!("[audio-queue] new(); state={:?}", ctx.state()));
        let now = ctx.current_time();
        Ok(Self {
            ctx,
            next_start: now,
            sources: Vec::new(),
            halted: false,
            pending: VecDeque::new(),
            pending_max: 4096,
        })
    }

    /// Decode + schedule a wav blob to play immediately after whatever is
    /// already queued. No-op when the queue is halted (barge-in in progress).
    /// Enqueue raw float32 PCM (mono) at `sample_rate`. Skips wav decode
    /// entirely — used by the streaming-TTS path where each chunk lands as
    /// pre-decoded float samples. Constructs an `AudioBuffer` directly via
    /// `createBuffer` + `copyToChannel`. Same gapless scheduling as
    /// `enqueue_wav`.
    pub fn enqueue_pcm(&mut self, samples: &[f32], sample_rate: u32) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        // While halted (a barge-in is in flight), buffer the chunk instead
        // of dropping it. resume() drains; halt_immediately() discards.
        // This prevents the "JARVIS cuts mid-sentence then starts at
        // sentence 4" failure mode that happens when JARVIS's own audio
        // trips the RMS gate, fires on_barge_in, and the in-flight
        // ElevenLabs stream is still pumping chunks.
        if self.halted {
            if self.pending.len() < self.pending_max {
                self.pending.push_back((samples.to_vec(), sample_rate));
            } else {
                // Buffer full — drop oldest so the most recent audio
                // survives. Silero verdict should have arrived by now;
                // something else is wedged.
                self.pending.pop_front();
                self.pending.push_back((samples.to_vec(), sample_rate));
            }
            return Ok(());
        }
        if let Ok(promise) = self.ctx.resume() {
            let _ = promise;
        }
        let buffer = self
            .ctx
            .create_buffer(1, samples.len() as u32, sample_rate as f32)
            .map_err(|e| format!("create_buffer: {:?}", e))?;
        // `copy_to_channel` wants `&mut [f32]`. The samples we have are
        // immutable; copy into a local mut buffer first.
        let mut owned = samples.to_vec();
        buffer
            .copy_to_channel(&mut owned[..], 0)
            .map_err(|e| format!("copy_to_channel: {:?}", e))?;
        let source = self
            .ctx
            .create_buffer_source()
            .map_err(|e| format!("source: {:?}", e))?;
        source.set_buffer(Some(&buffer));
        source
            .connect_with_audio_node(&self.ctx.destination())
            .map_err(|e| format!("connect: {:?}", e))?;
        let now = self.ctx.current_time();
        let start_time = self.next_start.max(now);
        source
            .start_with_when(start_time)
            .map_err(|e| format!("start: {:?}", e))?;
        let end_time = start_time + buffer.duration();
        self.next_start = end_time;
        self.sources.push((source, start_time, end_time));
        Ok(())
    }

    pub async fn enqueue_wav(&mut self, wav: &[u8]) -> Result<(), String> {
        if self.halted {
            return Ok(());
        }
        // Resume if the context drifted into suspended (happens when the tab
        // backgrounds, or after long idle periods).
        if let Ok(promise) = self.ctx.resume() {
            let _ = JsFuture::from(promise).await;
        }
        console_log(&format!(
            "[audio-queue] enqueue {} bytes; state={:?}",
            wav.len(),
            self.ctx.state()
        ));
        let array = js_sys::Uint8Array::from(wav);
        let array_buffer = array.buffer();
        let decoded_js = JsFuture::from(
            self.ctx
                .decode_audio_data(&array_buffer)
                .map_err(|e| format!("decode: {:?}", e))?,
        )
        .await
        .map_err(|e| format!("decode-await: {:?}", e))?;
        let buffer: AudioBuffer = decoded_js
            .dyn_into()
            .map_err(|_| "not AudioBuffer".to_string())?;

        let source = self
            .ctx
            .create_buffer_source()
            .map_err(|e| format!("source: {:?}", e))?;
        source.set_buffer(Some(&buffer));
        source
            .connect_with_audio_node(&self.ctx.destination())
            .map_err(|e| format!("connect: {:?}", e))?;

        let now = self.ctx.current_time();
        let start_time = self.next_start.max(now);
        source
            .start_with_when(start_time)
            .map_err(|e| format!("start: {:?}", e))?;

        let end_time = start_time + buffer.duration();
        self.next_start = end_time;
        self.sources.push((source, start_time, end_time));
        Ok(())
    }

    /// Hard stop: silence immediately, drop the schedule. Used when the user
    /// signals "stop, listen to me NOW" (or when Silero confirms a real
    /// barge-in, not music). Also discards the pending buffer — the user
    /// genuinely wants this turn dead.
    pub fn halt_immediately(&mut self) {
        self.halted = true;
        self.pending.clear();
        let now = self.ctx.current_time();
        for (src, _, _) in self.sources.drain(..) {
            let _ = src.stop_with_when(now);
        }
        self.next_start = now;
    }

    /// Graceful stop: let the currently-playing source finish, cancel
    /// everything else. This is the barge-in default — JARVIS shuts up at
    /// the next sentence boundary so we never cut him off mid-word.
    pub fn halt_after_current(&mut self) {
        self.halted = true;
        let now = self.ctx.current_time();
        let mut keep = Vec::new();
        let mut latest_end = now;
        for (src, start, end) in self.sources.drain(..) {
            if start > now {
                // Not started yet — cancel by stopping before/at its start.
                let _ = src.stop_with_when(start);
            } else {
                // Already playing — let it finish naturally.
                if end > latest_end {
                    latest_end = end;
                }
                keep.push((src, start, end));
            }
        }
        self.sources = keep;
        // Rewind next_start to the end of what's actually still playing.
        // Without this, next_start still points at the end of the cancelled
        // future sentences (several seconds out), so when resume() fires
        // for the next response, new audio gets scheduled in that dead zone
        // and the user hears multi-second latency before JARVIS speaks.
        self.next_start = latest_end;
    }

    /// Re-open the queue for new enqueues. Called by `queue_sentence` for
    /// every fresh JARVIS sentence AND by `on_silero_reject` when a
    /// phantom barge-in (music tripping RMS) gets walked back. Drains
    /// any chunks that arrived while we were halted so the user hears
    /// the full response, not a clipped fragment.
    pub fn resume(&mut self) {
        self.halted = false;
        // Push next_start forward so the resumed audio starts after the
        // currently-playing source (if any) finishes.
        let now = self.ctx.current_time();
        self.next_start = self.next_start.max(now);
        // Drain buffered chunks via the normal scheduling path. We
        // recursively call enqueue_pcm — safe because halted is now
        // false, so the new calls hit the schedule branch instead of
        // re-buffering. take() avoids borrow conflicts.
        let pending = std::mem::take(&mut self.pending);
        for (samples, sr) in pending {
            if let Err(e) = self.enqueue_pcm(&samples, sr) {
                console_log(&format!("[audio-queue] resume drain failed: {e}"));
            }
        }
    }

    /// Insert a silent gap before the next enqueued PCM chunk. Used between
    /// sentences so JARVIS sounds like he's breathing instead of stitching
    /// audio buffers back-to-back. We don't actually queue a silence
    /// buffer — we just bump `next_start` forward, and the WebAudio clock
    /// gives us silence for free in the gap.
    ///
    /// No-op when halted (we're mid-barge-in and don't want to extend the
    /// queue's reach) or when the queue is already idle (the gap would land
    /// in the past and clip away).
    pub fn add_gap(&mut self, gap_secs: f64) {
        if self.halted || gap_secs <= 0.0 {
            return;
        }
        let now = self.ctx.current_time();
        if self.next_start <= now {
            // Queue is drained; a gap here would just be silence before
            // audio starts, which the user already perceives as latency.
            // Skip so a long-idle queue doesn't accumulate phantom delay.
            return;
        }
        self.next_start += gap_secs;
    }

    /// Total queued duration ahead of the audio clock (seconds). 0 when
    /// nothing's queued.
    #[allow(dead_code)]
    pub fn queued_seconds(&self) -> f64 {
        let now = self.ctx.current_time();
        (self.next_start - now).max(0.0)
    }
}

/// Play a wav byte blob via a hidden `<audio>` element. Fire-and-forget.
pub fn play_wav(bytes: &[u8]) -> Result<HtmlAudioElement, String> {
    let arr = Uint8Array::from(bytes);
    let parts = Array::new();
    parts.push(&arr.buffer());
    let props = BlobPropertyBag::new();
    props.set_type("audio/wav");
    let blob = Blob::new_with_buffer_source_sequence_and_options(&parts, &props)
        .map_err(|e| format!("blob: {:?}", e))?;
    let url = Url::create_object_url_with_blob(&blob).map_err(|e| format!("url: {:?}", e))?;

    let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let elem = document
        .create_element("audio")
        .map_err(|e| format!("create: {:?}", e))?;
    let audio: HtmlAudioElement = elem.dyn_into().map_err(|_| "cast".to_string())?;
    audio.set_src(&url);

    // Revoke the object URL once playback ends so we don't leak blobs.
    let url_for_revoke = url.clone();
    let on_ended = Closure::once_into_js(move || {
        let _ = Url::revoke_object_url(&url_for_revoke);
    });
    let _ = js_sys::Reflect::set(
        audio.as_ref(),
        &JsValue::from_str("onended"),
        &on_ended,
    );

    let play_promise = audio.play().map_err(|e| format!("play: {:?}", e))?;
    // Promise rejections (e.g., browser autoplay block) become console errors
    // but don't break us. Caller still gets the element to inspect.
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = JsFuture::from(play_promise).await {
            console_log(&format!("[jarvis-tts] play() rejected: {:?}", e));
        }
    });
    Ok(audio)
}
