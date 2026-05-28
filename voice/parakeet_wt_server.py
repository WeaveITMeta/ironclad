"""WebTransport (HTTP/3 over QUIC) streaming endpoint for Parakeet.

Why this instead of WebSocket: on a lossy network QUIC avoids TCP head-of-line
blocking, and datagrams skip retransmit overhead entirely. On localhost the
latency difference is sub-millisecond either way, but we want the modern
stack and unreliable datagrams for outbound audio frames.

Protocol over the wire (single WebTransport session per browser tab):
  Browser -> Server  (datagrams):
      raw float32-LE PCM @ 16 kHz, mono, ~80ms per datagram (~1280 samples,
      ~5 KB). Datagrams are unreliable; a dropped 80ms frame is acceptable
      because Parakeet decodes from accumulated context anyway.

  Browser -> Server  (unidirectional stream, "control" stream):
      one JSON line per message, e.g.
        {"type":"start"}
        {"type":"end"}     - flush accumulator, return a final transcript
        {"type":"reset"}   - drop accumulator and last_text

  Server -> Browser  (unidirectional stream, "events" stream):
      one JSON line per message:
        {"type":"interim","text":"..."}
        {"type":"final",  "text":"..."}

Boot:
    python parakeet_wt_server.py \
        --host 127.0.0.1 --port 4443 \
        --cert cert.pem --key key.pem

If the cert files don't exist, we generate a self-signed one (good for the
browser's `serverCertificateHashes` API; never trust this for anything beyond
loopback). The hash is written to `cert.sha256` next to the cert so the
dashboard can read it at boot.
"""

from __future__ import annotations

import argparse
import asyncio
import datetime as dt
import hashlib
import json
import logging
import os
import signal
import struct
import sys
import threading
import time
from pathlib import Path
from typing import Dict, Optional

# Process-global inference lock. NeMo's `model.transcribe()` mutates the
# encoder's freeze state (freeze before, unfreeze(partial=True) after) and is
# NOT thread-safe. Without this lock, concurrent transcribes from the
# interim-scheduler and the finalize-handler land on different executor
# threads and corrupt the encoder's state, surfacing as
# `ValueError: Cannot unfreeze partially without first freezing the module`.
# Symptom on the dashboard: the transcript "deletes itself" because the
# committed-prefix chunk got sliced off the window before the failed
# transcribe could add its text to committed_text.
_INFERENCE_LOCK = threading.Lock()

# NeMo's exp_manager hits signal.SIGKILL at import — POSIX-only, breaks on
# Windows. Patch before any nemo import; SIGTERM is functionally equivalent
# for everything NeMo uses SIGKILL for.
if not hasattr(signal, "SIGKILL"):
    signal.SIGKILL = signal.SIGTERM  # type: ignore[attr-defined]

import numpy as np
import torch
from aioquic.asyncio import QuicConnectionProtocol, serve
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import (
    DatagramReceived,
    DataReceived,
    H3Event,
    HeadersReceived,
    WebTransportStreamDataReceived,
)
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import ConnectionTerminated, ProtocolNegotiated, QuicEvent
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

logging.basicConfig(
    level=logging.INFO,
    format="[parakeet-wt] %(asctime)s %(levelname)s %(message)s",
)
log = logging.getLogger("parakeet-wt")

# ----------------------------------------------------------------------------
# Self-signed cert (regenerated only when files don't exist; the hash is what
# the browser pins via `serverCertificateHashes`).
# ----------------------------------------------------------------------------

# Chrome's serverCertificateHashes accepts only:
#   * ECDSA with P-256 (no RSA, no P-384)
#   * SHA-256 signature
#   * validity strictly less than 14 days (1209600 s)
# We use 13 days so a near-midnight clock skew doesn't push us over.
CERT_VALID_DAYS = 13


def ensure_cert(cert_path: Path, key_path: Path, hash_path: Path) -> bytes:
    """Generate a self-signed ECDSA-P256 cert pinned to 127.0.0.1 if missing
    or expired. Returns SHA-256(DER(cert)) which the browser uses to pin via
    `serverCertificateHashes`. The hex form lands in `hash_path` for the
    dashboard to fetch via the gateway."""
    needs_regen = True
    if cert_path.exists() and key_path.exists():
        with open(cert_path, "rb") as f:
            try:
                existing = x509.load_pem_x509_certificate(f.read())
                not_after = (
                    existing.not_valid_after_utc
                    if hasattr(existing, "not_valid_after_utc")
                    else existing.not_valid_after.replace(tzinfo=dt.timezone.utc)
                )
                # Refresh proactively when we're within 1 day of expiry.
                if not_after > dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=1):
                    # Also verify it's an ECDSA cert; an old RSA cert from a
                    # previous run won't satisfy Chrome's WT requirements.
                    if isinstance(
                        existing.public_key(),
                        ec.EllipticCurvePublicKey,
                    ):
                        needs_regen = False
                    else:
                        log.info("old RSA cert found; regenerating as ECDSA P-256")
            except Exception:
                log.warning("existing cert unreadable; regenerating")

    if needs_regen:
        log.info("generating ECDSA P-256 self-signed cert at %s", cert_path)
        key = ec.generate_private_key(ec.SECP256R1())
        now = dt.datetime.now(dt.timezone.utc)
        subject = issuer = x509.Name([
            x509.NameAttribute(NameOID.COMMON_NAME, "ironclad-parakeet-wt"),
        ])
        cert = (
            x509.CertificateBuilder()
            .subject_name(subject)
            .issuer_name(issuer)
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - dt.timedelta(minutes=5))
            .not_valid_after(now + dt.timedelta(days=CERT_VALID_DAYS))
            .add_extension(
                x509.SubjectAlternativeName([
                    x509.DNSName("localhost"),
                    x509.IPAddress(__import__("ipaddress").ip_address("127.0.0.1")),
                ]),
                critical=False,
            )
            .sign(key, hashes.SHA256())
        )
        cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
        key_path.write_bytes(
            key.private_bytes(
                serialization.Encoding.PEM,
                serialization.PrivateFormat.TraditionalOpenSSL,
                serialization.NoEncryption(),
            )
        )

    with open(cert_path, "rb") as f:
        cert = x509.load_pem_x509_certificate(f.read())
    der = cert.public_bytes(serialization.Encoding.DER)
    sha = hashlib.sha256(der).digest()
    hash_path.write_text(sha.hex())
    log.info("cert SHA-256: %s", sha.hex())
    return sha


# ----------------------------------------------------------------------------
# Streaming-ASR state per WebTransport session.
# ----------------------------------------------------------------------------

CHUNK_INTERVAL_S = 0.3            # how often to fire an interim transcription
SAMPLE_RATE = 16_000
INTERIM_WINDOW_S = 4.0            # max audio to feed Parakeet per interim
COMMIT_AFTER_S = 2.5              # tail audio beyond this is "committed" —
                                  # transcribed, frozen, sliced off the window


class StreamingSession:
    """Per-session audio buffer with sliding-window + committed-prefix
    interim transcription.

    Naive approach (what we used to do): transcribe the full accumulator on
    every tick. Per-interim cost grows linearly with utterance length, and
    on a long sentence each interim re-decodes 5+ seconds of audio.

    This version maintains two parts:
      - `committed_text`: text we're confident in, frozen from earlier chunks
      - `window_audio`: the most recent ~4s of audio, re-transcribed each tick

    Every time the window grows past `INTERIM_WINDOW_S`, we transcribe the
    oldest `COMMIT_AFTER_S` of it once, append the result to committed_text,
    and drop that audio from the window. The interim broadcast is then
    `committed_text + transcribe(window_audio)`.

    Per-interim cost is now bounded by INTERIM_WINDOW_S regardless of how
    long the user talks. Final transcript (on "end") still uses the full
    audio for highest accuracy.
    """

    def __init__(self, model, send_event):
        self.model = model
        self.send_event = send_event
        # Full audio buffer (used for final transcription).
        self.full_frames: list[np.ndarray] = []
        # Sliding window for interim transcription.
        self.window: np.ndarray = np.zeros(0, dtype=np.float32)
        # Text we've already committed (oldest audio that's been transcribed
        # and frozen). New interims append fresh-window text to this.
        self.committed_text: str = ""
        self.last_emitted: str = ""
        self.last_run: float = 0.0
        self.lock = asyncio.Lock()
        self._task: Optional[asyncio.Task] = None
        self._stop = asyncio.Event()

    def start(self):
        self._task = asyncio.create_task(self._scheduler())

    def stop(self):
        self._stop.set()
        if self._task:
            self._task.cancel()

    def push_audio(self, pcm: np.ndarray):
        # Caller side: producer thread. We append to both the full buffer
        # (for final) and the sliding window (for interims) under no lock —
        # numpy concat happens later under the asyncio lock.
        self.full_frames.append(pcm)
        # Lock-free append; the scheduler copies under lock before reading.
        self.window = np.concatenate([self.window, pcm.astype(np.float32)])

    async def _scheduler(self):
        try:
            while not self._stop.is_set():
                await asyncio.sleep(CHUNK_INTERVAL_S / 2)
                if self.window.size == 0:
                    continue
                now = time.perf_counter()
                if now - self.last_run < CHUNK_INTERVAL_S:
                    continue
                self.last_run = now
                text = await self._interim_step()
                if text != self.last_emitted:
                    self.last_emitted = text
                    await self.send_event({"type": "interim", "text": text})
        except asyncio.CancelledError:
            pass

    async def _interim_step(self) -> str:
        """One interim transcription pass. Commits old window audio if the
        window is full; transcribes the remaining window for the live tail."""
        async with self.lock:
            window_size = self.window.size
            # If the window grew past the threshold, commit the front portion
            # (transcribe it once, append to committed_text, slice it off).
            commit_samples = int(COMMIT_AFTER_S * SAMPLE_RATE)
            window_samples = int(INTERIM_WINDOW_S * SAMPLE_RATE)
            if window_size > window_samples:
                front = self.window[:commit_samples]
                tail = self.window[commit_samples:]
                self.window = tail
                # Transcribe the committed front in the background; merge into
                # committed_text. Done synchronously so the next interim
                # already includes it.
                loop = asyncio.get_running_loop()
                committed_chunk_text = await loop.run_in_executor(
                    None, self._transcribe_blocking, front
                )
                if committed_chunk_text:
                    self.committed_text = (
                        self.committed_text + " " + committed_chunk_text
                    ).strip()
            window_copy = self.window.copy()

        if window_copy.size < 1600:  # < 0.1s
            return self.committed_text

        loop = asyncio.get_running_loop()
        try:
            tail_text = await loop.run_in_executor(
                None, self._transcribe_blocking, window_copy
            )
        except Exception:
            log.exception("interim transcribe failed")
            return self.last_emitted

        if self.committed_text and tail_text:
            return f"{self.committed_text} {tail_text}"
        return self.committed_text or tail_text

    def _transcribe_blocking(self, audio: np.ndarray) -> str:
        # Serialize across ALL executor threads so NeMo's encoder freeze
        # state can't be corrupted by overlapping transcribes. This is the
        # fix for the "transcript deletes itself" bug — a failed transcribe
        # used to drop the front of the window without adding anything to
        # committed_text.
        with _INFERENCE_LOCK:
            with torch.inference_mode():
                results = self.model.transcribe([audio], batch_size=1, verbose=False)
        return _extract_text(results)

    async def finalize(self):
        # Final pass uses the FULL audio buffer (committed + un-committed)
        # for the most accurate transcript. This is the canonical user turn
        # we send to Claude.
        async with self.lock:
            if not self.full_frames:
                audio = np.zeros(0, dtype=np.float32)
            else:
                audio = np.concatenate(self.full_frames).astype(np.float32)
        loop = asyncio.get_running_loop()
        text = ""
        if audio.size >= 1600:
            try:
                text = await loop.run_in_executor(
                    None, self._transcribe_blocking, audio
                )
            except Exception:
                log.exception("final transcribe failed")
                text = self.last_emitted
        await self.send_event({"type": "final", "text": text})
        self._reset_state()

    def reset(self):
        self._reset_state()

    def _reset_state(self):
        self.full_frames.clear()
        self.window = np.zeros(0, dtype=np.float32)
        self.committed_text = ""
        self.last_emitted = ""
        self.last_run = 0.0


def _extract_text(results) -> str:
    if not results:
        return ""
    first = results[0]
    if isinstance(first, str):
        return first.strip()
    if hasattr(first, "text"):
        return str(first.text).strip()
    if isinstance(first, list) and first:
        return _extract_text(first)
    return str(first).strip()


# ----------------------------------------------------------------------------
# WebTransport HTTP/3 protocol handler.
# ----------------------------------------------------------------------------


class WebTransportProtocol(QuicConnectionProtocol):
    def __init__(self, *args, model=None, **kwargs):
        super().__init__(*args, **kwargs)
        self._http: Optional[H3Connection] = None
        self._model = model
        # session_id -> StreamingSession
        self._sessions: Dict[int, StreamingSession] = {}
        # session_id -> outbound unidirectional stream_id (for sending events)
        self._event_streams: Dict[int, int] = {}
        # buffered control-stream bytes per stream
        self._stream_buffers: Dict[int, bytearray] = {}

    def quic_event_received(self, event: QuicEvent):
        try:
            if isinstance(event, ProtocolNegotiated):
                self._http = H3Connection(self._quic, enable_webtransport=True)
            if isinstance(event, ConnectionTerminated):
                # Browser reloaded / closed the tab. Free every per-session
                # background task so we don't leak schedulers each refresh.
                # The leak symptom: aioquic's QuicServer stops dispatching
                # new connections after a few zombie sessions accumulate
                # (asyncio task queue saturates with no-op transcribe loops).
                log.info(
                    "connection terminated (reason=%r); tearing down %d sessions",
                    getattr(event, "reason_phrase", "?"),
                    len(self._sessions),
                )
                for s in self._sessions.values():
                    s.stop()
                self._sessions.clear()
                self._event_streams.clear()
                self._stream_buffers.clear()
                return
            if self._http is not None:
                for h3_event in self._http.handle_event(event):
                    self._h3_event_received(h3_event)
        except Exception:
            log.exception("quic_event_received raised; suppressing to keep QUIC server alive")

    def _h3_event_received(self, event: H3Event):
        if isinstance(event, HeadersReceived):
            self._on_headers(event)
        elif isinstance(event, DatagramReceived):
            self._on_datagram(event)
        elif isinstance(event, WebTransportStreamDataReceived):
            self._on_wt_stream(event)
        elif isinstance(event, DataReceived):
            # Plain HTTP/3 stream — used for the upgrade request body, ignore.
            pass

    def _on_headers(self, event: HeadersReceived):
        headers = {k: v for k, v in event.headers}
        method = headers.get(b":method", b"")
        protocol = headers.get(b":protocol", b"")
        path = headers.get(b":path", b"").decode("ascii", errors="ignore")

        if method == b"CONNECT" and protocol == b"webtransport":
            log.info("accepting WT session %d at %s", event.stream_id, path)
            # Accept the session by sending back a 200.
            self._http.send_headers(
                stream_id=event.stream_id,
                headers=[
                    (b":status", b"200"),
                    (b"sec-webtransport-http3-draft", b"draft02"),
                ],
            )
            session = StreamingSession(self._model, self._make_sender(event.stream_id))
            session.start()
            self._sessions[event.stream_id] = session
        else:
            log.info("rejecting non-WT request: %s %s", method, path)
            self._http.send_headers(
                stream_id=event.stream_id,
                headers=[(b":status", b"404")],
                end_stream=True,
            )

    def _on_datagram(self, event: DatagramReceived):
        # WT datagrams arrive here. aioquic's H3 layer routes them by the
        # CONNECT stream_id; if there's no matching session we have nothing
        # to do.
        sid = getattr(event, "stream_id", -1)
        session = self._sessions.get(sid)
        if session is None:
            # Track misses so we notice when datagrams arrive for sessions
            # we never registered (most often a stream_id vs flow_id mix-up).
            self._datagram_orphans = getattr(self, "_datagram_orphans", 0) + 1
            if self._datagram_orphans % 50 == 1:
                log.warning(
                    "datagram arrived for unknown session sid=%s (orphans=%d, known=%s, len=%d)",
                    sid, self._datagram_orphans, list(self._sessions.keys()), len(event.data),
                )
            return
        try:
            pcm = np.frombuffer(event.data, dtype="<f4")
        except Exception:
            log.exception("bad datagram payload")
            return
        if pcm.size:
            session.push_audio(pcm)
            # Log every ~1s of audio received so we can confirm flow without
            # spamming. 16k samples/s ÷ 256 samples/datagram = 62.5 dg/s.
            session._dgrams = getattr(session, "_dgrams", 0) + 1
            if session._dgrams % 60 == 0:
                log.info(
                    "session %s received %d datagrams (~%ds audio buffered)",
                    sid, session._dgrams, session._dgrams * 256 // 16000,
                )

    def _on_wt_stream(self, event: WebTransportStreamDataReceived):
        sid = getattr(event, "session_id", None)
        if sid is None:
            return
        session = self._sessions.get(sid)
        if session is None:
            return
        buf = self._stream_buffers.setdefault(event.stream_id, bytearray())
        buf.extend(event.data)
        # Process complete lines (control protocol is line-delimited JSON).
        while b"\n" in buf:
            line, _, rest = bytes(buf).partition(b"\n")
            buf[:] = rest
            text = line.decode("utf-8", errors="ignore").strip()
            if not text:
                continue
            try:
                msg = json.loads(text)
            except json.JSONDecodeError:
                log.warning("bad control JSON: %r", text)
                continue
            asyncio.create_task(self._handle_control(session, msg))
        if event.stream_ended:
            self._stream_buffers.pop(event.stream_id, None)

    async def _handle_control(self, session: StreamingSession, msg: dict):
        kind = msg.get("type")
        if kind == "end":
            await session.finalize()
        elif kind == "reset":
            session.reset()
        elif kind == "start":
            session.reset()

    def _make_sender(self, session_id: int):
        """Returns an async function that sends a JSON event on the events
        stream for `session_id`, opening it lazily on first use.

        WebTransport streams carry raw bytes — they don't go through the
        HTTP/3 frame state machine. `H3Connection.send_data` would try to
        wrap our bytes in a DATA frame and get rejected ("DATA frame is not
        allowed in this state") because the WT stream is in the WT-prefix
        state, not the HTTP-response state. We push bytes directly via the
        QUIC layer; aioquic already wrote the WT stream-type + session-id
        prefix when `create_webtransport_stream` ran."""

        async def send(msg: dict):
            stream_id = self._event_streams.get(session_id)
            if stream_id is None:
                stream_id = self._http.create_webtransport_stream(
                    session_id=session_id, is_unidirectional=True
                )
                self._event_streams[session_id] = stream_id
            payload = (json.dumps(msg) + "\n").encode("utf-8")
            # Bypass H3 frame layer — raw QUIC stream write.
            self._quic.send_stream_data(stream_id, payload, end_stream=False)
            self.transmit()

        return send


# ----------------------------------------------------------------------------
# Model bootstrap (mirrors parakeet_server.py)
# ----------------------------------------------------------------------------

_model_name = os.environ.get("PARAKEET_MODEL", "nvidia/parakeet-tdt-0.6b-v2")


def load_model(device: torch.device):
    log.info("loading %s on %s ...", _model_name, device)
    t0 = time.perf_counter()
    from nemo.collections.asr.models import ASRModel  # noqa: WPS433  (heavy)

    model = ASRModel.from_pretrained(model_name=_model_name).to(device)
    model.eval()
    log.info("model ready in %.1fs", time.perf_counter() - t0)
    # Warm JIT so the first user-audible inference isn't slow.
    log.info("JIT warm-up ...")
    t0 = time.perf_counter()
    try:
        warm = np.zeros(16_000, dtype=np.float32)
        with torch.inference_mode():
            model.transcribe([warm], batch_size=1, verbose=False)
        log.info("warm-up done in %.1fs", time.perf_counter() - t0)
    except Exception:
        log.exception("warm-up failed (non-fatal)")
    return model


# ----------------------------------------------------------------------------
# Entrypoint
# ----------------------------------------------------------------------------


async def main_async(args):
    # Bump aioquic to INFO so we see per-connection events (negotiated
    # version, ALPN, errors). Helps diagnose handshake failures that the
    # browser side reports only as "idle timeout".
    logging.getLogger("aioquic").setLevel(logging.INFO)
    logging.getLogger("asyncio").setLevel(logging.WARNING)

    # Surface ANY unhandled exception in asyncio tasks. Without this, an
    # exception in our handler can silently kill the task that processes
    # incoming QUIC packets, leaving the socket bound but unresponsive
    # (which the browser reports as "idle timeout"). Logging makes it
    # impossible to miss.
    loop = asyncio.get_running_loop()

    def _global_exc_handler(loop, context):
        msg = context.get("message", "unhandled async exception")
        exc = context.get("exception")
        log.error("[asyncio] %s: %r", msg, exc)
        if exc is not None:
            log.exception("traceback:", exc_info=exc)

    loop.set_exception_handler(_global_exc_handler)

    if args.cpu or not torch.cuda.is_available():
        device = torch.device("cpu")
        log.info("device: CPU")
    else:
        device = torch.device("cuda")
        log.info("device: %s", torch.cuda.get_device_name(0))

    cert_path = Path(args.cert)
    key_path = Path(args.key)
    hash_path = Path(args.hash_file)
    ensure_cert(cert_path, key_path, hash_path)

    model = load_model(device)

    config = QuicConfiguration(
        alpn_protocols=H3_ALPN,
        is_client=False,
        max_datagram_frame_size=65536,
        # Aggressive idle timeout: kill QUIC connections that haven't sent
        # or received anything in 8 s. Browser-side reloads that don't send
        # a clean CLOSE leave zombie connections; without short idle the
        # server accumulates state until new handshakes fail. 8s is short
        # enough to recover from a reload in seconds, long enough that a
        # talking user's gap-between-words doesn't kill the session.
        idle_timeout=8.0,
    )
    config.load_cert_chain(str(cert_path), str(key_path))

    # Heartbeat task: prove the asyncio loop is still alive. If WT wedges,
    # we'll see this stop logging while the process still exists — that
    # tells us the event loop is blocked vs the process crashing.
    async def heartbeat():
        n = 0
        while True:
            await asyncio.sleep(5)
            n += 1
            log.info("[heartbeat] alive tick %d", n)

    asyncio.create_task(heartbeat())

    def protocol_factory(*pargs, **pkw):
        return WebTransportProtocol(*pargs, model=model, **pkw)

    log.info("listening on https://%s:%d  (HTTP/3 + WebTransport)", args.host, args.port)
    log.info("cert hash file: %s", hash_path)

    await serve(
        host=args.host,
        port=args.port,
        configuration=config,
        create_protocol=protocol_factory,
    )
    # serve() returns the server; keep the loop alive forever.
    await asyncio.Event().wait()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=4443)
    parser.add_argument("--cert", default="cert.pem")
    parser.add_argument("--key", default="key.pem")
    parser.add_argument("--hash-file", default="cert.sha256")
    parser.add_argument("--cpu", action="store_true")
    args = parser.parse_args()

    try:
        asyncio.run(main_async(args))
    except KeyboardInterrupt:
        sys.exit(0)


if __name__ == "__main__":
    main()
