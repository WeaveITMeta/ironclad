//! WebTransport client for the Parakeet streaming STT sidecar.
//!
//! Wire protocol matches `voice/parakeet_wt_server.py`:
//! - audio out: QUIC datagrams, raw float32-LE PCM @ 16 kHz, ~80 ms each
//! - control out: unidirectional stream, line-delimited JSON
//!     {"type":"start"} / {"type":"end"} / {"type":"reset"}
//! - events in: unidirectional stream from the server (one per session),
//!   line-delimited JSON
//!     {"type":"interim","text":"..."}
//!     {"type":"final",  "text":"..."}
//!
//! Why WebTransport instead of WebSocket: cleaner streaming semantics, real
//! datagrams (no TCP head-of-line blocking, no per-message framing overhead),
//! and it's the modern stack. On localhost both protocols are sub-millisecond
//! so the latency win is theoretical; the design hygiene win is real.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};

thread_local! {
    /// Sent-datagram counter, logged every 60 sends (~1s of audio) so we
    /// can confirm send_audio is firing at the expected rate.
    static COUNTER: Cell<u64> = const { Cell::new(0) };
}
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};

fn console_log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

#[wasm_bindgen]
extern "C" {
    /// `WebTransport` constructor + minimal surface we need. We hit it raw via
    /// js-sys rather than web-sys because the construction options object (with
    /// `serverCertificateHashes`) needs nested typed-arrays that web-sys's
    /// generated wrappers don't make pleasant.
    #[wasm_bindgen(js_name = WebTransport)]
    type WebTransport;

    #[wasm_bindgen(constructor, js_class = "WebTransport", catch)]
    fn new(url: &str, options: &JsValue) -> Result<WebTransport, JsValue>;

    #[wasm_bindgen(method, getter, js_class = "WebTransport")]
    fn ready(this: &WebTransport) -> Promise;

    #[wasm_bindgen(method, getter, js_class = "WebTransport")]
    fn closed(this: &WebTransport) -> Promise;

    #[wasm_bindgen(method, getter, js_class = "WebTransport")]
    fn datagrams(this: &WebTransport) -> JsValue;

    #[wasm_bindgen(method, getter, js_name = "incomingUnidirectionalStreams", js_class = "WebTransport")]
    fn incoming_unidirectional_streams(this: &WebTransport) -> JsValue;

    #[wasm_bindgen(method, js_name = "createUnidirectionalStream", js_class = "WebTransport")]
    fn create_unidirectional_stream(this: &WebTransport) -> Promise;

    #[wasm_bindgen(method, js_class = "WebTransport")]
    fn close(this: &WebTransport);
}

/// Live streaming session. Drop to tear down the WT connection (the server
/// gets a `closed` event and frees its per-session Parakeet buffer).
pub struct StreamingStt {
    transport: WebTransport,
    /// Writer for the datagram channel; binary float32 PCM goes here.
    datagram_writer: JsValue,
    /// Writer for the outbound control stream; JSON lines go here.
    control_writer: JsValue,
    /// Set to false once `transport.closed` resolves. The dashboard checks
    /// this via `is_alive()` and falls back to the legacy POST path if
    /// streaming is dead — without this, a mid-session WT close left the
    /// `StreamingStt` `Some` in the cell, on_utterance kept calling
    /// `finalize()` on a dead control stream, and the user got silence.
    alive: Rc<Cell<bool>>,
    /// Single error-catch closure shared across every datagram/control
    /// write `.catch()`. Earlier we built `Closure::once` per call and
    /// `.forget()`d it — at ~67 datagrams/sec that leaked ~20k closures
    /// per 5-minute conversation. One closure, lifetime of the session.
    /// js_sys::Promise::catch takes `&Closure<dyn FnMut(JsValue)>`.
    shared_err_handler: Closure<dyn FnMut(JsValue)>,
}

impl StreamingStt {
    pub fn is_alive(&self) -> bool {
        self.alive.get()
    }
}

impl Drop for StreamingStt {
    fn drop(&mut self) {
        self.transport.close();
    }
}

/// One event the server emitted on the inbound stream.
#[derive(Debug, Clone)]
pub enum SttEvent {
    Interim(String),
    Final(String),
}

impl StreamingStt {
    /// Open a WebTransport session at `url` (e.g. `https://127.0.0.1:4443/`),
    /// pinning the self-signed cert via its SHA-256. `on_event` fires once
    /// per inbound JSON line.
    pub async fn connect<F>(url: &str, cert_sha256_hex: &str, on_event: F) -> Result<Self, String>
    where
        F: FnMut(SttEvent) + 'static,
    {
        console_log(&format!(
            "[stream-stt] connecting WT to {} (cert hash {})",
            url,
            &cert_sha256_hex[..12.min(cert_sha256_hex.len())]
        ));

        // Build serverCertificateHashes: [{ algorithm: "sha-256", value: Uint8Array(32) }]
        let cert_bytes = hex_decode(cert_sha256_hex)
            .ok_or_else(|| format!("bad cert hash hex: {}", cert_sha256_hex))?;
        if cert_bytes.len() != 32 {
            return Err(format!(
                "cert hash must be 32 bytes (SHA-256), got {}",
                cert_bytes.len()
            ));
        }
        let value = Uint8Array::new_with_length(32);
        value.copy_from(&cert_bytes);
        let hash_obj = Object::new();
        let _ = Reflect::set(&hash_obj, &"algorithm".into(), &"sha-256".into());
        let _ = Reflect::set(&hash_obj, &"value".into(), &value);
        let hashes = Array::new();
        hashes.push(&hash_obj);
        let options = Object::new();
        let _ = Reflect::set(&options, &"serverCertificateHashes".into(), &hashes);

        let transport = WebTransport::new(url, &options)
            .map_err(|e| format!("WebTransport construct: {:?}", e))?;

        JsFuture::from(transport.ready())
            .await
            .map_err(|e| format!("WT ready: {:?}", e))?;
        console_log("[stream-stt] WT session ready");

        // Track liveness via transport.closed. When it resolves (for any
        // reason — Chrome reload, server timeout, network drop) we flip
        // alive=false so the on_utterance closure falls back to legacy.
        let alive = Rc::new(Cell::new(true));
        let alive_for_close = Rc::clone(&alive);
        let closed_promise = transport.closed();
        spawn_local(async move {
            match JsFuture::from(closed_promise).await {
                Ok(v) => console_log(&format!("[stream-stt] WT closed (clean): {:?}", v)),
                Err(e) => console_log(&format!("[stream-stt] WT closed (error): {:?}", e)),
            }
            alive_for_close.set(false);
            console_log("[stream-stt] marked dead; future utterances will use legacy fallback");
        });

        // Datagram writer
        let datagrams = transport.datagrams();
        // Log the negotiated max datagram size. Browsers cap this at
        // path-MTU minus QUIC/HTTP3 overhead — typically ~1200 bytes. Any
        // datagram bigger than this is silently dropped by the writer.
        if let Ok(size) = Reflect::get(&datagrams, &"maxDatagramSize".into()) {
            console_log(&format!(
                "[stream-stt] datagrams.maxDatagramSize = {:?}",
                size
            ));
        }
        let writable = Reflect::get(&datagrams, &"writable".into())
            .map_err(|e| format!("datagrams.writable: {:?}", e))?;
        let get_writer = Reflect::get(&writable, &"getWriter".into())
            .map_err(|e| format!("writable.getWriter: {:?}", e))?
            .dyn_into::<Function>()
            .map_err(|e| format!("getWriter not function: {:?}", e))?;
        let datagram_writer = get_writer
            .call0(&writable)
            .map_err(|e| format!("getWriter() call: {:?}", e))?;

        // Open a unidirectional stream for control messages and grab its writer.
        let control_stream =
            JsFuture::from(transport.create_unidirectional_stream())
                .await
                .map_err(|e| format!("createUnidirectionalStream: {:?}", e))?;
        let get_writer = Reflect::get(&control_stream, &"getWriter".into())
            .map_err(|e| format!("control.getWriter: {:?}", e))?
            .dyn_into::<Function>()
            .map_err(|e| format!("control getWriter not function: {:?}", e))?;
        let control_writer = get_writer
            .call0(&control_stream)
            .map_err(|e| format!("control getWriter() call: {:?}", e))?;

        // Pump inbound unidirectional streams. The server only opens one
        // (the "events" stream); we accept it and read JSON lines forever.
        let incoming = transport.incoming_unidirectional_streams();
        let on_event = Rc::new(RefCell::new(on_event));
        spawn_local(read_inbound_loop(incoming, on_event));

        let shared_err_handler = Closure::wrap(Box::new(|e: JsValue| {
            console_log(&format!("[stream-stt] WT write rejected: {:?}", e));
        }) as Box<dyn FnMut(JsValue)>);

        Ok(Self {
            transport,
            datagram_writer,
            control_writer,
            alive,
            shared_err_handler,
        })
    }

    /// Send one chunk of audio as a QUIC datagram. Caller passes raw float32
    /// PCM @ 16 kHz; we ship the LE bytes through `datagrams.writable.write`.
    /// We don't await the resulting promise — datagrams are fire-and-forget
    /// — but we do attach a catch so silent rejections at least log.
    pub fn send_audio(&self, samples: &[f32]) -> Result<(), String> {
        let byte_count = samples.len() * 4;
        let bytes = Uint8Array::new_with_length(byte_count as u32);
        let f32_view = unsafe {
            std::slice::from_raw_parts(samples.as_ptr() as *const u8, byte_count)
        };
        bytes.copy_from(f32_view);
        let write_fn = Reflect::get(&self.datagram_writer, &"write".into())
            .map_err(|e| format!("dgram writer.write: {:?}", e))?
            .dyn_into::<Function>()
            .map_err(|e| format!("write not function: {:?}", e))?;
        let promise = write_fn
            .call1(&self.datagram_writer, &bytes)
            .map_err(|e| format!("dgram write() call: {:?}", e))?;

        // Increment counter and log every 60 calls (~1s of audio at 15ms
        // cadence). This lets us see in console whether send_audio is firing
        // at all, vs being filtered out before it gets here.
        let count = COUNTER.with(|c| {
            let n = c.get() + 1;
            c.set(n);
            n
        });
        if count % 60 == 1 {
            console_log(&format!(
                "[stream-stt] sent {} datagrams ({} bytes each)",
                count, byte_count
            ));
        }

        // Attach an error catch on the promise so rejected writes
        // surface instead of vanishing into the void. We share a single
        // closure for the lifetime of the StreamingStt rather than
        // allocating a fresh `Closure::once` per send_audio — at ~67
        // calls/sec, the old per-call .forget() leaked ~20k closures
        // per 5-minute conversation.
        if let Ok(promise) = promise.dyn_into::<Promise>() {
            let _ = promise.catch(&self.shared_err_handler);
        }
        Ok(())
    }

    /// Send a control JSON message on the reliable control stream.
    pub fn send_control(&self, msg: &serde_json::Value) -> Result<(), String> {
        let line = format!("{}\n", msg);
        let bytes = Uint8Array::from(line.as_bytes());
        let write_fn = Reflect::get(&self.control_writer, &"write".into())
            .map_err(|e| format!("control writer.write: {:?}", e))?
            .dyn_into::<Function>()
            .map_err(|e| format!("write not function: {:?}", e))?;
        let promise = write_fn
            .call1(&self.control_writer, &bytes)
            .map_err(|e| format!("control write() call: {:?}", e))?;
        // Attach the shared catch so a WT-close-mid-write doesn't
        // bubble as an unhandled rejection ("WebTransportError:
        // Connection lost") in the browser console.
        if let Ok(promise) = promise.dyn_into::<Promise>() {
            let _ = promise.catch(&self.shared_err_handler);
        }
        Ok(())
    }

    pub fn finalize(&self) {
        if let Err(e) = self.send_control(&serde_json::json!({"type": "end"})) {
            console_log(&format!("[stream-stt] finalize send failed: {e}"));
        }
    }

    pub fn reset(&self) {
        if let Err(e) = self.send_control(&serde_json::json!({"type": "reset"})) {
            console_log(&format!("[stream-stt] reset send failed: {e}"));
        }
    }
}

async fn read_inbound_loop(
    incoming: JsValue,
    on_event: Rc<RefCell<dyn FnMut(SttEvent)>>,
) {
    // incoming is a ReadableStream<WebTransportReceiveStream>. Read one receive
    // stream (the server only sends one), then drain JSON lines from it.
    let outer_reader = match get_reader(&incoming) {
        Ok(r) => r,
        Err(e) => {
            console_log(&format!("[stream-stt] no inbound reader: {e}"));
            return;
        }
    };

    loop {
        let next_stream = match read_one(&outer_reader).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                console_log("[stream-stt] inbound streams closed");
                return;
            }
            Err(e) => {
                console_log(&format!("[stream-stt] inbound read err: {e}"));
                return;
            }
        };
        // next_stream is a WebTransportReceiveStream (a ReadableStream of bytes).
        let inner_reader = match get_reader(&next_stream) {
            Ok(r) => r,
            Err(e) => {
                console_log(&format!("[stream-stt] inner reader: {e}"));
                continue;
            }
        };
        drain_lines(&inner_reader, &on_event).await;
    }
}

fn get_reader(stream: &JsValue) -> Result<JsValue, String> {
    let get_reader = Reflect::get(stream, &"getReader".into())
        .map_err(|e| format!("getReader lookup: {:?}", e))?
        .dyn_into::<Function>()
        .map_err(|e| format!("getReader not function: {:?}", e))?;
    get_reader
        .call0(stream)
        .map_err(|e| format!("getReader call: {:?}", e))
}

/// Resolve one .read() into Some(value) or None on done.
async fn read_one(reader: &JsValue) -> Result<Option<JsValue>, String> {
    let read = Reflect::get(reader, &"read".into())
        .map_err(|e| format!("read lookup: {:?}", e))?
        .dyn_into::<Function>()
        .map_err(|e| format!("read not function: {:?}", e))?;
    let promise = read
        .call0(reader)
        .map_err(|e| format!("read() call: {:?}", e))?;
    let promise: Promise = promise
        .dyn_into()
        .map_err(|e| format!("read() not Promise: {:?}", e))?;
    let result = JsFuture::from(promise)
        .await
        .map_err(|e| format!("read await: {:?}", e))?;
    let done = Reflect::get(&result, &"done".into())
        .map_err(|e| format!("done lookup: {:?}", e))?
        .as_bool()
        .unwrap_or(false);
    if done {
        return Ok(None);
    }
    let value = Reflect::get(&result, &"value".into())
        .map_err(|e| format!("value lookup: {:?}", e))?;
    Ok(Some(value))
}

async fn drain_lines(reader: &JsValue, on_event: &Rc<RefCell<dyn FnMut(SttEvent)>>) {
    let mut buf = String::new();
    loop {
        match read_one(reader).await {
            Ok(Some(value)) => {
                let chunk: Uint8Array = match value.dyn_into() {
                    Ok(u) => u,
                    Err(e) => {
                        console_log(&format!("[stream-stt] chunk not Uint8Array: {:?}", e));
                        continue;
                    }
                };
                let vec = chunk.to_vec();
                if let Ok(s) = std::str::from_utf8(&vec) {
                    buf.push_str(s);
                }
                while let Some(nl) = buf.find('\n') {
                    let line = buf[..nl].trim().to_string();
                    buf.drain(..=nl);
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(&line) {
                        Ok(v) => {
                            let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                            let text = v
                                .get("text")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            let event = match kind {
                                "interim" => Some(SttEvent::Interim(text)),
                                "final" => Some(SttEvent::Final(text)),
                                _ => None,
                            };
                            if let Some(ev) = event {
                                (on_event.borrow_mut())(ev);
                            }
                        }
                        Err(e) => {
                            console_log(&format!(
                                "[stream-stt] bad JSON line {:?}: {e}",
                                line
                            ));
                        }
                    }
                }
            }
            Ok(None) => {
                console_log("[stream-stt] events stream ended");
                return;
            }
            Err(e) => {
                console_log(&format!("[stream-stt] events read err: {e}"));
                return;
            }
        }
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}
