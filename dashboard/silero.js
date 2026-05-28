// Silero VAD shim. Wraps @ricky0123/vad-web's NonRealTimeVAD so the Rust
// side can score an utterance with a single async call instead of dealing
// with the async-iterator API directly.
//
// Wire: voice.rs (VAD endpoint hit) -> resamples to 16k float32 ->
//       window.__sileroScore(samples) -> 0..1 -> if < 0.5 drop, else emit.
//
// Why a gate, not a replacement: the existing RMS VAD does utterance
// segmentation well; we keep it for capture timing. Silero only decides
// "is the captured audio actually speech?" — if music or noise tripped
// the RMS threshold but Silero says "not speech," we drop. This kills the
// "JARVIS responds to song lyrics" failure mode without breaking the
// timing logic.
(function () {
    "use strict";

    let vadInstance = null;
    let initPromise = null;

    async function ensureInit() {
        if (vadInstance) return vadInstance;
        if (!initPromise) {
            initPromise = (async () => {
                if (!window.vad || !window.vad.NonRealTimeVAD) {
                    throw new Error("@ricky0123/vad-web not loaded on window.vad");
                }
                const t0 = performance.now();
                vadInstance = await window.vad.NonRealTimeVAD.new();
                console.log(
                    `[silero] NonRealTimeVAD ready in ${Math.round(performance.now() - t0)}ms`
                );
                return vadInstance;
            })().catch((e) => {
                // Reset so a transient failure (slow CDN) can be retried.
                initPromise = null;
                throw e;
            });
        }
        return initPromise;
    }

    // Returns a number in [0, 1]: the fraction of the input audio that
    // Silero classified as containing speech. 1.0 = entirely speech; 0.0
    // = entirely non-speech. Caller picks a threshold.
    //
    // `samples16k` must be Float32 PCM at 16 kHz. Mono.
    //
    // Failure mode: if the model can't initialize (CDN down, ort.js
    // missing, etc.) we return 1.0 — fail-OPEN, never silence the user
    // because the gate is broken.
    window.__sileroScore = async function (samples16k) {
        try {
            const v = await ensureInit();
            const f32 =
                samples16k instanceof Float32Array
                    ? samples16k
                    : new Float32Array(samples16k);
            const totalSamples = f32.length;
            if (totalSamples === 0) return 0.0;
            // seg.start/end are MILLISECONDS, not sample indices. Work in ms
            // on both sides so the ratio is dimensionally correct.
            const totalMs = (totalSamples / 16000) * 1000;
            let speechMs = 0;
            for await (const seg of v.run(f32, 16000)) {
                speechMs += seg.end - seg.start;
            }
            const ratio = speechMs / totalMs;
            return Math.max(0, Math.min(1, ratio));
        } catch (e) {
            console.warn(
                "[silero] scoring failed; fail-open (accepting utterance):",
                e
            );
            return 1.0;
        }
    };
})();
