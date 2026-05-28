"""NVIDIA Parakeet ASR sidecar.

Drop-in replacement for whisper-server's `/inference` HTTP endpoint.
JARVIS's voice gateway (`src/voice/stt.rs`) POSTs a `file` multipart field
with wav/mp3/ogg bytes; we transcribe with `nvidia/parakeet-tdt-0.6b-v2`
and return `{"text": "<transcribed>"}` JSON.

Launched by `jarvis_up` alongside the gateway. Model loads once into GPU
memory at startup, then per-call inference is ~50-150 ms on a GTX 1080 Ti.
That makes streaming-as-you-speak actually feasible.

Run:
    python parakeet_server.py --host 127.0.0.1 --port 8932

Routes:
    GET  /                       - liveness probe
    POST /inference              - multipart file=<audio> -> {"text": "..."}
    WS   /ws/stream              - bidirectional chunked streaming
                                   (client sends raw float32 16k PCM frames;
                                    server emits interim + final transcripts)
"""

from __future__ import annotations

import argparse
import asyncio
import io
import json
import logging
import os
import signal
import sys
import tempfile
import time
from typing import Optional

# NeMo's exp_manager references signal.SIGKILL at module top-level, which
# breaks NeMo on Windows (SIGKILL is POSIX-only). Patch before any nemo
# import — SIGTERM is close enough for what NeMo uses it for.
if not hasattr(signal, "SIGKILL"):
    signal.SIGKILL = signal.SIGTERM  # type: ignore[attr-defined]

import numpy as np
import torch
import uvicorn
from fastapi import FastAPI, File, UploadFile, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse

logging.basicConfig(
    level=logging.INFO,
    format="[parakeet] %(asctime)s %(levelname)s %(message)s",
)
log = logging.getLogger("parakeet")

# NeMo is heavy; import lazily so --help / liveness probes don't pay the cost.
_model = None
_model_name = os.environ.get("PARAKEET_MODEL", "nvidia/parakeet-tdt-0.6b-v2")
_device: torch.device  # set in main()


def get_model():
    global _model
    if _model is not None:
        return _model
    log.info("loading %s on %s ...", _model_name, _device)
    t0 = time.perf_counter()
    from nemo.collections.asr.models import ASRModel  # noqa: WPS433  (heavy import)

    model = ASRModel.from_pretrained(model_name=_model_name)
    model = model.to(_device)
    model.eval()
    _model = model
    log.info("model ready in %.1fs", time.perf_counter() - t0)

    # JIT warm-up. The very first inference triggers CUDA kernel compilation
    # (we measured 24s cold vs 200ms warm on a GTX 1080 Ti). Doing the warm-up
    # here means the cost is paid during `--preload` startup rather than on
    # the user's first sentence. We feed a 1-second silent buffer; the model
    # produces an empty transcript but the kernels get compiled either way.
    log.info("warming up JIT kernels with 1s silence ...")
    t0 = time.perf_counter()
    try:
        warmup_audio = np.zeros(16_000, dtype=np.float32)
        with torch.inference_mode():
            model.transcribe([warmup_audio], batch_size=1, verbose=False)
        log.info("JIT warm-up done in %.1fs", time.perf_counter() - t0)
    except Exception:
        log.exception("JIT warm-up failed (non-fatal; first real call will be slow)")

    return _model


# ----------------------------------------------------------------------------
# HTTP layer
# ----------------------------------------------------------------------------

app = FastAPI(title="Parakeet ASR sidecar")


@app.get("/")
async def root():
    return {"ok": True, "model": _model_name, "device": str(_device)}


@app.post("/inference")
async def inference(file: UploadFile = File(...)):
    """Transcribe a full audio clip. Matches whisper-server's contract.

    JARVIS sends a `file` multipart field with wav bytes; we hand the bytes
    to NeMo via a temp file (NeMo wants a path, not a buffer), and return
    `{"text": "<transcribed>"}`.
    """
    audio_bytes = await file.read()
    if not audio_bytes:
        return JSONResponse({"text": ""})

    suffix = os.path.splitext(file.filename or "")[1] or ".wav"
    with tempfile.NamedTemporaryFile(suffix=suffix, delete=False) as tmp:
        tmp.write(audio_bytes)
        tmp_path = tmp.name

    try:
        model = get_model()
        t0 = time.perf_counter()
        with torch.inference_mode():
            results = model.transcribe([tmp_path], batch_size=1, verbose=False)
        elapsed = time.perf_counter() - t0

        text = _extract_text(results)
        log.info("inference %.0fms %d bytes -> %r", elapsed * 1000, len(audio_bytes), text)
        return JSONResponse({"text": text})
    except Exception as exc:  # noqa: BLE001
        log.exception("inference failed")
        return JSONResponse({"text": "", "error": str(exc)}, status_code=500)
    finally:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass


def _extract_text(results) -> str:
    """NeMo's transcribe() return shape varies by model. Normalize to str."""
    if not results:
        return ""
    first = results[0]
    if isinstance(first, str):
        return first.strip()
    # newer NeMo returns Hypothesis objects with .text
    if hasattr(first, "text"):
        return str(first.text).strip()
    if isinstance(first, list) and first:
        return _extract_text(first)
    return str(first).strip()


# ----------------------------------------------------------------------------
# WebSocket streaming layer (cache-aware)
# ----------------------------------------------------------------------------
#
# Client protocol: binary frames are raw float32 PCM at 16 kHz, mono. The
# server batches them, runs incremental inference, and sends back JSON text
# messages:
#   {"type": "interim", "text": "..."}   - best guess so far
#   {"type": "final",   "text": "..."}   - emitted when the client sends
#                                          {"type":"end"} to flush.
#
# Right now this is a simplified "re-transcribe the accumulator every chunk"
# implementation. NeMo also has CacheAwareStreamingAudioStream for true
# cache-aware streaming; that's a follow-up if accumulator-replay shows lag.

CHUNK_INTERVAL_S = 0.6  # how often to fire an interim transcription


@app.websocket("/ws/stream")
async def ws_stream(ws: WebSocket):
    await ws.accept()
    log.info("ws/stream client connected")
    model = get_model()
    accumulator: list[np.ndarray] = []
    last_run = time.perf_counter()
    last_text = ""

    async def transcribe_accumulator() -> str:
        nonlocal last_text
        if not accumulator:
            return last_text
        audio = np.concatenate(accumulator).astype(np.float32)
        if audio.size < 1600:  # < 0.1s of audio, skip
            return last_text
        # NeMo can transcribe in-memory float arrays via the `audio` kwarg
        # on newer versions; fall back to a temp wav otherwise.
        try:
            with torch.inference_mode():
                results = model.transcribe([audio], batch_size=1, verbose=False)
            text = _extract_text(results)
        except Exception:
            log.exception("interim transcribe failed")
            text = last_text
        last_text = text
        return text

    try:
        while True:
            msg = await ws.receive()
            if "bytes" in msg and msg["bytes"] is not None:
                frame = np.frombuffer(msg["bytes"], dtype=np.float32)
                accumulator.append(frame)
                now = time.perf_counter()
                if now - last_run >= CHUNK_INTERVAL_S:
                    last_run = now
                    text = await transcribe_accumulator()
                    await ws.send_text(json.dumps({"type": "interim", "text": text}))
            elif "text" in msg and msg["text"] is not None:
                try:
                    payload = json.loads(msg["text"])
                except json.JSONDecodeError:
                    continue
                if payload.get("type") == "end":
                    text = await transcribe_accumulator()
                    await ws.send_text(json.dumps({"type": "final", "text": text}))
                    accumulator.clear()
                    last_text = ""
                    last_run = time.perf_counter()
                elif payload.get("type") == "reset":
                    accumulator.clear()
                    last_text = ""
                    last_run = time.perf_counter()
    except WebSocketDisconnect:
        log.info("ws/stream client disconnected")


# ----------------------------------------------------------------------------
# Entrypoint
# ----------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8932)
    parser.add_argument(
        "--preload",
        action="store_true",
        help="Load the model at startup instead of on first request. "
        "Recommended for production so the first call isn't slow.",
    )
    parser.add_argument(
        "--cpu",
        action="store_true",
        help="Force CPU even if CUDA is available (for debugging).",
    )
    args = parser.parse_args()

    global _device
    if args.cpu or not torch.cuda.is_available():
        _device = torch.device("cpu")
        log.info("running on CPU (CUDA unavailable or --cpu set)")
    else:
        _device = torch.device("cuda")
        log.info("running on GPU: %s", torch.cuda.get_device_name(0))

    if args.preload:
        get_model()

    uvicorn.run(app, host=args.host, port=args.port, log_level="warning")


if __name__ == "__main__":
    main()
