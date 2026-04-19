//! Context compaction strategies.
//!
//! `CompactionPolicy` defines how over-long context is summarised and trimmed.

use crate::message::Message;

/// Strategy for compacting context when it grows too large.
pub trait CompactionPolicy: Send + Sync {
    /// Given the current history, return a compacted version.
    /// The default implementation preserves the last `preserve_count` messages
    /// and summarizes the rest.
    fn compact(&self, history: &[Message]) -> Vec<Message>;
}

/// Simple compaction: preserve last N messages, summarize the rest.
pub struct SimpleCompaction {
    pub preserve_count: usize,
}

impl SimpleCompaction {
    pub fn new(preserve_count: usize) -> Self {
        Self { preserve_count }
    }
}

impl CompactionPolicy for SimpleCompaction {
    fn compact(&self, history: &[Message]) -> Vec<Message> {
        if history.len() <= self.preserve_count {
            return history.to_vec();
        }
        let split = history.len() - self.preserve_count;
        let to_summarize = &history[..split];
        let preserved = &history[split..];

        // Build a summary message from the older context
        let summary_text = format!(
            "[Previous conversation summarized: {} messages compacted]",
            to_summarize.len()
        );

        let mut result = vec![Message::System {
            content: summary_text,
        }];
        result.extend_from_slice(preserved);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::UserMessage;

    #[test]
    fn test_simple_compaction_preserves_last_n() {
        let policy = SimpleCompaction::new(2);
        let history = vec![
            Message::User(UserMessage::text("a")),
            Message::Assistant { content: Some("b".to_string()), tool_calls: None },
            Message::User(UserMessage::text("c")),
            Message::Assistant { content: Some("d".to_string()), tool_calls: None },
            Message::User(UserMessage::text("e")),
        ];
        let compacted = policy.compact(&history);
        assert_eq!(compacted.len(), 3); // summary + 2 preserved
        assert!(matches!(&compacted[0], Message::System { .. }));
        assert!(matches!(&compacted[1], Message::Assistant { .. }));
        assert!(matches!(&compacted[2], Message::User(u) if u.flatten_for_recall() == "e"));
    }

    #[test]
    fn test_simple_compaction_noop_when_small() {
        let policy = SimpleCompaction::new(10);
        let history = vec![
            Message::User(UserMessage::text("a")),
            Message::Assistant { content: Some("b".to_string()), tool_calls: None },
        ];
        let compacted = policy.compact(&history);
        assert_eq!(compacted.len(), 2);
    }

    #[test]
    fn test_simple_compaction_empty_history() {
        let policy = SimpleCompaction::new(2);
        let history: Vec<Message> = vec![];
        let compacted = policy.compact(&history);
        assert_eq!(compacted.len(), 0);
    }

    #[test]
    fn test_simple_compaction_exactly_n_messages() {
        let policy = SimpleCompaction::new(2);
        let history = vec![
            Message::User(UserMessage::text("a")),
            Message::Assistant { content: Some("b".to_string()), tool_calls: None },
        ];
        let compacted = policy.compact(&history);
        assert_eq!(compacted.len(), 2);
    }
}
