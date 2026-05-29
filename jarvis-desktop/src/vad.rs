//! RMS-based VAD with adaptive noise floor + endpoint detection.
//!
//! Ported from `dashboard/src/jarvis/voice.rs` but in native Rust
//! instead of WASM. Same state machine, same tunables (after the
//! 1200ms → 350ms latency fix). Operates on f32 PCM at the device's
//! native sample rate; the resampler downstream of this rebuilds 16 kHz
//! mono for the STT call.
//!
//! State machine:
//!   Idle/Listening → (RMS > threshold for min_speech_ms) → Utterance
//!   Utterance → (RMS < threshold for endpoint_silence_ms) → emit WAV
//!
//! Output: when an utterance closes, the accumulated samples (at native
//! rate) are passed to the caller's closure. The caller handles
//! resampling + STT upload.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct VadConfig {
    pub speech_threshold: f32,
    pub min_speech_ms: u32,
    pub endpoint_silence_ms: u32,
    pub frame_ms: u32,
    pub pre_roll_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            speech_threshold: 0.010,
            // 400ms minimum sustained voice before we promote to an
            // utterance — was 250, raised because background music
            // and brief mouse-click / keyboard / breath transients
            // were promoting too easily and parakeet was emitting
            // hallucinated single-word transcripts. 400ms requires
            // genuinely sustained speech (most words are 200-300ms
            // so this also requires at least 2 syllables roughly).
            min_speech_ms: 400,
            // 1200ms endpoint silence (was 700ms; 350ms before that).
            // The 700ms threshold was still cutting McKale off mid-
            // thought on filler-pause patterns like "Yeah, I'm
            // curious. Um." The actual 2026-05-29 20:47:45 transcript
            // shows JARVIS responding 1s after the "Um" — clearly
            // treating it as end-of-utterance. 1200ms gives natural
            // thinking time without leaving sub-second pauses for
            // utterance latency to suffer. If McKale wants even more
            // patience, tune up; if cutoffs disappear and JARVIS feels
            // slow, tune down.
            endpoint_silence_ms: 1200,
            frame_ms: 40,
            pre_roll_ms: 600,
        }
    }
}

/// Max utterance length before we force-emit even without an endpoint
/// silence. Protects against runaway memory when the room is noisy
/// enough that the VAD never sees the silence boundary (TV, fan, music
/// at conversational volume). 30s at 48kHz = ~5.7MB of samples — still
/// far below any reasonable limit but catches the pathological case.
const MAX_UTTERANCE_MS: u32 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VadState {
    Listening,
    Utterance,
}

/// Single-utterance VAD: feed samples in, get utterance buffers out.
pub struct Vad {
    cfg: VadConfig,
    sample_rate: u32,
    state: VadState,
    samples: Vec<f32>,
    pre_roll: VecDeque<f32>,
    pre_roll_cap: usize,
    voice_run_ms: u32,
    silence_run_ms: u32,
    pre_utt_silence_ms: u32,
    /// Total ms accumulated in the current utterance. Used to enforce
    /// MAX_UTTERANCE_MS — without this an unbounded noisy environment
    /// could grow `samples` until OOM.
    utterance_ms: u32,
    noise_floor_ema: f32,
    muted: bool,
    /// Snapshot of the pre-roll audio captured on the Listening→
    /// Utterance transition. Set inside `feed()` when the state
    /// promotes; consumed once by `take_streaming_preroll()` so the
    /// caller can pipe the missing first ~400-600 ms of speech into
    /// the streaming-STT WebTransport stream. Without this, parakeet
    /// only sees audio AFTER the VAD has committed to the utterance,
    /// which means the first syllables of every utterance — exactly
    /// the part that contains the verb / wake word — are lost and
    /// parakeet hallucinates from the middle of the sentence.
    streaming_preroll_pending: Option<Vec<f32>>,
}

const PRE_UTT_HANGOVER_MS: u32 = 240;

impl Vad {
    pub fn new(cfg: VadConfig, sample_rate: u32) -> Self {
        let pre_roll_cap =
            (sample_rate as f32 * cfg.pre_roll_ms as f32 / 1000.0).round() as usize;
        // Pre-allocate the utterance buffer to typical-case size.
        // Most utterances are 2-5 seconds at native rate (~96k-240k
        // samples at 48kHz). Reserving 240k avoids reallocs during
        // every common utterance.
        let typical_utt = (sample_rate as usize) * 5;
        Self {
            cfg,
            sample_rate,
            state: VadState::Listening,
            samples: Vec::with_capacity(typical_utt),
            pre_roll: VecDeque::with_capacity(pre_roll_cap.max(1024)),
            pre_roll_cap,
            voice_run_ms: 0,
            silence_run_ms: 0,
            pre_utt_silence_ms: 0,
            utterance_ms: 0,
            noise_floor_ema: 0.005,
            muted: false,
            streaming_preroll_pending: None,
        }
    }

    /// Consume the pre-roll snapshot captured on the most recent
    /// Listening→Utterance transition. Returns `None` if there's no
    /// pending snapshot OR if it was already taken. Caller is expected
    /// to call this once per onset event from the mic pipeline so the
    /// streaming-STT path gets the leading audio.
    pub fn take_streaming_preroll(&mut self) -> Option<Vec<f32>> {
        self.streaming_preroll_pending.take()
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if muted {
            // Drop any in-progress utterance to avoid sending half a
            // sentence after the user toggles mute.
            self.reset();
        }
    }

    pub fn state(&self) -> VadState {
        self.state
    }

    /// Feed one frame (`samples` covers `cfg.frame_ms` worth at
    /// `sample_rate`). Returns `Some(utterance)` when the frame closes
    /// an utterance; the utterance buffer holds native-rate samples
    /// including the pre-roll.
    pub fn feed(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        if self.muted || frame.is_empty() {
            return None;
        }

        // RMS over the frame.
        let mut sum_sq = 0.0_f32;
        for &s in frame {
            sum_sq += s * s;
        }
        let rms = (sum_sq / frame.len() as f32).sqrt();

        // Adaptive noise-floor: slow EMA of low-RMS frames.
        if self.state == VadState::Listening && rms < self.cfg.speech_threshold {
            self.noise_floor_ema = self.noise_floor_ema * 0.95 + rms * 0.05;
        }
        let adaptive_threshold = (self.noise_floor_ema * 2.0).max(self.cfg.speech_threshold);
        let voiced = rms > adaptive_threshold;

        // Pre-roll buffer maintenance (only while listening).
        if self.state == VadState::Listening {
            for &s in frame {
                if self.pre_roll.len() >= self.pre_roll_cap {
                    self.pre_roll.pop_front();
                }
                self.pre_roll.push_back(s);
            }
        }

        if voiced {
            self.voice_run_ms = self.voice_run_ms.saturating_add(self.cfg.frame_ms);
            self.silence_run_ms = 0;
            self.pre_utt_silence_ms = 0;
        } else if self.state == VadState::Listening {
            // Hangover: brief silences within forming utterance don't
            // reset the accumulating voice_run_ms counter.
            if self.voice_run_ms > 0 {
                self.pre_utt_silence_ms =
                    self.pre_utt_silence_ms.saturating_add(self.cfg.frame_ms);
                if self.pre_utt_silence_ms > PRE_UTT_HANGOVER_MS {
                    self.voice_run_ms = 0;
                    self.pre_utt_silence_ms = 0;
                }
            }
        } else {
            // In utterance: accumulate silence toward endpoint.
            self.silence_run_ms = self.silence_run_ms.saturating_add(self.cfg.frame_ms);
        }

        // State transitions.
        match self.state {
            VadState::Listening => {
                if self.voice_run_ms >= self.cfg.min_speech_ms {
                    // Promote to Utterance. Snapshot the pre-roll into
                    // both the utterance buffer (for the file-based
                    // STT path that ships the full WAV at endpoint)
                    // AND into `streaming_preroll_pending` (for the
                    // WT path that needs to pipe the leading audio
                    // through immediately so parakeet sees the start
                    // of speech, not just the middle).
                    self.state = VadState::Utterance;
                    self.samples.clear();
                    let pre: Vec<f32> = self.pre_roll.drain(..).collect();
                    self.streaming_preroll_pending = Some(pre.clone());
                    self.samples.extend(pre);
                    self.samples.extend_from_slice(frame);
                }
            }
            VadState::Utterance => {
                self.samples.extend_from_slice(frame);
                self.utterance_ms =
                    self.utterance_ms.saturating_add(self.cfg.frame_ms);
                if self.silence_run_ms >= self.cfg.endpoint_silence_ms {
                    // Normal endpoint: emit + reset.
                    let out = std::mem::take(&mut self.samples);
                    self.reset();
                    return Some(out);
                }
                if self.utterance_ms >= MAX_UTTERANCE_MS {
                    // Force-emit cap. Protects against an environment
                    // where the VAD never sees the silence boundary
                    // (loud constant background). The downstream
                    // speech_gate will likely reject this if it's not
                    // real speech; either way memory stays bounded.
                    tracing::warn!(
                        "VAD force-emit at {} ms (max utterance cap); {} samples",
                        self.utterance_ms,
                        self.samples.len()
                    );
                    let out = std::mem::take(&mut self.samples);
                    self.reset();
                    return Some(out);
                }
            }
        }
        None
    }

    fn reset(&mut self) {
        self.state = VadState::Listening;
        self.samples.clear();
        self.voice_run_ms = 0;
        self.silence_run_ms = 0;
        self.pre_utt_silence_ms = 0;
        self.utterance_ms = 0;
        self.pre_roll.clear();
        self.streaming_preroll_pending = None;
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Encode a chunk of mono float samples as a 16-bit PCM WAV file in
/// memory. STT endpoint accepts WAV; we pick 16 kHz mono to match what
/// the Parakeet sidecar expects natively.
pub fn encode_wav_16khz(samples: &[f32], input_rate: u32) -> Vec<u8> {
    let resampled = if input_rate != 16_000 {
        downsample_to_16k(samples, input_rate)
    } else {
        samples.to_vec()
    };

    let bytes_per_sample = 2u16;
    let num_samples = resampled.len() as u32;
    let byte_rate = 16_000 * bytes_per_sample as u32;
    let block_align = bytes_per_sample;
    let data_size = num_samples * bytes_per_sample as u32;
    let riff_size = 36 + data_size;

    let mut out = Vec::with_capacity(44 + data_size as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    out.extend_from_slice(&1u16.to_le_bytes());  // PCM
    out.extend_from_slice(&1u16.to_le_bytes());  // mono
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for s in resampled {
        let clipped = s.clamp(-1.0, 1.0);
        let v = (clipped * i16::MAX as f32) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Linear interp resample. Good enough for STT — Parakeet doesn't need
/// audiophile-grade resampling. Use rubato for the live mic stream if
/// we want better quality during streaming.
pub fn downsample_to_16k(input: &[f32], in_rate: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let ratio = in_rate as f32 / 16_000.0;
    let out_len = (input.len() as f32 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f32 * ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f32;
        let a = input[idx];
        let b = *input.get(idx + 1).unwrap_or(&a);
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_after_silence_block() {
        let mut vad = Vad::new(VadConfig::default(), 16_000);
        // ~150ms of "voice" (high amplitude).
        for _ in 0..6 {
            let mut frame = vec![0.0; 640]; // 40ms at 16kHz = 640 samples
            for s in frame.iter_mut() {
                *s = 0.3;
            }
            assert!(vad.feed(&frame).is_none());
        }
        assert_eq!(vad.state(), VadState::Utterance);

        // 400ms of silence (above the 350ms endpoint).
        let silence = vec![0.0; 640];
        let mut emitted = None;
        for _ in 0..10 {
            if let Some(buf) = vad.feed(&silence) {
                emitted = Some(buf);
                break;
            }
        }
        assert!(emitted.is_some());
        assert_eq!(vad.state(), VadState::Listening);
    }

    #[test]
    fn wav_header_writes_correct_sample_rate() {
        let samples = vec![0.1; 8000];
        let wav = encode_wav_16khz(&samples, 16_000);
        // bytes 24..28 hold the sample rate as u32 LE
        let sr = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        assert_eq!(sr, 16_000);
    }
}
