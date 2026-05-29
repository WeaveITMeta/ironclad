//! SSE connection manager for broadcasting events to browser tabs.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::channels::web::types::SseEvent;

/// Manages SSE broadcast to all connected browser tabs.
pub struct SseManager {
    tx: broadcast::Sender<SseEvent>,
    connection_count: Arc<AtomicU64>,
}

impl SseManager {
    /// Create a new SSE manager.
    pub fn new() -> Self {
        // Buffer 256 events; slow clients will miss events (acceptable for SSE with reconnect)
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            connection_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Broadcast an event to all connected clients.
    pub fn broadcast(&self, event: SseEvent) {
        // Ignore send errors (no receivers is fine)
        let _ = self.tx.send(event);
    }

    /// Get current number of active connections.
    pub fn connection_count(&self) -> u64 {
        self.connection_count.load(Ordering::Relaxed)
    }

    /// Create a raw broadcast subscription for non-SSE consumers (e.g. WebSocket).
    ///
    /// Returns a stream of `SseEvent` values and increments/decrements the
    /// connection counter on creation/drop, just like `subscribe()` does for SSE.
    pub fn subscribe_raw(&self) -> impl Stream<Item = SseEvent> + Send + 'static + use<> {
        let counter = Arc::clone(&self.connection_count);
        counter.fetch_add(1, Ordering::Relaxed);
        let rx = self.tx.subscribe();

        let stream = BroadcastStream::new(rx).filter_map(|result| result.ok());

        CountedStream {
            inner: stream,
            counter,
        }
    }

    /// Create a new SSE stream for a client connection.
    ///
    /// Every emitted event carries `retry: 3000ms`. Browser EventSource
    /// honors this: after any disconnect (gateway restart, Trunk reload,
    /// transient network blip) it auto-reconnects in 3 s instead of
    /// silently going dark. Without this the user has to hard-refresh the
    /// dashboard every time the gateway bounces — was the most common
    /// "JARVIS didn't reply" symptom.
    pub fn subscribe(
        &self,
    ) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send + 'static + use<>> {
        let counter = Arc::clone(&self.connection_count);
        counter.fetch_add(1, Ordering::Relaxed);
        let rx = self.tx.subscribe();

        // Prepend a one-shot bootstrap event that pins the retry interval
        // immediately on connect. Browsers cache `retry:` for the lifetime
        // of the connection, so even if the next real event takes minutes,
        // the reconnect-on-drop behavior is already armed.
        let bootstrap = futures::stream::iter(std::iter::once(Ok(
            Event::default()
                .event("ready")
                .retry(Duration::from_millis(3000))
                .data("{}"),
        )));

        let stream = BroadcastStream::new(rx)
            .filter_map(|result| result.ok())
            .map(|event| {
                let data = serde_json::to_string(&event).unwrap_or_default();
                let event_type = match &event {
                    SseEvent::Response { .. } => "response",
                    SseEvent::Thinking { .. } => "thinking",
                    SseEvent::ToolStarted { .. } => "tool_started",
                    SseEvent::ToolCompleted { .. } => "tool_completed",
                    SseEvent::ToolResult { .. } => "tool_result",
                    SseEvent::StreamChunk { .. } => "stream_chunk",
                    SseEvent::Status { .. } => "status",
                    SseEvent::ApprovalNeeded { .. } => "approval_needed",
                    SseEvent::Error { .. } => "error",
                    SseEvent::Heartbeat => "heartbeat",
                    SseEvent::SubAgentStarted { .. } => "sub_agent_started",
                    SseEvent::SubAgentProgress { .. } => "sub_agent_progress",
                    SseEvent::SubAgentCompleted { .. } => "sub_agent_completed",
                };
                Ok(Event::default()
                    .event(event_type)
                    .data(data)
                    .retry(Duration::from_millis(3000)))
            });
        let stream = bootstrap.chain(stream);

        // Wrap in a stream that decrements on drop
        let counted_stream = CountedStream {
            inner: stream,
            counter,
        };

        Sse::new(counted_stream)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)).text(""))
    }
}

impl Default for SseManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Stream wrapper that decrements connection count on drop.
///
/// When the SSE client disconnects, this stream is dropped
/// and the counter is decremented.
struct CountedStream<S> {
    inner: S,
    counter: Arc<AtomicU64>,
}

impl<S: Stream + Unpin> Stream for CountedStream<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl<S> Drop for CountedStream<S> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_manager_creation() {
        let manager = SseManager::new();
        assert_eq!(manager.connection_count(), 0);
    }

    #[test]
    fn test_broadcast_without_receivers() {
        let manager = SseManager::new();
        // Should not panic even with no receivers
        manager.broadcast(SseEvent::Heartbeat);
    }

    #[tokio::test]
    async fn test_broadcast_to_receiver() {
        let manager = SseManager::new();
        let mut rx = BroadcastStream::new(manager.tx.subscribe());

        manager.broadcast(SseEvent::Status {
            message: "test".to_string(),
        });

        let event = rx.next().await;
        assert!(event.is_some());
        let event = event.unwrap().unwrap();
        match event {
            SseEvent::Status { message } => assert_eq!(message, "test"),
            _ => panic!("unexpected event type"),
        }
    }

    #[tokio::test]
    async fn test_subscribe_raw_receives_events() {
        let manager = SseManager::new();
        let mut stream = Box::pin(manager.subscribe_raw());

        assert_eq!(manager.connection_count(), 1);

        manager.broadcast(SseEvent::Thinking {
            message: "working".to_string(),
        });

        let event = stream.next().await.unwrap();
        match event {
            SseEvent::Thinking { message } => assert_eq!(message, "working"),
            _ => panic!("Expected Thinking event"),
        }
    }

    #[tokio::test]
    async fn test_subscribe_raw_decrements_on_drop() {
        let manager = SseManager::new();
        {
            let _stream = Box::pin(manager.subscribe_raw());
            assert_eq!(manager.connection_count(), 1);
        }
        // Stream dropped, counter should decrement
        assert_eq!(manager.connection_count(), 0);
    }

    #[tokio::test]
    async fn test_subscribe_raw_multiple_subscribers() {
        let manager = SseManager::new();
        let mut s1 = Box::pin(manager.subscribe_raw());
        let mut s2 = Box::pin(manager.subscribe_raw());
        assert_eq!(manager.connection_count(), 2);

        manager.broadcast(SseEvent::Heartbeat);

        let e1 = s1.next().await.unwrap();
        let e2 = s2.next().await.unwrap();
        assert!(matches!(e1, SseEvent::Heartbeat));
        assert!(matches!(e2, SseEvent::Heartbeat));

        drop(s1);
        assert_eq!(manager.connection_count(), 1);
        drop(s2);
        assert_eq!(manager.connection_count(), 0);
    }
}
