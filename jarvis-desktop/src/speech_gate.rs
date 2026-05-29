//! Silero VAD — second-stage speech gate.
//!
//! Wraps a Silero VAD v5 ONNX model and exposes a single thread-safe
//! `Gate` instance with two methods: `score_frame` for per-frame
//! barge-in checks and `score_utterance` for end-of-utterance gating.
//!
//! Silero distinguishes human speech from music, JARVIS's own TTS
//! voice, noise, and ambient signal. The dashboard ran it as
//! `silero.js`; this is the native Rust port via `tract-onnx` (pure
//! Rust, no native ORT binary needed).
//!
//! Failure mode: if the model can't initialize, we fail OPEN
//! (return 1.0). Better to let a real utterance through to parakeet
//! and have it discarded downstream than to silence the user because
//! the gate is broken.

use std::sync::OnceLock;

use ndarray::{Array1, Array2, Array3};
use parking_lot::Mutex;
use tract_onnx::prelude::*;

const MODEL_BYTES: &[u8] = include_bytes!("../assets/models/silero_vad.onnx");

/// Silero v5 expects 16 kHz audio in 512-sample chunks (32 ms).
const SR: i64 = 16_000;
const CHUNK: usize = 512;

type SileroModel = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

struct GateState {
    model: SileroModel,
    /// Recurrent state — Silero v5 uses a single 2x1x128 hidden state
    /// tensor. We thread it across chunks within one utterance window
    /// and reset between separate calls.
    state: Array3<f32>,
}

static GATE: OnceLock<Option<Mutex<GateState>>> = OnceLock::new();

fn init_gate() -> Option<Mutex<GateState>> {
    // tract: load the ONNX file from a byte slice → typed → optimize → make_runnable.
    let mut cursor = std::io::Cursor::new(MODEL_BYTES);
    let model = onnx().model_for_read(&mut cursor).ok()?;
    let model = model
        .with_input_fact(0, f32::fact([1, CHUNK]).into())
        .ok()?
        .with_input_fact(1, f32::fact([2, 1, 128]).into())
        .ok()?
        .with_input_fact(2, i64::fact([1]).into())
        .ok()?
        .into_optimized()
        .ok()?
        .into_runnable()
        .ok()?;
    Some(Mutex::new(GateState {
        model,
        state: Array3::<f32>::zeros((2, 1, 128)),
    }))
}

fn gate() -> Option<&'static Mutex<GateState>> {
    GATE.get_or_init(|| match init_gate() {
        Some(g) => Some(g),
        None => {
            tracing::warn!("Silero VAD failed to initialize — gate disabled (fail-open)");
            None
        }
    })
    .as_ref()
}

/// Run one Silero chunk inference. `samples` MUST be exactly 512
/// f32 samples at 16 kHz. Updates the recurrent state in place.
fn run_chunk(state: &mut GateState, samples: &[f32]) -> Option<f32> {
    if samples.len() != CHUNK {
        return None;
    }
    let audio = Array2::<f32>::from_shape_vec((1, CHUNK), samples.to_vec()).ok()?;
    let sr = Array1::<i64>::from_vec(vec![SR]);

    let inputs = tvec!(
        audio.into_tensor().into(),
        state.state.clone().into_tensor().into(),
        sr.into_tensor().into(),
    );

    let outputs = state.model.run(inputs).ok()?;
    // out[0] = speech probability (shape [1, 1]), out[1] = new state ([2, 1, 128])
    let prob_tensor = outputs.first()?;
    let prob: f32 = *prob_tensor.to_array_view::<f32>().ok()?.iter().next()?;

    if let Some(new_state) = outputs.get(1) {
        if let Ok(view) = new_state.to_array_view::<f32>() {
            if let Ok(new_arr) = view.to_owned().into_shape_with_order((2, 1, 128)) {
                state.state = new_arr;
            }
        }
    }

    Some(prob)
}

/// Decimate 48 kHz mono PCM to 16 kHz by 3:1 averaging. Fast, fine
/// for VAD (no antialias filter needed at this scale; Silero is
/// robust to the resulting modest aliasing).
fn decimate_48_to_16(input: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(input.len() / 3 + 1);
    let mut i = 0;
    while i + 3 <= input.len() {
        out.push((input[i] + input[i + 1] + input[i + 2]) / 3.0);
        i += 3;
    }
    out
}

/// Resample arbitrary-rate input to 16 kHz f32 via linear interp.
/// Cheaper than rubato and good enough for a VAD frontend.
fn resample_to_16k(input: &[f32], src_rate: u32) -> Vec<f32> {
    if src_rate == 16_000 {
        return input.to_vec();
    }
    if src_rate == 48_000 {
        return decimate_48_to_16(input);
    }
    let ratio = src_rate as f32 / 16_000.0;
    let out_len = (input.len() as f32 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f32 * ratio;
        let idx = src_pos as usize;
        if idx + 1 >= input.len() {
            break;
        }
        let frac = src_pos - idx as f32;
        let a = input[idx];
        let b = input[idx + 1];
        out.push(a + (b - a) * frac);
    }
    out
}

/// Score an entire utterance buffer. Returns the maximum per-chunk
/// speech probability across the buffer — the dashboard's
/// silero_score equivalent. State is reset before scoring so each
/// call is independent.
///
/// Music sits around 0.05–0.15. Pure noise around 0.01–0.05.
/// JARVIS's own TTS voice through speakers scores HIGH (it IS
/// speech) — combine this gate with the playback-RMS heuristic
/// in mic_pipeline for the "ignore own voice" pass.
///
/// Fail-open: returns 1.0 if the model didn't load or inference
/// errored. Mirrors `silero.js` behavior.
pub fn score(samples: &[f32], sample_rate: u32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let Some(gate) = gate() else {
        return 1.0;
    };
    let resampled = resample_to_16k(samples, sample_rate);
    if resampled.len() < CHUNK {
        // Too short to run even one inference window — pad with zeros
        // (silence) and let Silero decide. Padding speech-detector
        // input is what silero.js does too.
        let mut padded = resampled.clone();
        padded.resize(CHUNK, 0.0);
        let mut g = gate.lock();
        g.state.fill(0.0);
        return run_chunk(&mut g, &padded).unwrap_or(1.0);
    }
    let mut g = gate.lock();
    g.state.fill(0.0);
    let mut max_p: f32 = 0.0;
    let mut i = 0;
    while i + CHUNK <= resampled.len() {
        if let Some(p) = run_chunk(&mut g, &resampled[i..i + CHUNK]) {
            if p > max_p {
                max_p = p;
            }
        }
        i += CHUNK;
    }
    if max_p == 0.0 { 1.0 } else { max_p }
}

/// True iff the buffer looks like speech. Threshold 0.5 matches the
/// Leptos dashboard's `silero.js` default.
#[allow(dead_code)]
pub fn is_speech(samples: &[f32], sample_rate: u32) -> bool {
    score(samples, sample_rate) >= 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_scores_zero() {
        assert_eq!(score(&[], 16_000), 0.0);
    }

    #[test]
    fn silence_scores_low() {
        let silent = vec![0.0_f32; 16_000]; // 1 second of silence
        let s = score(&silent, 16_000);
        // Silence should score well below speech threshold. We don't
        // assert exact value (Silero v5 isn't fully deterministic on
        // a fresh-state cold start) — just that it's not flagged as
        // confident speech.
        assert!(s < 0.5, "silence scored {} (expected <0.5)", s);
    }
}
