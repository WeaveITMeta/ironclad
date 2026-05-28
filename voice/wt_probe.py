"""Minimal aioquic WebTransport client. Probes whether the server's WT
handshake works at all, independently of Chrome's caching and cert pinning
behavior. If THIS connects, the server is fine and Chrome is the problem.
If this also times out, the server is broken."""

import asyncio
import logging
import ssl
import sys
from pathlib import Path

from aioquic.asyncio.client import connect
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import HeadersReceived
from aioquic.quic.configuration import QuicConfiguration

logging.basicConfig(level=logging.INFO, format="[wt-probe] %(message)s")
log = logging.getLogger("wt-probe")


async def main():
    cert_path = Path(__file__).parent / "wt_cert.pem"
    config = QuicConfiguration(
        alpn_protocols=H3_ALPN,
        is_client=True,
        max_datagram_frame_size=65536,
    )
    # Skip cert verification (we're testing against self-signed loopback).
    config.verify_mode = ssl.CERT_NONE
    if cert_path.exists():
        config.load_verify_locations(cafile=str(cert_path))

    log.info("connecting to https://127.0.0.1:4443 ...")
    try:
        async with connect("127.0.0.1", 4443, configuration=config) as client:
            log.info("QUIC handshake OK")
            h3 = H3Connection(client._quic, enable_webtransport=True)
            # Send WT CONNECT
            stream_id = client._quic.get_next_available_stream_id()
            h3.send_headers(
                stream_id=stream_id,
                headers=[
                    (b":method", b"CONNECT"),
                    (b":protocol", b"webtransport"),
                    (b":scheme", b"https"),
                    (b":authority", b"127.0.0.1:4443"),
                    (b":path", b"/"),
                ],
            )
            client.transmit()
            log.info("sent WT CONNECT on stream %d, waiting for response...", stream_id)

            # Drain events for up to 5s
            for _ in range(50):
                await asyncio.sleep(0.1)
                events = []
                while True:
                    e = client._quic.next_event()
                    if e is None:
                        break
                    events.extend(h3.handle_event(e))
                for ev in events:
                    if isinstance(ev, HeadersReceived):
                        status = next(
                            (v for k, v in ev.headers if k == b":status"), b"?"
                        )
                        log.info("WT CONNECT response status=%s", status.decode())
                        return
            log.error("no response within 5s — server is not replying")
    except Exception as exc:
        log.exception("probe failed: %s", exc)
        sys.exit(2)


asyncio.run(main())
