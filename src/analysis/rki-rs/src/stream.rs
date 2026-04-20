//! Persistent session-scoped event stream backed by SQLite.
//!
//! `SessionStream` replays historical wire events and subscribes to live ones.

use crate::store::Store;
use crate::wire::{RootWireHub, WireEnvelope, WireEvent};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Persistent session-scoped event stream (§5.2 deviation).
/// All events are persisted to SQLite and can be replayed.
pub struct SessionStream {
    session_id: String,
    store: Store,
    hub: RootWireHub,
    _persist_task: Arc<tokio::task::JoinHandle<()>>,
}

impl SessionStream {
    pub fn new(session_id: String, store: Store, hub: RootWireHub) -> Self {
        let mut rx = hub.subscribe();
        let store_clone = store.clone();
        let sid = session_id.clone();
        let persist = tokio::spawn(async move {
            while let Ok(envelope) = rx.recv().await {
                let event_type = envelope.event.serde_type_key();
                let payload = match serde_json::to_string(&envelope) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let _ = store_clone.append_wire_event(&sid, &event_type, &payload);
            }
        });
        Self {
            session_id,
            store,
            hub,
            _persist_task: Arc::new(persist),
        }
    }

    /// Publish an event to the stream. Persists and broadcasts.
    pub fn publish(&self, event: WireEvent) {
        self.hub.broadcast(event);
    }

    /// Subscribe to live events only.
    pub fn subscribe_live(&self) -> broadcast::Receiver<WireEnvelope> {
        self.hub.subscribe()
    }

    /// Replay historical events from SQLite, then return a live receiver.
    pub fn replay(&self, from_cursor: i64) -> anyhow::Result<Vec<WireEnvelope>> {
        let rows = self.store.get_wire_events(&self.session_id)?;
        let mut events = Vec::new();
        for (id, _event_type, payload) in rows {
            if id < from_cursor {
                continue;
            }
            if let Ok(envelope) = serde_json::from_str::<WireEnvelope>(&payload) {
                events.push(envelope);
            }
        }
        Ok(events)
    }

    /// Subscribe with replay: returns historical events plus a live receiver.
    pub fn subscribe_with_replay(
        &self,
        from_cursor: i64,
    ) -> anyhow::Result<(Vec<WireEnvelope>, broadcast::Receiver<WireEnvelope>)> {
        let history = self.replay(from_cursor)?;
        let live = self.subscribe_live();
        Ok((history, live))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{UserInput, WireEvent};

    #[tokio::test]
    async fn test_session_stream_persist_and_replay() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let hub = RootWireHub::new();
        let stream = SessionStream::new("test-session".to_string(), store.clone(), hub);

        // Publish some events
        stream.publish(WireEvent::TurnBegin {
            user_input: UserInput::text_only("hello"),
        });
        stream.publish(WireEvent::TextPart {
            text: "world".to_string(),
        });

        // Give persistence task a moment to write
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Replay
        let history = stream.replay(0).unwrap();
        assert_eq!(history.len(), 2, "Expected 2 persisted events");
        assert!(matches!(history[0].event, WireEvent::TurnBegin { .. }));
        assert!(matches!(history[1].event, WireEvent::TextPart { .. }));
    }

    #[tokio::test]
    async fn test_session_stream_live_subscription() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let hub = RootWireHub::new();
        let stream = SessionStream::new("test-session-2".to_string(), store, hub);

        let mut rx = stream.subscribe_live();
        stream.publish(WireEvent::TurnEnd);

        let envelope = rx.recv().await.unwrap();
        assert!(matches!(envelope.event, WireEvent::TurnEnd));
    }

    #[tokio::test]
    async fn test_session_stream_replay_empty() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let hub = RootWireHub::new();
        let stream = SessionStream::new("empty-session".to_string(), store, hub);
        let history = stream.replay(0).unwrap();
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn test_session_stream_persist_row_uses_serde_type_key() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let hub = RootWireHub::new();
        let stream = SessionStream::new("type-key-session".to_string(), store.clone(), hub);
        stream.publish(WireEvent::SessionShutdown {
            reason: "fixture".into(),
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(60)).await;
        let rows = store.get_wire_events("type-key-session").unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1, "session_shutdown");
    }

    #[tokio::test]
    async fn test_session_stream_replay_from_offset() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let hub = RootWireHub::new();
        let stream = SessionStream::new("offset-session".to_string(), store.clone(), hub);

        stream.publish(WireEvent::TurnBegin {
            user_input: UserInput::text_only("a"),
        });
        stream.publish(WireEvent::TurnBegin {
            user_input: UserInput::text_only("b"),
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let history = stream.replay(2).unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn test_session_stream_subscribe_with_replay() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let hub = RootWireHub::new();
        let stream = SessionStream::new("swr-session".to_string(), store.clone(), hub);

        stream.publish(WireEvent::TextPart {
            text: "past".to_string(),
        });
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let (history, mut live) = stream.subscribe_with_replay(0).unwrap();
        assert_eq!(history.len(), 1);

        stream.publish(WireEvent::TurnEnd);
        let envelope = live.recv().await.unwrap();
        assert!(matches!(envelope.event, WireEvent::TurnEnd));
    }
}
