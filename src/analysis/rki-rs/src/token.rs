//! Explicit context token for cross-cutting propagation.
//!
//! Replaces implicit `ContextVar` propagation with an explicit token
//! passed through the call stack.

use crate::stream::SessionStream;
use crate::wire::EventSource;
use std::sync::Arc;

/// Explicit propagation token replacing implicit ContextVars (§5.3 deviation).
/// Carried through every call boundary in the soul for source tracking
/// and cancellation scoping.
#[derive(Clone)]
pub struct ContextToken {
    pub session_id: String,
    pub turn_id: String,
    pub step_id: String,
    pub tool_call_id: Option<String>,
    /// Tracks the approval source for this turn/step (replaces ContextVar).
    pub approval_source: EventSource,
    /// Reference to the session stream for publishing events (replaces hub.broadcast).
    pub stream: Option<Arc<SessionStream>>,
}

impl ContextToken {
    pub fn new(session_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        let turn_id = turn_id.into();
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.clone(),
            step_id: "0".to_string(),
            tool_call_id: None,
            approval_source: EventSource::root(),
            stream: None,
        }
    }

    pub fn with_approval_source(mut self, source: EventSource) -> Self {
        self.approval_source = source;
        self
    }

    pub fn with_stream(mut self, stream: Arc<SessionStream>) -> Self {
        self.stream = Some(stream);
        self
    }

    pub fn child_step(&self, step_id: impl Into<String>) -> Self {
        Self {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            step_id: step_id.into(),
            tool_call_id: None,
            approval_source: self.approval_source.clone(),
            stream: self.stream.clone(),
        }
    }

    pub fn child_tool_call(&self, tool_call_id: impl Into<String>) -> Self {
        Self {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            step_id: self.step_id.clone(),
            tool_call_id: Some(tool_call_id.into()),
            approval_source: self.approval_source.clone(),
            stream: self.stream.clone(),
        }
    }

    pub fn event_source(&self) -> EventSource {
        self.approval_source.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_hierarchy() {
        let parent = ContextToken::new("session1", "turn1");
        assert_eq!(parent.session_id, "session1");
        assert_eq!(parent.turn_id, "turn1");
        assert_eq!(parent.step_id, "0");
        assert!(parent.tool_call_id.is_none());

        let step = parent.child_step("3");
        assert_eq!(step.step_id, "3");
        assert_eq!(step.session_id, "session1");

        let tc = step.child_tool_call("tc-abc");
        assert_eq!(tc.tool_call_id, Some("tc-abc".to_string()));
        assert_eq!(tc.step_id, "3");
    }

    #[test]
    fn test_token_event_source() {
        let token = ContextToken::new("s1", "t1");
        let source = token.event_source();
        assert_eq!(source.agent_id, None);
    }

    #[test]
    fn test_token_child_preserves_session() {
        let parent = ContextToken::new("s1", "t1");
        let child = parent.child_step("5");
        assert_eq!(child.session_id, "s1");
        assert_eq!(child.turn_id, "t1");
    }

    #[test]
    fn test_token_with_approval_source() {
        let token = ContextToken::new("s1", "t1")
            .with_approval_source(EventSource::background_task("task-1".to_string()));
        assert_eq!(token.approval_source.task_id, Some("task-1".to_string()));
        let child = token.child_step("2");
        assert_eq!(child.approval_source.task_id, Some("task-1".to_string()));
    }

    #[tokio::test]
    async fn test_token_child_preserves_stream() {
        let hub = crate::wire::RootWireHub::new();
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let stream = Arc::new(SessionStream::new("s1".to_string(), store, hub));
        let token = ContextToken::new("s1", "t1").with_stream(stream);
        assert!(token.stream.is_some());
        let child = token.child_step("2");
        assert!(child.stream.is_some());
    }
}
