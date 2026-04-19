//! Event bus (Wire) for soul-to-UI communication.
//!
//! `RootWireHub` is a session-scoped broadcast channel.
//! `Wire` is per-turn SPMC channel with merge-aware receiver.

use crate::message::{ContentPart, FunctionCall};
use serde::{Deserialize, Serialize};

/// Provenance metadata for every event in the session stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSource {
    pub source_type: SourceType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<String>,
}

impl EventSource {
    pub fn root() -> Self {
        Self {
            source_type: SourceType::Root,
            agent_id: None,
            task_id: None,
            parent_event_id: None,
        }
    }

    pub fn subagent(agent_id: String, parent_event_id: Option<String>) -> Self {
        Self {
            source_type: SourceType::Subagent,
            agent_id: Some(agent_id),
            task_id: None,
            parent_event_id,
        }
    }

    #[allow(dead_code)]
    pub fn background_task(task_id: String) -> Self {
        Self {
            source_type: SourceType::BackgroundTask,
            agent_id: None,
            task_id: Some(task_id),
            parent_event_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Root,
    Subagent,
    BackgroundTask,
    External,
}

/// Wrapper for every event on the wire, carrying provenance metadata.
/// This replaces the old per-turn Wire with an event-sourced model where
/// all events carry source metadata (§6.5 deviation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEnvelope {
    pub event_id: String,
    pub timestamp: String, // ISO 8601
    pub source: EventSource,
    pub event: WireEvent,
}

impl WireEnvelope {
    pub fn new(event: WireEvent) -> Self {
        Self {
            event_id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            source: EventSource::root(),
            event,
        }
    }

    pub fn with_source(mut self, source: EventSource) -> Self {
        self.source = source;
        self
    }

    pub fn is_subagent_event(&self) -> bool {
        matches!(self.source.source_type, SourceType::Subagent)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    TurnBegin { user_input: UserInput },
    TurnEnd,
    /// Process or UI session is shutting down (§1.2 L35); typically followed by [`WireEvent::TurnEnd`].
    SessionShutdown { reason: String },
    StepBegin { n: usize },
    StepInterrupted { reason: String },
    SteerInput { content: String },
    CompactionBegin,
    CompactionEnd,
    StatusUpdate {
        token_count: usize,
        context_size: usize,
        plan_mode: bool,
        mcp_status: String,
    },
    MCPLoadingBegin,
    MCPLoadingEnd,
    MCPStatusSnapshot { servers: Vec<String> },
    TextPart { text: String },
    ThinkPart { text: String },
    ImageUrlPart { url: String },
    AudioUrlPart { url: String },
    VideoUrlPart { url: String },
    ToolCall { id: String, function: FunctionCall },
    ToolCallPart { id: String, partial: String },
    ToolResult {
        tool_call_id: String,
        output: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
    },
    ApprovalRequest {
        id: String,
        tool_call_id: String,
        sender: String,
        action: String,
        description: String,
        display: String,
    },
    ApprovalResponse {
        id: String,
        approved: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    QuestionRequest { id: String, questions: Vec<Question> },
    QuestionResponse { id: String, answers: Vec<String> },
    Notification {
        category: String,
        kind: String,
        severity: String,
        payload: serde_json::Value,
    },
    PlanDisplay { content: String },
    BtwBegin,
    BtwEnd,
    HookTriggered,
    HookResolved,
}

impl WireEvent {
    /// Snake-case `type` tag used in JSON and in `wire_events.event_type` (matches `serde` externally-tagged shape).
    pub fn serde_type_key(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_else(|| "wire_event".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
}

impl UserInput {
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            parts: vec![],
        }
    }

    pub fn from_turn(turn: &crate::turn_input::TurnInput) -> Self {
        Self {
            text: turn.text_summary(),
            parts: turn.parts_for_wire(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub options: Vec<String>,
}

#[derive(Clone)]
pub struct Wire {
    sender: tokio::sync::broadcast::Sender<WireEnvelope>,
    merged_sender: tokio::sync::broadcast::Sender<WireEnvelope>,
}

impl Wire {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(capacity);
        let (merged_sender, _) = tokio::sync::broadcast::channel(capacity);
        Self { sender, merged_sender }
    }

    pub fn send(&self, event: WireEvent) {
        let envelope = WireEnvelope::new(event.clone());
        let _ = self.sender.send(envelope.clone());
        // Send merged version
        let merged = self.merge_event(event);
        let _ = self.merged_sender.send(WireEnvelope::new(merged));
    }

    pub fn send_envelope(&self, envelope: WireEnvelope) {
        let _ = self.sender.send(envelope);
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<WireEnvelope> {
        self.sender.subscribe()
    }

    #[allow(dead_code)]
    pub fn subscribe_merged(&self) -> tokio::sync::broadcast::Receiver<WireEnvelope> {
        self.merged_sender.subscribe()
    }

    /// Merge logic per §2.3: coalesce consecutive mergeable events.
    /// For simplicity in this broadcast model, we emit each event individually
    /// but consumers can use a merge-aware receiver. The true merge is done
    /// by the consumer; here we just provide a separate channel that could
    /// be backed by a merge buffer in a more complex impl.
    fn merge_event(&self, event: WireEvent) -> WireEvent {
        event
    }
}

/// Merge-aware wire consumer. Buffers mergeable events and flushes
/// on non-mergeable events or explicit flush.
pub struct MergedWireReceiver {
    raw: tokio::sync::broadcast::Receiver<WireEnvelope>,
    buffer: Option<WireEvent>,
}

impl MergedWireReceiver {
    pub fn new(raw: tokio::sync::broadcast::Receiver<WireEnvelope>) -> Self {
        Self { raw, buffer: None }
    }

    /// Receive the next merged event. Consecutive TextPart and ThinkPart
    /// events are coalesced into single events.
    pub async fn recv(&mut self) -> Option<WireEnvelope> {
        loop {
            // If we have a non-mergeable buffered event, return it immediately
            // without blocking on raw.recv().
            if let Some(event) = self.buffer.take() {
                if !Self::is_mergeable(&event) {
                    return Some(WireEnvelope::new(event));
                }
                self.buffer = Some(event);
            }

            match self.raw.recv().await {
                Ok(envelope) => {
                    let event = envelope.event.clone();
                    if let Some(merged) = self.try_merge(event) {
                        return Some(WireEnvelope::new(merged));
                    }
                    // Buffered, continue reading
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return self.flush();
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    }

    fn is_mergeable(event: &WireEvent) -> bool {
        matches!(event, WireEvent::TextPart { .. } | WireEvent::ThinkPart { .. })
    }

    fn try_merge(&mut self, event: WireEvent) -> Option<WireEvent> {
        match (&mut self.buffer, &event) {
            // TextPart + TextPart
            (Some(WireEvent::TextPart { text: buf }), WireEvent::TextPart { text }) => {
                buf.push_str(text);
                None
            }
            // ThinkPart + ThinkPart
            (Some(WireEvent::ThinkPart { text: buf }), WireEvent::ThinkPart { text }) => {
                buf.push_str(text);
                None
            }
            // Flush buffer, start new
            (buf @ Some(_), _) => {
                let old = buf.take().unwrap();
                *buf = Some(event);
                Some(old)
            }
            // No buffer, start new
            (None, _) => {
                self.buffer = Some(event);
                None
            }
        }
    }

    /// Flush any buffered event.
    pub fn flush(&mut self) -> Option<WireEnvelope> {
        self.buffer.take().map(WireEnvelope::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_wire_merged_receiver_coalesces_text() {
        let wire = Wire::new(1024);
        let raw_rx = wire.subscribe();
        let mut merged = MergedWireReceiver::new(raw_rx);

        wire.send(WireEvent::TextPart { text: "Hello ".to_string() });
        wire.send(WireEvent::TextPart { text: "world".to_string() });
        wire.send(WireEvent::TurnEnd);
        drop(wire); // close channel to trigger final flush

        let ev = merged.recv().await.unwrap();
        assert!(matches!(ev.event, WireEvent::TextPart { text } if text == "Hello world"));

        let ev = merged.recv().await.unwrap();
        assert!(matches!(ev.event, WireEvent::TurnEnd));
    }

    #[tokio::test]
    async fn test_wire_merged_receiver_coalesces_think() {
        let wire = Wire::new(1024);
        let raw_rx = wire.subscribe();
        let mut merged = MergedWireReceiver::new(raw_rx);

        wire.send(WireEvent::ThinkPart { text: "A".to_string() });
        wire.send(WireEvent::ThinkPart { text: "B".to_string() });
        wire.send(WireEvent::ThinkPart { text: "C".to_string() });
        wire.send(WireEvent::StepBegin { n: 1 });
        drop(wire);

        let ev = merged.recv().await.unwrap();
        assert!(matches!(ev.event, WireEvent::ThinkPart { text } if text == "ABC"));

        let ev = merged.recv().await.unwrap();
        assert!(matches!(ev.event, WireEvent::StepBegin { n: 1 }));
    }

    #[tokio::test]
    async fn test_wire_merged_receiver_flush() {
        let wire = Wire::new(1024);
        let raw_rx = wire.subscribe();
        let mut merged = MergedWireReceiver::new(raw_rx);

        wire.send(WireEvent::TextPart { text: "leftover".to_string() });
        wire.send(WireEvent::TurnEnd);
        drop(wire);

        let ev = merged.recv().await.unwrap();
        assert!(matches!(ev.event, WireEvent::TextPart { text } if text == "leftover"));

        let ev2 = merged.recv().await.unwrap();
        assert!(matches!(ev2.event, WireEvent::TurnEnd));

        // Buffer empty after all events consumed
        assert!(merged.flush().is_none());
    }

    #[tokio::test]
    async fn test_root_wire_hub_broadcast() {
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        hub.broadcast(WireEvent::TurnBegin {
            user_input: UserInput::text_only("hi"),
        });
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev.event, WireEvent::TurnBegin { .. }));
    }

    #[tokio::test]
    async fn test_wire_envelope_source() {
        let env = WireEnvelope::new(WireEvent::TurnEnd).with_source(EventSource::subagent("sa1".to_string(), Some("tc-1".to_string())));
        assert_eq!(env.source.agent_id, Some("sa1".to_string()));
        assert!(env.is_subagent_event());
    }

    #[tokio::test]
    async fn test_wire_envelope_is_not_subagent_event() {
        let env = WireEnvelope::new(WireEvent::TurnEnd);
        assert!(!env.is_subagent_event());
    }

    #[test]
    fn test_session_shutdown_serde_roundtrip() {
        let ev = WireEvent::SessionShutdown {
            reason: "interactive_exit".into(),
        };
        assert_eq!(ev.serde_type_key(), "session_shutdown");
        let s = serde_json::to_string(&ev).unwrap();
        let back: WireEvent = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(&back, WireEvent::SessionShutdown { reason } if reason == "interactive_exit"),
            "{back:?}"
        );
    }
}

#[derive(Clone)]
pub struct RootWireHub {
    sender: tokio::sync::broadcast::Sender<WireEnvelope>,
}

impl RootWireHub {
    pub fn new() -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(1024);
        Self { sender }
    }

    pub fn broadcast(&self, event: WireEvent) {
        let _ = self.sender.send(WireEnvelope::new(event));
    }

    pub fn broadcast_envelope(&self, envelope: WireEnvelope) {
        let _ = self.sender.send(envelope);
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<WireEnvelope> {
        self.sender.subscribe()
    }

    /// Subscribe to events filtered by [`SourceType`] (§6.5 — consumer-side filter on the broadcast stream).
    pub fn subscribe_filtered(&self, source_type: SourceType) -> SourceFilteredWireReceiver {
        SourceFilteredWireReceiver::new(self, source_type)
    }
}

/// Skips wire envelopes whose [`EventSource::source_type`] does not match (§6.5 consumer-side filter).
pub struct SourceFilteredWireReceiver {
    inner: tokio::sync::broadcast::Receiver<WireEnvelope>,
    filter: SourceType,
}

impl SourceFilteredWireReceiver {
    pub fn new(hub: &RootWireHub, filter: SourceType) -> Self {
        Self {
            inner: hub.subscribe(),
            filter,
        }
    }

    pub async fn recv(&mut self) -> Result<WireEnvelope, tokio::sync::broadcast::error::RecvError> {
        loop {
            let env = self.inner.recv().await?;
            if env.source.source_type == self.filter {
                return Ok(env);
            }
        }
    }

    pub fn try_recv(
        &mut self,
    ) -> Result<WireEnvelope, tokio::sync::broadcast::error::TryRecvError> {
        loop {
            match self.inner.try_recv() {
                Ok(env) if env.source.source_type == self.filter => return Ok(env),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod filtered_tests {
    use super::*;

    #[tokio::test]
    async fn test_source_filtered_receiver_yields_only_subagent() {
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe_filtered(SourceType::Subagent);
        hub.broadcast(WireEvent::TurnEnd);
        hub.broadcast_envelope(
            WireEnvelope::new(WireEvent::TextPart {
                text: "root".to_string(),
            })
            .with_source(EventSource::root()),
        );
        hub.broadcast_envelope(
            WireEnvelope::new(WireEvent::TextPart {
                text: "sub".to_string(),
            })
            .with_source(EventSource::subagent("sa".to_string(), None)),
        );
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev.event, WireEvent::TextPart { text } if text == "sub"));
    }
}
