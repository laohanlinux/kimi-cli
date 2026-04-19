//! Async question manager for user interaction mid-turn.
//!
//! `QuestionManager` emits `QuestionRequest` onto the wire and awaits
//! resolution via `QuestionResolve`.

use crate::wire::{Question, RootWireHub, WireEvent};
use std::collections::HashMap;
use tokio::sync::{oneshot, Mutex};

pub struct QuestionManager {
    waiters: Mutex<HashMap<String, oneshot::Sender<Vec<String>>>>,
    hub: RootWireHub,
}

impl QuestionManager {
    pub fn new(hub: RootWireHub) -> Self {
        Self {
            waiters: Mutex::new(HashMap::new()),
            hub,
        }
    }

    pub async fn request(&self, questions: Vec<Question>) -> anyhow::Result<Vec<String>> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(id.clone(), tx);
        self.hub.broadcast(WireEvent::QuestionRequest {
            id: id.clone(),
            questions,
        });
        let answers = tokio::time::timeout(std::time::Duration::from_secs(300), rx).await??;
        Ok(answers)
    }

    pub async fn resolve(&self, id: String, answers: Vec<String>) -> anyhow::Result<()> {
        let mut waiters = self.waiters.lock().await;
        if let Some(tx) = waiters.remove(&id) {
            let _ = tx.send(answers);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Question;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_question_request_and_resolve() {
        let hub = RootWireHub::new();
        let qm = Arc::new(QuestionManager::new(hub));

        let qm2 = qm.clone();
        let handle = tokio::spawn(async move {
            let questions = vec![Question { question: "What is your name?".to_string(), options: vec![] }];
            qm2.request(questions).await.unwrap()
        });

        // Small delay to let request register
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Find the pending request id by inspecting waiters
        let waiters = qm.waiters.lock().await;
        let id = waiters.keys().next().cloned().unwrap();
        drop(waiters);

        qm.resolve(id, vec!["Alice".to_string()]).await.unwrap();
        let answers = handle.await.unwrap();
        assert_eq!(answers, vec!["Alice"]);
    }

    #[tokio::test]
    async fn test_resolve_unknown_id_is_noop() {
        let hub = RootWireHub::new();
        let qm = QuestionManager::new(hub);
        qm.resolve("unknown".to_string(), vec!["x".to_string()]).await.unwrap();
        // Should not panic
    }

    #[tokio::test]
    async fn test_question_manager_new() {
        let hub = RootWireHub::new();
        let qm = QuestionManager::new(hub);
        // Verify empty state
        let waiters = qm.waiters.lock().await;
        assert!(waiters.is_empty());
    }

    #[tokio::test]
    async fn test_request_broadcasts_event() {
        let hub = RootWireHub::new();
        let mut rx = hub.subscribe();
        let qm = Arc::new(QuestionManager::new(hub));

        let qm2 = qm.clone();
        let handle = tokio::spawn(async move {
            let questions = vec![
                Question { question: "Q1".to_string(), options: vec!["a".to_string(), "b".to_string()] },
            ];
            qm2.request(questions).await.unwrap()
        });

        // Receive the broadcast
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(envelope.event, WireEvent::QuestionRequest { .. }));

        // Resolve so the test doesn't hang
        let waiters = qm.waiters.lock().await;
        let id = waiters.keys().next().cloned().unwrap();
        drop(waiters);
        qm.resolve(id, vec!["a".to_string()]).await.unwrap();
        let _ = handle.await;
    }
}
