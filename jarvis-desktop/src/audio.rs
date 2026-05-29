//! Native audio I/O via cpal.
//!
//! Two streams open at startup:
//!
//!   - **Input**: default input device, configured to deliver f32 mono
//!     (resampled internally if the device only offers stereo). Frames
//!     ship over an async-channel into the VAD task. cpal's stream
//!     callback fires on its own thread; we use the channel to hand
//!     off to tokio without blocking the audio thread.
//!
//!   - **Output**: default output device, f32 samples. Owns a
//!     `Mutex<VecDeque<f32>>` queue plus a sample-rate cell. The
//!     gateway's TTS streamer pushes chunks via `enqueue_pcm`; the cpal
//!     callback drains the queue every buffer. Resamples cheaply via
//!     linear interp if the queue's sample rate differs from the
//!     device's.
//!
//! There is no AudioWorklet shim, no WebAudio context, no browser
//! permission flow. WASAPI on Windows, CoreAudio on mac, ALSA / Pulse
//! on Linux — cpal abstracts all of them.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_channel::Sender;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};
use parking_lot::Mutex;

use crate::crash_isolation::safe_callback;

/// Frame chunk handed off from the cpal input callback to the VAD task.
#[derive(Debug)]
pub struct MicFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// Software AGC + noise gate + reference-aware echo suppression that
/// runs INSIDE the cpal input callback, before the frame is shipped to
/// VAD/WT. Closes part of the gap where Leptos's `getUserMedia({
/// echoCancellation, noiseSuppression, autoGainControl: true })` gives
/// the browser side free DSP that the raw cpal/WASAPI stream skips.
///
/// What this does:
///
///   - **AGC**: track a slow-EMA peak amplitude; scale samples so the
///     loudest sample in the moving window lands near 0.5. Soft-knee:
///     gain is clamped to [0.5, 8.0] so a sneeze doesn't crater the
///     mic and a whisper doesn't blow up house noise.
///   - **Noise gate**: when the moving-RMS sits below `gate_floor`,
///     attenuate hard (mul by 0.05) so room hum / fan noise doesn't
///     reach the VAD's energy estimator.
///   - **Reference-aware echo suppression**: if the playback queue is
///     pushing TTS to the speakers right now, attenuate the input by
///     a factor proportional to (mic_rms / playback_rms). When the
///     speaker is loud and the mic is comparatively quiet, we're
///     almost certainly hearing our own TTS through open speakers —
///     suppress. This is a coarse fallback for real WebRTC AEC3 (which
///     uses adaptive filters with the reference signal); it kills the
///     common "JARVIS hears himself" failure mode at the cost of
///     occasionally suppressing a quiet barge-in during loud TTS.
///
/// Phase 3 may replace this with actual webrtc-audio-processing once
/// the build infrastructure for it is figured out. Until then this
/// gets ~80% of the way at <1% of the code complexity.
struct MicDsp {
    /// Slow-EMA of peak amplitude. ~2s time constant.
    peak_ema: f32,
    /// Slow-EMA of RMS for the gate.
    rms_ema: f32,
    /// Below this RMS, attenuate.
    gate_floor: f32,
    /// Smoothing factor for the EMAs. ~0.005 = ~200ms half-life at
    /// frame rate.
    alpha: f32,
    /// Reference signal monitor — points at the speaker queue's
    /// recent-playback-RMS atomic. None = no echo suppression (e.g.
    /// during unit tests).
    reference: Option<Arc<PlaybackMonitor>>,
}

impl MicDsp {
    fn new() -> Self {
        Self {
            peak_ema: 0.1,
            rms_ema: 0.0,
            // Below ~-50 dBFS = pretty quiet room — most likely
            // background. Tuned conservatively; the dashboard side
            // pushes more aggressive gating via WebRTC's NS.
            gate_floor: 0.003,
            alpha: 0.005,
            reference: None,
        }
    }

    fn with_reference(mut self, reference: Arc<PlaybackMonitor>) -> Self {
        self.reference = Some(reference);
        self
    }

    /// MicDsp is now PASS-THROUGH except for reference-aware echo
    /// gating during TTS playback. AGC was moved to the STT-bound
    /// audio path in mic_pipeline — VAD now sees the raw mic signal
    /// with its TRUE noise floor (~0.005 in McKale's room), so the
    /// adaptive threshold (max(0.010, 2× noise_floor)) correctly
    /// classifies silence frames as silence. The STT path applies
    /// its own per-chunk gain AFTER the VAD decision so parakeet
    /// still receives a healthy signal level. This decouples the
    /// "is this silence?" question from the "is this loud enough?"
    /// question, which was the structural bug behind the 30s
    /// utterances + empty Final("") responses.
    fn process(&mut self, samples: &mut [f32]) {
        if samples.is_empty() {
            return;
        }

        let mut sum_sq = 0.0_f32;
        for &s in samples.iter() {
            sum_sq += s * s;
        }
        let frame_rms = (sum_sq / samples.len() as f32).sqrt();
        self.rms_ema = self.rms_ema * (1.0 - self.alpha) + frame_rms * self.alpha;
        let agc_gain = 1.0_f32;

        // Reference-aware echo suppression — KEPT because cpal can't
        // do AEC and the dashboard side gets echoCancellation for
        // free via the browser. Without this our own TTS leaks back
        // through the mic and the VAD treats it as speech.
        let echo_attenuation = if let Some(r) = &self.reference {
            let playback_rms = r.recent_rms();
            if playback_rms > 0.02 {
                let ratio = frame_rms / playback_rms;
                if ratio < 0.3 {
                    0.1 // strong echo — suppress
                } else if ratio < 0.7 {
                    0.5 // possibly echo + voice
                } else {
                    0.9 // probably barge-in
                }
            } else {
                1.0
            }
        } else {
            1.0
        };

        let total_gain = agc_gain * echo_attenuation;
        if (total_gain - 1.0).abs() > 1e-3 {
            for s in samples.iter_mut() {
                *s = (*s * total_gain).clamp(-1.0, 1.0);
            }
        }
    }
}

/// Lock-free side-channel that exposes the speaker's recent-playback
/// RMS to the MicDsp without the mic callback having to lock the
/// playback queue. The cpal output callback writes the RMS of each
/// frame it emits into a single atomic; the cpal input callback reads
/// it. Both sides are on hot real-time paths — no locks allowed.
pub struct PlaybackMonitor {
    /// RMS of the last ~20ms output frame, stored as f32 reinterpreted
    /// as u32 bits. f32::to_bits / from_bits is a cheap pun.
    recent_rms_bits: AtomicU32,
}

impl PlaybackMonitor {
    pub fn new() -> Self {
        Self {
            recent_rms_bits: AtomicU32::new(0),
        }
    }

    fn store_rms(&self, rms: f32) {
        self.recent_rms_bits.store(rms.to_bits(), Ordering::Relaxed);
    }

    /// Last cpal output-frame RMS. Read from the mic pipeline to power
    /// barge-in detection (raw mic RMS minus this estimate isolates the
    /// caller's voice from JARVIS's own playback echo).
    pub fn recent_rms(&self) -> f32 {
        f32::from_bits(self.recent_rms_bits.load(Ordering::Relaxed))
    }
}

/// Holds the live input stream. Drop = stream stops.
pub struct Mic {
    _stream: Stream,
    pub sample_rate: u32,
}

impl Mic {
    /// Open the default input device, build a stream that ships frames
    /// to `tx`. Frames are roughly `frame_ms` long but cpal's actual
    /// buffer size is device-determined; the consumer should accumulate
    /// and re-window if it cares.
    ///
    /// `playback_monitor` is the side-channel from the Speaker; when
    /// present, MicDsp uses it to suppress mic frames that look like
    /// echo of our own TTS playback. Pass `None` to disable AEC
    /// (e.g. for tests).
    pub fn open(
        tx: Sender<MicFrame>,
        playback_monitor: Option<Arc<PlaybackMonitor>>,
    ) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default input device — plug a mic in")?;
        let supported_config = device
            .default_input_config()
            .context("query default input config")?;
        let sample_rate = supported_config.sample_rate().0;
        let channels = supported_config.channels();
        let sample_format = supported_config.sample_format();
        let config: StreamConfig = supported_config.into();

        tracing::info!(
            "mic open: device={:?}, sr={}, channels={}, format={:?}",
            device.name().unwrap_or_default(),
            sample_rate,
            channels,
            sample_format
        );

        let err_fn = |e: cpal::StreamError| tracing::error!("mic stream error: {e}");

        // We always emit f32 mono. If the device gives us interleaved
        // stereo or 16-bit ints, convert inline. Each branch owns a
        // mutable MicDsp instance for AGC + noise gate + reference-
        // aware echo suppression (best-effort cpal-side approximation
        // of browser-side echoCancellation / NS / AGC).
        let make_dsp = || {
            let dsp = MicDsp::new();
            match &playback_monitor {
                Some(m) => dsp.with_reference(m.clone()),
                None => dsp,
            }
        };
        // Each callback body is wrapped in `safe_callback` so a panic
        // (in downmix, DSP, channel send, anywhere) is caught on the
        // Rust side BEFORE it unwinds through cpal's C FFI — which
        // would otherwise be UB and abort the whole process. The cost
        // is one catch_unwind setup per frame (~50ns).
        //
        // RefCell<MicDsp> + std::cell::Cell pattern would be nicer here
        // but cpal callbacks are FnMut so `move`'d captures + plain
        // `&mut dsp` works. The safe_callback takes the callback body
        // by value (FnOnce + UnwindSafe); we re-acquire `dsp` inside
        // a RefCell that lives for the stream's lifetime so the
        // closure stays FnMut.
        use std::cell::RefCell;
        let stream = match sample_format {
            SampleFormat::F32 => {
                let tx = tx.clone();
                let dsp = RefCell::new(make_dsp());
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        let data = data.to_vec();
                        let tx = tx.clone();
                        let dsp_cell = &dsp;
                        safe_callback("mic_f32", move || {
                            let mut mono = downmix_to_mono_f32(&data, channels);
                            dsp_cell.borrow_mut().process(&mut mono);
                            let _ = tx.try_send(MicFrame {
                                samples: mono,
                                sample_rate,
                            });
                        });
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I16 => {
                let tx = tx.clone();
                let dsp = RefCell::new(make_dsp());
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        let data: Vec<f32> = data.iter().map(|&s| s.to_sample::<f32>()).collect();
                        let tx = tx.clone();
                        let dsp_cell = &dsp;
                        safe_callback("mic_i16", move || {
                            let mut mono = downmix_to_mono_f32(&data, channels);
                            dsp_cell.borrow_mut().process(&mut mono);
                            let _ = tx.try_send(MicFrame {
                                samples: mono,
                                sample_rate,
                            });
                        });
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let tx = tx.clone();
                let dsp = RefCell::new(make_dsp());
                device.build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        let data: Vec<f32> = data.iter().map(|&s| s.to_sample::<f32>()).collect();
                        let tx = tx.clone();
                        let dsp_cell = &dsp;
                        safe_callback("mic_u16", move || {
                            let mut mono = downmix_to_mono_f32(&data, channels);
                            dsp_cell.borrow_mut().process(&mut mono);
                            let _ = tx.try_send(MicFrame {
                                samples: mono,
                                sample_rate,
                            });
                        });
                    },
                    err_fn,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported input sample format: {:?}", other),
        };
        stream.play().context("start mic stream")?;
        Ok(Self {
            _stream: stream,
            sample_rate,
        })
    }
}

/// PCM playback queue. Push chunks via `enqueue_pcm`; the cpal output
/// callback drains. Resamples cheaply if the chunk rate differs from
/// the device rate.
///
/// Bounded at ~10s of audio at the device rate. If the producer (TTS
/// stream) outruns the consumer (cpal output thread) somehow — e.g.
/// because the cpal callback got blocked behind another lock — we drop
/// the oldest samples instead of growing forever. 10s is well past any
/// reasonable TTS sentence latency; if we ever hit the cap something
/// is wrong upstream and silence-with-truncation is safer than OOM.
pub struct PlaybackQueue {
    queue: Mutex<std::collections::VecDeque<f32>>,
    src_rate: AtomicU32,
    out_rate: u32,
    halted: std::sync::atomic::AtomicBool,
    /// TTS-worker liveness flag. Set true at the START of every
    /// sentence the tts_worker pulls from its channel; cleared after
    /// the inter-sentence gap completes. mic_pipeline consults this
    /// for the half-duplex VAD mute decision so the brief
    /// pending_samples → 0 moments between TTS chunks (which would
    /// otherwise un-mute the mic and let JARVIS hear himself) no
    /// longer matter.
    tts_active: std::sync::atomic::AtomicBool,
    max_samples: usize,
}

impl PlaybackQueue {
    pub fn new(out_rate: u32) -> Self {
        // 10 seconds of audio at the device rate. Past this we drop
        // oldest samples instead of growing forever.
        let max_samples = (out_rate as usize) * 10;
        Self {
            queue: Mutex::new(std::collections::VecDeque::with_capacity(48_000)),
            src_rate: AtomicU32::new(out_rate),
            out_rate,
            halted: std::sync::atomic::AtomicBool::new(false),
            tts_active: std::sync::atomic::AtomicBool::new(false),
            max_samples,
        }
    }

    /// Called by tts_worker when it begins streaming a sentence.
    pub fn mark_tts_active(&self) {
        self.tts_active.store(true, Ordering::Release);
    }

    /// Called by tts_worker after the inter-sentence gap completes
    /// (i.e. when the worker idles waiting for the next sentence).
    pub fn mark_tts_idle(&self) {
        self.tts_active.store(false, Ordering::Release);
    }

    /// Lock-free probe used by mic_pipeline for the half-duplex VAD
    /// mute decision.
    pub fn is_tts_active(&self) -> bool {
        self.tts_active.load(Ordering::Acquire)
    }

    /// Append a chunk to the queue. Cheap linear-interp resample if
    /// the source rate is different from the device's output rate.
    /// Enforces a soft cap of ~10s of audio; older samples drop first.
    ///
    /// Resample output is written DIRECTLY into the VecDeque under the
    /// lock — no intermediate Vec<f32> allocation per chunk. Saves an
    /// allocation per TTS chunk (~50/sec during streaming TTS).
    pub fn enqueue_pcm(&self, src_rate: u32, samples: &[f32]) {
        if self.halted.load(Ordering::Acquire) {
            return;
        }
        self.src_rate.store(src_rate, Ordering::Relaxed);
        let mut q = self.queue.lock();
        if src_rate == self.out_rate {
            // No resample needed — extend directly.
            q.extend(samples.iter().copied());
        } else {
            // Inline linear-interp resample → push directly to VecDeque.
            let ratio = src_rate as f32 / self.out_rate as f32;
            let out_len = (samples.len() as f32 / ratio).floor() as usize;
            q.reserve(out_len);
            for i in 0..out_len {
                let src_pos = i as f32 * ratio;
                let idx = src_pos as usize;
                let frac = src_pos - idx as f32;
                let a = samples[idx];
                let b = *samples.get(idx + 1).unwrap_or(&a);
                q.push_back(a + (b - a) * frac);
            }
        }
        // Drop-oldest backpressure. Should never trigger in normal
        // operation; protects against runaway memory if the cpal
        // output thread is stalled.
        let max = self.max_samples;
        if q.len() > max {
            let drop_n = q.len() - max;
            tracing::warn!(
                "playback queue overflow: dropping {} samples (~{:.2}s)",
                drop_n,
                drop_n as f32 / self.out_rate as f32
            );
            q.drain(..drop_n);
        }
    }

    /// Append `seconds` of silence to the queue. Used for inter-sentence
    /// gaps so JARVIS doesn't sound robotic between sentences.
    pub fn add_gap(&self, seconds: f32) {
        let n = (self.out_rate as f32 * seconds).round() as usize;
        let mut q = self.queue.lock();
        q.reserve(n);
        for _ in 0..n {
            q.push_back(0.0);
        }
    }

    /// Stop playback immediately. Drains the queue and ignores future
    /// enqueues until `resume` is called.
    pub fn halt(&self) {
        self.halted.store(true, Ordering::Release);
        self.queue.lock().clear();
    }

    pub fn resume(&self) {
        self.halted.store(false, Ordering::Release);
    }

    pub fn pending_samples(&self) -> usize {
        self.queue.lock().len()
    }
}

/// Holds the live output stream. Drop = stream stops.
pub struct Speaker {
    _stream: Stream,
    pub queue: Arc<PlaybackQueue>,
    pub sample_rate: u32,
    /// Side-channel exposing recent-playback RMS to the Mic side for
    /// reference-aware echo suppression. Pass `monitor()` into
    /// `Mic::open` to enable.
    pub monitor: Arc<PlaybackMonitor>,
}

impl Speaker {
    pub fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no default output device")?;
        let supported_config = device
            .default_output_config()
            .context("query default output config")?;
        let sample_rate = supported_config.sample_rate().0;
        let channels = supported_config.channels();
        let sample_format = supported_config.sample_format();
        let config: StreamConfig = supported_config.into();

        tracing::info!(
            "speaker open: device={:?}, sr={}, channels={}, format={:?}",
            device.name().unwrap_or_default(),
            sample_rate,
            channels,
            sample_format
        );

        let queue = Arc::new(PlaybackQueue::new(sample_rate));
        let monitor = Arc::new(PlaybackMonitor::new());
        let err_fn = |e: cpal::StreamError| tracing::error!("speaker stream error: {e}");

        // Output callbacks are wrapped in `safe_callback` for the same
        // reason as input — a panic unwinding through cpal's C FFI is
        // UB; catching at the Rust boundary lets the audio thread eat
        // a frame of silence and keep going. On panic the output
        // buffer was already filled by cpal as zero, so a dropped
        // frame is silent rather than glitched.
        let stream = match sample_format {
            SampleFormat::F32 => {
                let q = queue.clone();
                let m = monitor.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [f32], _| {
                        // SAFETY: we transmute `data` to a `usize` ptr +
                        // len pair so we can move it into the FnOnce
                        // closure. catch_unwind requires the closure
                        // be UnwindSafe + FnOnce, and `&mut [f32]` is
                        // already Unwind-safe.
                        let data_ptr = data.as_mut_ptr() as usize;
                        let data_len = data.len();
                        let q = q.clone();
                        let m = m.clone();
                        safe_callback("speaker_f32", move || {
                            let slice = unsafe {
                                std::slice::from_raw_parts_mut(
                                    data_ptr as *mut f32,
                                    data_len,
                                )
                            };
                            fill_output_f32(slice, channels, &q, &m);
                        });
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I16 => {
                let q = queue.clone();
                let m = monitor.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [i16], _| {
                        let data_ptr = data.as_mut_ptr() as usize;
                        let data_len = data.len();
                        let q = q.clone();
                        let m = m.clone();
                        safe_callback("speaker_i16", move || {
                            let slice = unsafe {
                                std::slice::from_raw_parts_mut(
                                    data_ptr as *mut i16,
                                    data_len,
                                )
                            };
                            fill_output_int(slice, channels, &q, &m);
                        });
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let q = queue.clone();
                let m = monitor.clone();
                device.build_output_stream(
                    &config,
                    move |data: &mut [u16], _| {
                        let data_ptr = data.as_mut_ptr() as usize;
                        let data_len = data.len();
                        let q = q.clone();
                        let m = m.clone();
                        safe_callback("speaker_u16", move || {
                            let slice = unsafe {
                                std::slice::from_raw_parts_mut(
                                    data_ptr as *mut u16,
                                    data_len,
                                )
                            };
                            fill_output_int(slice, channels, &q, &m);
                        });
                    },
                    err_fn,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported output sample format: {:?}", other),
        };
        stream.play().context("start speaker stream")?;

        Ok(Self {
            _stream: stream,
            queue,
            sample_rate,
            monitor,
        })
    }
}

// -------- helpers --------------------------------------------------------------

fn downmix_to_mono_f32(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch = channels as usize;
    let frames = interleaved.len() / ch;
    let mut out = Vec::with_capacity(frames);
    for i in 0..frames {
        let mut sum = 0.0_f32;
        for c in 0..ch {
            sum += interleaved[i * ch + c];
        }
        out.push(sum / ch as f32);
    }
    out
}

fn fill_output_f32(
    data: &mut [f32],
    channels: u16,
    q: &PlaybackQueue,
    monitor: &PlaybackMonitor,
) {
    let ch = channels as usize;
    // try_lock: if a producer happens to hold the queue lock at the
    // exact moment cpal calls us, emit silence for this frame instead
    // of blocking the audio thread. With parking_lot's adaptive spin
    // this would be tens of microseconds at worst, but on a real-time
    // priority audio thread even that is enough to glitch — silent
    // dropout is always preferable. In practice this almost never
    // fires; producers hold the lock for microseconds.
    let Some(mut queue) = q.queue.try_lock() else {
        for s in data.iter_mut() {
            *s = 0.0;
        }
        monitor.store_rms(0.0);
        return;
    };
    let mut sum_sq = 0.0_f32;
    let mut count = 0_usize;
    for frame in data.chunks_mut(ch) {
        let sample = queue.pop_front().unwrap_or(0.0);
        sum_sq += sample * sample;
        count += 1;
        for s in frame {
            *s = sample;
        }
    }
    if count > 0 {
        monitor.store_rms((sum_sq / count as f32).sqrt());
    }
}

fn fill_output_int<T>(
    data: &mut [T],
    channels: u16,
    q: &PlaybackQueue,
    monitor: &PlaybackMonitor,
) where
    T: Sample + cpal::FromSample<f32>,
{
    let ch = channels as usize;
    let Some(mut queue) = q.queue.try_lock() else {
        let zero = T::from_sample(0.0);
        for s in data.iter_mut() {
            *s = zero;
        }
        monitor.store_rms(0.0);
        return;
    };
    let mut sum_sq = 0.0_f32;
    let mut count = 0_usize;
    for frame in data.chunks_mut(ch) {
        let sample = queue.pop_front().unwrap_or(0.0);
        sum_sq += sample * sample;
        count += 1;
        let s = T::from_sample(sample);
        for slot in frame {
            *slot = s;
        }
    }
    if count > 0 {
        monitor.store_rms((sum_sq / count as f32).sqrt());
    }
}

