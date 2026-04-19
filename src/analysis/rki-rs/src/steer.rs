//! User steer queue for mid-turn message injection.
//!
//! Steers are consumed by the orchestrator at the end of each step.

use std::collections::VecDeque;
use tokio::sync::Mutex;

/// Queue for user messages sent during an active turn ("steers").
/// When the user sends input while a turn is in progress, it is queued
/// here and injected into the next step.
pub struct SteerQueue {
    queue: Mutex<VecDeque<String>>,
}

impl SteerQueue {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
        }
    }

    pub async fn push(&self, text: String) {
        self.queue.lock().await.push_back(text);
    }

    pub async fn drain(&self) -> Vec<String> {
        self.queue.lock().await.drain(..).collect()
    }

    pub async fn is_empty(&self) -> bool {
        self.queue.lock().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_steer_queue_push_and_drain() {
        let q = SteerQueue::new();
        q.push("steer 1".to_string()).await;
        q.push("steer 2".to_string()).await;

        let items = q.drain().await;
        assert_eq!(items, vec!["steer 1", "steer 2"]);
        assert!(q.is_empty().await);
    }

    #[tokio::test]
    async fn test_steer_queue_empty() {
        let q = SteerQueue::new();
        assert!(q.is_empty().await);
        q.push("x".to_string()).await;
        assert!(!q.is_empty().await);
    }

    #[tokio::test]
    async fn test_steer_queue_drain_clears_all() {
        let q = SteerQueue::new();
        q.push("a".to_string()).await;
        let _ = q.drain().await;
        assert!(q.is_empty().await);
        let second = q.drain().await;
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn test_steer_queue_multiple_drains() {
        let q = SteerQueue::new();
        q.push("first".to_string()).await;
        assert_eq!(q.drain().await.len(), 1);
        q.push("second".to_string()).await;
        assert_eq!(q.drain().await, vec!["second"]);
    }
}
