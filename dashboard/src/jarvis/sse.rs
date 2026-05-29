//! EventSource subscriber for `/api/chat/events`. Each typed event becomes
//! a `ChatEvent` and is forwarded to a callback.

use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{EventSource, MessageEvent};

fn console_log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// Final assistant response (after tools finish).
    Response { content: String },
    /// Streaming token chunk. Appended to the in-progress assistant message.
    StreamChunk { content: String },
    /// Free-form status line (e.g., "Sending...", "Thinking...").
    Status(String),
    /// A tool started executing.
    ToolStarted { name: String },
    /// A tool finished.
    ToolCompleted { name: String, success: bool },
    /// The agent is paused waiting for the user to approve a tool call.
    /// Until the dashboard POSTs to /api/chat/approval with the request_id,
    /// the agent loop will block indefinitely.
    ApprovalNeeded {
        request_id: String,
        tool_name: String,
        description: String,
        parameters: String,
    },
}

/// RAII handle. Drop to close the underlying EventSource and free closures.
pub struct ChatStream {
    source: EventSource,
    _handlers: Vec<Closure<dyn FnMut(MessageEvent)>>,
}

impl Drop for ChatStream {
    fn drop(&mut self) {
        self.source.close();
    }
}

#[derive(Deserialize)]
struct ResponseData {
    content: String,
}

#[derive(Deserialize)]
struct StreamChunkData {
    content: String,
}

#[derive(Deserialize)]
struct StatusData {
    message: String,
}

#[derive(Deserialize)]
struct ToolStartedData {
    name: String,
}

#[derive(Deserialize)]
struct ToolCompletedData {
    name: String,
    success: bool,
}

#[derive(Deserialize)]
struct ApprovalNeededData {
    request_id: String,
    tool_name: String,
    description: String,
    parameters: String,
}

/// Subscribe to chat events; `on_event` fires once per typed event the gateway
/// emits. Returns a handle; drop it to close the stream.
pub fn subscribe<F>(url: &str, mut on_event: F) -> Result<ChatStream, String>
where
    F: FnMut(ChatEvent) + 'static,
{
    console_log(&format!("[jarvis-sse] subscribing to {url}"));
    let source = EventSource::new(url).map_err(|e| {
        let err = format!("EventSource construct failed: {:?}", e);
        console_log(&format!("[jarvis-sse] {err}"));
        err
    })?;
    let mut handlers: Vec<Closure<dyn FnMut(MessageEvent)>> = Vec::new();

    // open / error lifecycle so we can confirm the stream actually connects.
    let on_open = Closure::wrap(Box::new(move |_: JsValue| {
        console_log("[jarvis-sse] connection opened");
    }) as Box<dyn FnMut(JsValue)>);
    let _ = source.add_event_listener_with_callback("open", on_open.as_ref().unchecked_ref());
    on_open.forget();

    // EventSource has built-in reconnect for transport-level errors
    // (3-second backoff per `retry:` field the gateway sends). What it
    // CANNOT recover from is a stale URL — gateway rotates the auth
    // token on every boot, so an `error` after the gateway restarted
    // means the cached `?token=...` is dead and EventSource is now
    // looping forever against a 401. The error handler logs prominently
    // so the user can spot it in DevTools; full token-refresh-on-401
    // is out of scope for the soon-to-be-retired Leptos surface.
    let on_err = Closure::wrap(Box::new(move |ev: JsValue| {
        console_log(&format!(
            "[jarvis-sse] connection error (will auto-retry; if persistent, refresh the page to pick up a new auth token): {:?}",
            ev
        ));
    }) as Box<dyn FnMut(JsValue)>);
    let _ = source.add_event_listener_with_callback("error", on_err.as_ref().unchecked_ref());
    on_err.forget();

    let register = |event_name: &str,
                    handler: Closure<dyn FnMut(MessageEvent)>,
                    handlers: &mut Vec<Closure<dyn FnMut(MessageEvent)>>,
                    source: &EventSource| {
        source
            .add_event_listener_with_callback(event_name, handler.as_ref().unchecked_ref())
            .map(|_| handlers.push(handler))
    };

    // We need to share the user's callback across many handlers; wrap in Rc<RefCell>.
    use std::cell::RefCell;
    use std::rc::Rc;
    let cb = Rc::new(RefCell::new(move |ev: ChatEvent| on_event(ev)));

    macro_rules! parse_and_emit {
        ($event:literal, $ty:ty, $build:expr) => {{
            let cb = Rc::clone(&cb);
            let handler = Closure::wrap(Box::new(move |ev: MessageEvent| {
                let raw = ev.data().as_string().unwrap_or_default();
                console_log(&format!("[jarvis-sse] event '{}' raw={}", $event, raw));
                match serde_json::from_str::<$ty>(&raw) {
                    Ok(d) => {
                        let chat_event = $build(d);
                        (cb.borrow_mut())(chat_event);
                    }
                    Err(e) => {
                        console_log(&format!(
                            "[jarvis-sse] failed to parse '{}': {} (raw={})",
                            $event, e, raw
                        ));
                    }
                }
            }) as Box<dyn FnMut(MessageEvent)>);
            let _ = register($event, handler, &mut handlers, &source);
        }};
    }

    parse_and_emit!("response", ResponseData, |d: ResponseData| ChatEvent::Response {
        content: d.content,
    });
    parse_and_emit!("stream_chunk", StreamChunkData, |d: StreamChunkData| {
        ChatEvent::StreamChunk { content: d.content }
    });
    parse_and_emit!("status", StatusData, |d: StatusData| ChatEvent::Status(d.message));
    parse_and_emit!("thinking", StatusData, |d: StatusData| ChatEvent::Status(d.message));
    parse_and_emit!("tool_started", ToolStartedData, |d: ToolStartedData| {
        ChatEvent::ToolStarted { name: d.name }
    });
    parse_and_emit!("tool_completed", ToolCompletedData, |d: ToolCompletedData| {
        ChatEvent::ToolCompleted {
            name: d.name,
            success: d.success,
        }
    });
    parse_and_emit!("approval_needed", ApprovalNeededData, |d: ApprovalNeededData| {
        ChatEvent::ApprovalNeeded {
            request_id: d.request_id,
            tool_name: d.tool_name,
            description: d.description,
            parameters: d.parameters,
        }
    });

    Ok(ChatStream {
        source,
        _handlers: handlers,
    })
}
