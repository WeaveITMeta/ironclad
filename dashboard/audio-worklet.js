// AudioWorklet PCM pump.
//
// Replaces the AnalyserNode-based sample reader in ContinuousMic. The
// AnalyserNode is a *snapshot* primitive — every read returns the most
// recent N samples, which means consecutive reads overlap and you lose
// inter-snapshot audio. That's fine for visualizers, broken for STT.
//
// AudioWorklet runs on the audio thread and gives us every sample exactly
// once. We downsample from the AudioContext rate (typically 48 kHz on
// Windows / Chrome) to 16 kHz (what Parakeet wants), then post the float32
// buffer to the main thread via the worklet port. The main thread either
// hands it to the VAD (so endpoint detection still works) or fires it
// straight at the WebTransport datagram channel.
//
// Output cadence: ~80 ms chunks at 16 kHz = 1280 samples per message.
// That's small enough to keep RTT low and big enough that we're not
// posting 100+ msgs/sec into the main thread.

const TARGET_SR = 16000;
// 240 samples × 4 bytes/sample = 960 bytes per outbound message. Chrome
// reports maxDatagramSize=1024 on loopback; we go ~6% under to leave
// headroom for any per-frame overhead the browser adds. Cadence is one
// datagram every 15 ms which is plenty fast for live STT.
const POST_INTERVAL_SAMPLES = 240;

class PcmPumpProcessor extends AudioWorkletProcessor {
    constructor() {
        super();
        // AudioContext sample rate is exposed by the global `sampleRate`.
        this._inputRate = sampleRate;
        this._ratio = this._inputRate / TARGET_SR;
        // Fractional decimation accumulator so we don't lose samples by
        // truncating each block independently.
        this._cursor = 0;
        // 80ms ring buffer for resampled output.
        this._outBuf = new Float32Array(POST_INTERVAL_SAMPLES);
        this._outFill = 0;
    }

    /**
     * @param {Float32Array[][]} inputs   inputs[0][0] is the first mono channel
     * @param {Float32Array[][]} _outputs (unused — we don't render audio)
     * @param {Record<string, Float32Array>} _params
     */
    process(inputs, _outputs, _params) {
        const ch = inputs[0]?.[0];
        if (!ch || ch.length === 0) {
            // No input yet (mic still warming up). Keep the worklet alive.
            return true;
        }

        // Linear-interpolation downsample. Browsers' built-in resampler is
        // higher quality but requires reattaching the graph; for STT this is
        // more than good enough.
        while (this._cursor < ch.length) {
            const i = Math.floor(this._cursor);
            const frac = this._cursor - i;
            const a = ch[i];
            const b = i + 1 < ch.length ? ch[i + 1] : a;
            const sample = a + (b - a) * frac;
            this._outBuf[this._outFill++] = sample;
            this._cursor += this._ratio;

            if (this._outFill >= POST_INTERVAL_SAMPLES) {
                // Transfer the buffer to avoid copying (zero-copy postMessage).
                const chunk = this._outBuf;
                this._outBuf = new Float32Array(POST_INTERVAL_SAMPLES);
                this._outFill = 0;
                this.port.postMessage(chunk, [chunk.buffer]);
            }
        }
        // Wrap cursor back into the next block's space.
        this._cursor -= ch.length;

        return true;
    }
}

registerProcessor('pcm-pump', PcmPumpProcessor);
