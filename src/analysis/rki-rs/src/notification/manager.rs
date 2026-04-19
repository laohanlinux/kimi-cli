use crate::notification::types::NotificationEvent;
use crate::store::Store;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// In-memory deduplication filter for notification publish.
///
/// Acts as a perfect bloom filter (no false negatives, no false positives)
/// backed by a HashSet. Persistent dedupe check falls back to SQLite.
#[derive(Clone)]
pub struct NotificationManager {
    session_id: String,
    store: Store,
    seen_dedupe_keys: Arc<Mutex<HashSet<String>>>,
}

impl NotificationManager {
    pub fn new(session_id: String, store: Store) -> Self {
        Self {
            session_id,
            store,
            seen_dedupe_keys: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Publish a notification with deduplication.
    ///
    /// If `event.dedupe_key` is set and matches a previously published
    /// notification for this session, the event is silently dropped.
    pub async fn publish(&self, event: NotificationEvent) -> anyhow::Result<Option<String>> {
        let dedupe_key = event.dedupe_key.as_deref();

        // Fast-path: in-memory bloom filter
        if let Some(key) = dedupe_key
            && self.seen_dedupe_keys.lock().unwrap().contains(key)
        {
            return Ok(None);
        }

        // Slow-path: persistent dedupe check via DB
        if let Some(key) = dedupe_key
            && self.store.has_notification_dedupe(&self.session_id, key)?
        {
            self.seen_dedupe_keys.lock().unwrap().insert(key.to_string());
            return Ok(None);
        }

        let id = uuid::Uuid::new_v4().to_string();
        let payload = serde_json::to_string(&event.payload)?;
        self.store.append_notification(
            &id,
            &self.session_id,
            &event.category,
            &event.kind,
            &event.severity,
            &payload,
            dedupe_key,
        )?;

        if let Some(key) = dedupe_key {
            self.seen_dedupe_keys.lock().unwrap().insert(key.to_string());
        }

        Ok(Some(id))
    }

    /// Claim pending notifications for a sink with exactly-once semantics.
    ///
    /// Returns only notifications that are not already claimed by this
    /// consumer. Creates claim records so subsequent calls for the same
    /// consumer will not re-deliver until acked.
    pub async fn claim(&self, sink: &str) -> Vec<NotificationEvent> {
        match self.store.claim_notifications(&self.session_id, sink, 100) {
            Ok(rows) => rows
                .into_iter()
                .map(|(id, category, kind, severity, payload)| {
                    let payload =
                        serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
                    NotificationEvent {
                        category,
                        kind,
                        severity,
                        payload,
                        dedupe_key: Some(id),
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Acknowledge delivery of a specific notification for a consumer.
    ///
    /// After processing claimed notifications, callers should ack each
    /// one to prevent redelivery on stale-claim recovery.
    pub async fn ack(&self, consumer_id: &str, notification_id: &str) -> anyhow::Result<()> {
        self.store
            .ack_notification_claim(notification_id, consumer_id)?;
        Ok(())
    }

    /// Recover stale claims and return notification IDs for redelivery.
    ///
    /// Claims older than `stale_after_ms` that have not been acked are
    /// deleted, allowing the associated notifications to be re-claimed.
    pub async fn recover_stale_claims(&self, stale_after_ms: i64) -> Vec<String> {
        self.store.recover_stale_claims(stale_after_ms).unwrap_or_default()
    }

    /// Subscribe as a consumer group member, returning events after the
    /// last acknowledged offset. Uses claim-based exactly-once delivery.
    pub async fn subscribe_consumer(
        &self,
        consumer_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<NotificationEvent>> {
        let rows = self
            .store
            .claim_notifications(&self.session_id, consumer_id, limit)?;

        let mut results = Vec::new();
        for (id, category, kind, severity, payload) in rows {
            let payload = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
            results.push(NotificationEvent {
                category,
                kind,
                severity,
                payload,
                dedupe_key: Some(id),
            });
        }

        Ok(results)
    }

    /// §8.4 offset tail: notifications newer than the persisted offset for `consumer_id` (no claim rows).
    pub async fn read_since_persisted_offset(
        &self,
        consumer_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<NotificationEvent>> {
        let after = self
            .store
            .get_notification_offset(consumer_id, &self.session_id)?;
        let rows = self.store.list_notifications_after(
            &self.session_id,
            after.as_deref(),
            limit,
        )?;
        Ok(rows
            .into_iter()
            .map(|(id, category, kind, severity, payload)| {
                let payload = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
                NotificationEvent {
                    category,
                    kind,
                    severity,
                    payload,
                    dedupe_key: Some(id),
                }
            })
            .collect())
    }

    /// Persist the last delivered notification id for offset-based consumers (pairs with [`Self::read_since_persisted_offset`]).
    pub async fn advance_consumer_offset(
        &self,
        consumer_id: &str,
        last_notification_id: &str,
    ) -> anyhow::Result<()> {
        self.store
            .set_notification_offset(consumer_id, &self.session_id, last_notification_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> NotificationManager {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        NotificationManager::new("test-session".to_string(), store)
    }

    #[tokio::test]
    async fn test_publish_and_claim() {
        let mgr = test_manager();
        let event = NotificationEvent {
            category: "system".to_string(),
            kind: "test".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"msg": "hello"}),
            dedupe_key: None,
        };
        let id = mgr.publish(event.clone()).await.unwrap();
        assert!(id.is_some());

        let claimed = mgr.claim("llm").await;
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].kind, "test");
    }

    #[tokio::test]
    async fn test_dedupe_filter_blocks_duplicates() {
        let mgr = test_manager();
        let event = NotificationEvent {
            category: "task".to_string(),
            kind: "done".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"id": "t1"}),
            dedupe_key: Some("dup-key-1".to_string()),
        };

        let id1 = mgr.publish(event.clone()).await.unwrap();
        assert!(id1.is_some());

        let id2 = mgr.publish(event.clone()).await.unwrap();
        assert!(id2.is_none()); // deduped

        let claimed = mgr.claim("llm").await;
        assert_eq!(claimed.len(), 1);
    }

    #[tokio::test]
    async fn test_claim_exactly_once_per_consumer() {
        let mgr = test_manager();
        for i in 0..3 {
            let event = NotificationEvent {
                category: "task".to_string(),
                kind: format!("event-{i}"),
                severity: "info".to_string(),
                payload: serde_json::json!({"i": i}),
                dedupe_key: None,
            };
            mgr.publish(event).await.unwrap();
        }

        // First claim by consumer "A" gets all 3
        let batch1 = mgr.claim("A").await;
        assert_eq!(batch1.len(), 3);

        // Second claim gets nothing (already claimed, not acked)
        let batch2 = mgr.claim("A").await;
        assert!(batch2.is_empty());

        // Consumer "B" can still claim all 3
        let batch3 = mgr.claim("B").await;
        assert_eq!(batch3.len(), 3);
    }

    #[tokio::test]
    async fn test_ack_prevents_redelivery() {
        let mgr = test_manager();
        for i in 0..2 {
            let event = NotificationEvent {
                category: "task".to_string(),
                kind: format!("event-{i}"),
                severity: "info".to_string(),
                payload: serde_json::json!({"i": i}),
                dedupe_key: None,
            };
            mgr.publish(event).await.unwrap();
        }

        let batch = mgr.claim("C").await;
        assert_eq!(batch.len(), 2);

        // Ack both
        for ev in &batch {
            mgr.ack("C", ev.dedupe_key.as_ref().unwrap()).await.unwrap();
        }

        // Now consumer "C" sees nothing new
        let batch2 = mgr.claim("C").await;
        assert!(batch2.is_empty());
    }

    #[tokio::test]
    async fn test_stale_claim_recovery() {
        let mgr = test_manager();
        let event = NotificationEvent {
            category: "task".to_string(),
            kind: "stale-test".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"x": 1}),
            dedupe_key: None,
        };
        mgr.publish(event).await.unwrap();

        // Claim but do NOT ack
        let batch = mgr.claim("D").await;
        assert_eq!(batch.len(), 1);
        let notif_id = batch[0].dedupe_key.clone().unwrap();

        // Not stale yet — should not recover
        let recovered = mgr.recover_stale_claims(60_000).await;
        assert!(recovered.is_empty());

        // Claim again should return empty (still claimed)
        let batch2 = mgr.claim("D").await;
        assert!(batch2.is_empty());

        // Small delay to ensure the claim is older than the threshold
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Recover with 1ms threshold (claim is now stale)
        let recovered = mgr.recover_stale_claims(1).await;
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0], notif_id);

        // After recovery, claim should re-deliver
        let batch3 = mgr.claim("D").await;
        assert_eq!(batch3.len(), 1);
        assert_eq!(batch3[0].dedupe_key.as_ref().unwrap(), &notif_id);
    }

    #[tokio::test]
    async fn test_claim_empty_store() {
        let mgr = test_manager();
        let claimed = mgr.claim("llm").await;
        assert!(claimed.is_empty());
    }

    #[tokio::test]
    async fn test_publish_multiple_events() {
        let mgr = test_manager();
        for i in 0..5 {
            let event = NotificationEvent {
                category: "system".to_string(),
                kind: format!("evt-{i}"),
                severity: "info".to_string(),
                payload: serde_json::json!({"i": i}),
                dedupe_key: None,
            };
            let id = mgr.publish(event).await.unwrap();
            assert!(id.is_some());
        }

        let claimed = mgr.claim("llm").await;
        assert_eq!(claimed.len(), 5);
    }

    #[tokio::test]
    async fn test_consumer_group_offset_tracking_replaced_by_claims() {
        let mgr = test_manager();
        for i in 0..3 {
            let event = NotificationEvent {
                category: "task".to_string(),
                kind: format!("event-{i}"),
                severity: "info".to_string(),
                payload: serde_json::json!({"i": i}),
                dedupe_key: None,
            };
            mgr.publish(event).await.unwrap();
        }

        // subscribe_consumer now uses claims
        let batch1 = mgr.subscribe_consumer("A", 10).await.unwrap();
        assert_eq!(batch1.len(), 3);

        // Second read gets nothing (claimed but not acked)
        let batch2 = mgr.subscribe_consumer("A", 10).await.unwrap();
        assert!(batch2.is_empty());

        // Consumer "B" starts from scratch, gets all 3
        let batch3 = mgr.subscribe_consumer("B", 10).await.unwrap();
        assert_eq!(batch3.len(), 3);
    }

    #[tokio::test]
    async fn test_read_since_persisted_offset_tail() {
        let mgr = test_manager();
        let id_a = mgr
            .publish(NotificationEvent {
                category: "sys".to_string(),
                kind: "a".to_string(),
                severity: "info".to_string(),
                payload: serde_json::json!({}),
                dedupe_key: None,
            })
            .await
            .unwrap()
            .unwrap();
        mgr.publish(NotificationEvent {
            category: "sys".to_string(),
            kind: "b".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({}),
            dedupe_key: None,
        })
        .await
        .unwrap();
        let id_c = mgr
            .publish(NotificationEvent {
                category: "sys".to_string(),
                kind: "c".to_string(),
                severity: "info".to_string(),
                payload: serde_json::json!({}),
                dedupe_key: None,
            })
            .await
            .unwrap()
            .unwrap();

        let all = mgr.read_since_persisted_offset("ui", 10).await.unwrap();
        assert_eq!(all.len(), 3);

        mgr.advance_consumer_offset("ui", &id_a).await.unwrap();
        let tail = mgr.read_since_persisted_offset("ui", 10).await.unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].kind, "b");
        assert_eq!(tail[1].kind, "c");

        mgr.advance_consumer_offset("ui", &id_c).await.unwrap();
        let done = mgr.read_since_persisted_offset("ui", 10).await.unwrap();
        assert!(done.is_empty());
    }
}
