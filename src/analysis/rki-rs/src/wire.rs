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
    TurnBegin {
        user_input: UserInput,
    },
    TurnEnd,
    /// Process or UI session is shutting down (§1.2 L35); typically followed by [`WireEvent::TurnEnd`].
    SessionShutdown {
        reason: String,
    },
    StepBegin {
        n: usize,
    },
    StepInterrupted {
        reason: String,
    },
    /// Python `SteerInput` uses `user_input`; alias `content` for older Rust JSON.
    SteerInput {
        #[serde(rename = "user_input", alias = "content")]
        content: String,
    },
    CompactionBegin,
    CompactionEnd,
    StatusUpdate {
        token_count: usize,
        context_size: usize,
        plan_mode: bool,
        mcp_status: String,
    },
    /// JSON `type`: `mcp_loading_begin` (stable). Alias `m_c_p_loading_begin` for legacy serde snake_case.
    #[serde(rename = "mcp_loading_begin", alias = "m_c_p_loading_begin")]
    MCPLoadingBegin,
    #[serde(rename = "mcp_loading_end", alias = "m_c_p_loading_end")]
    MCPLoadingEnd,
    #[serde(rename = "mcp_status_snapshot", alias = "m_c_p_status_snapshot")]
    MCPStatusSnapshot {
        servers: Vec<String>,
    },
    TextPart {
        text: String,
    },
    ThinkPart {
        text: String,
    },
    ImageUrlPart {
        url: String,
    },
    AudioUrlPart {
        url: String,
    },
    VideoUrlPart {
        url: String,
    },
    ToolCall {
        id: String,
        function: FunctionCall,
    },
    ToolCallPart {
        id: String,
        partial: String,
    },
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
    QuestionRequest {
        id: String,
        questions: Vec<Question>,
    },
    QuestionResponse {
        id: String,
        answers: Vec<String>,
    },
    /// Python `Notification` (flat `WireEvent` uses `kind` for the notification `type` string —
    /// the JSON key remains `kind` because `type` is reserved for the serde enum tag).
    Notification {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        id: String,
        category: String,
        kind: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        source_kind: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        source_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        body: String,
        severity: String,
        #[serde(default, skip_serializing_if = "is_zero_f64")]
        created_at: f64,
        #[serde(default = "default_json_object")]
        payload: serde_json::Value,
    },
    /// Inline plan markdown (Python `PlanDisplay`: `content` + `file_path`).
    PlanDisplay {
        content: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        file_path: String,
    },
    /// Python `BtwBegin` (/btw side question started).
    BtwBegin {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        question: String,
    },
    /// Python `BtwEnd`.
    BtwEnd {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Python `HookTriggered` (batch hooks).
    HookTriggered {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        event: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        target: String,
        #[serde(
            default = "default_hook_count",
            skip_serializing_if = "is_default_hook_count"
        )]
        hook_count: u32,
    },
    /// Python `HookResolved`.
    HookResolved {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        event: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        target: String,
        #[serde(
            default = "default_hook_action_allow",
            skip_serializing_if = "is_default_hook_action"
        )]
        action: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
        #[serde(default, skip_serializing_if = "is_zero_u64")]
        duration_ms: u64,
    },
}

fn default_hook_count() -> u32 {
    1
}

fn is_default_hook_count(n: &u32) -> bool {
    *n == 1
}

fn default_hook_action_allow() -> String {
    "allow".to_string()
}

fn is_default_hook_action(s: &String) -> bool {
    s == "allow"
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

fn is_zero_f64(n: &f64) -> bool {
    *n == 0.0
}

fn default_json_object() -> serde_json::Value {
    serde_json::json!({})
}

impl WireEvent {
    /// `type` tag string for JSON and `wire_events.event_type` (matches `serde` for this enum).
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
        Self {
            sender,
            merged_sender,
        }
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
        matches!(
            event,
            WireEvent::TextPart { .. } | WireEvent::ThinkPart { .. }
        )
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

        wire.send(WireEvent::TextPart {
            text: "Hello ".to_string(),
        });
        wire.send(WireEvent::TextPart {
            text: "world".to_string(),
        });
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

        wire.send(WireEvent::ThinkPart {
            text: "A".to_string(),
        });
        wire.send(WireEvent::ThinkPart {
            text: "B".to_string(),
        });
        wire.send(WireEvent::ThinkPart {
            text: "C".to_string(),
        });
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

        wire.send(WireEvent::TextPart {
            text: "leftover".to_string(),
        });
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
        let env = WireEnvelope::new(WireEvent::TurnEnd).with_source(EventSource::subagent(
            "sa1".to_string(),
            Some("tc-1".to_string()),
        ));
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

    #[test]
    fn test_mcp_wire_events_serialize_stable_type_names() {
        let begin = WireEvent::MCPLoadingBegin;
        assert_eq!(begin.serde_type_key(), "mcp_loading_begin");
        let json = serde_json::to_string(&begin).unwrap();
        assert!(json.contains("\"type\":\"mcp_loading_begin\""), "{json}");

        let snap = WireEvent::MCPStatusSnapshot {
            servers: vec!["a".into()],
        };
        assert_eq!(snap.serde_type_key(), "mcp_status_snapshot");
    }

    #[test]
    fn test_mcp_wire_events_deserialize_legacy_m_c_p_type_aliases() {
        let old: WireEvent = serde_json::from_str(r#"{"type":"m_c_p_loading_begin"}"#).unwrap();
        assert!(matches!(old, WireEvent::MCPLoadingBegin));

        let snap: WireEvent =
            serde_json::from_str(r#"{"type":"m_c_p_status_snapshot","servers":["x"]}"#).unwrap();
        assert!(
            matches!(snap, WireEvent::MCPStatusSnapshot { servers } if servers == vec!["x".to_string()])
        );
    }

    #[test]
    fn test_steer_input_serializes_user_input_key() {
        let ev = WireEvent::SteerInput {
            content: "hello".into(),
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("\"user_input\":\"hello\""), "{j}");
        let back: WireEvent = serde_json::from_str(&j).unwrap();
        assert!(
            matches!(&back, WireEvent::SteerInput { content } if content == "hello"),
            "{back:?}"
        );
    }

    #[test]
    fn test_steer_input_deserializes_legacy_content_key() {
        let ev: WireEvent =
            serde_json::from_str(r#"{"type":"steer_input","content":"legacy"}"#).unwrap();
        assert!(
            matches!(&ev, WireEvent::SteerInput { content } if content == "legacy"),
            "{ev:?}"
        );
    }

    #[test]
    fn test_plan_display_file_path_optional() {
        let ev: WireEvent =
            serde_json::from_str(r#"{"type":"plan_display","content":"body only"}"#).unwrap();
        assert!(
            matches!(&ev, WireEvent::PlanDisplay { content, file_path }
                if content == "body only" && file_path.is_empty()),
            "{ev:?}"
        );
        let with_path: WireEvent = serde_json::from_str(
            r#"{"type":"plan_display","content":"c","file_path":"/tmp/p.md"}"#,
        )
        .unwrap();
        assert!(
            matches!(&with_path, WireEvent::PlanDisplay { file_path, .. } if file_path == "/tmp/p.md"),
            "{with_path:?}"
        );
    }

    #[test]
    fn test_notification_deserialize_legacy_shape() {
        let ev: WireEvent = serde_json::from_str(
            r#"{"type":"notification","category":"c","kind":"k","severity":"info","payload":{}}"#,
        )
        .unwrap();
        assert!(
            matches!(
                &ev,
                WireEvent::Notification {
                    id,
                    category,
                    kind,
                    severity,
                    created_at,
                    ..
                } if id.is_empty() && category == "c" && kind == "k" && severity == "info" && *created_at == 0.0
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn test_btw_begin_end_minimal_and_full() {
        let min_b: WireEvent = serde_json::from_str(r#"{"type":"btw_begin"}"#).unwrap();
        assert!(
            matches!(&min_b, WireEvent::BtwBegin { id, question } if id.is_empty() && question.is_empty()),
            "{min_b:?}"
        );
        let j = serde_json::to_string(&min_b).unwrap();
        assert_eq!(j, r#"{"type":"btw_begin"}"#);

        let end: WireEvent =
            serde_json::from_str(r#"{"type":"btw_end","id":"x","response":"done","error":null}"#)
                .unwrap();
        assert!(
            matches!(&end, WireEvent::BtwEnd { id, response, error } if id == "x" && response.as_deref() == Some("done") && error.is_none()),
            "{end:?}"
        );
    }

    #[test]
    fn test_hook_triggered_resolved_serde() {
        let minimal: WireEvent = serde_json::from_str(r#"{"type":"hook_triggered"}"#).unwrap();
        assert!(
            matches!(
                &minimal,
                WireEvent::HookTriggered { event, target, hook_count }
                    if event.is_empty() && target.is_empty() && *hook_count == 1
            ),
            "{minimal:?}"
        );
        let j = serde_json::to_string(&minimal).unwrap();
        assert_eq!(j, r#"{"type":"hook_triggered"}"#);

        let full = WireEvent::HookResolved {
            event: "Stop".into(),
            target: "agent".into(),
            action: "block".into(),
            reason: "nope".into(),
            duration_ms: 9,
        };
        let jf = serde_json::to_string(&full).unwrap();
        let back: WireEvent = serde_json::from_str(&jf).unwrap();
        assert!(
            matches!(
                &back,
                WireEvent::HookResolved {
                    action,
                    duration_ms,
                    ..
                } if action == "block" && *duration_ms == 9
            ),
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
