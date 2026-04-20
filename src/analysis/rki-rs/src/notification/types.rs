use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

/// Parse SQLite `notifications.created_at` (`datetime('now')` style) to Unix seconds for wire parity.
pub(crate) fn notification_created_at_from_sqlite(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let base = s.split('.').next()?.trim();
    NaiveDateTime::parse_from_str(base, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|n| n.and_utc().timestamp() as f64)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationEvent {
    pub category: String,
    pub kind: String,
    pub severity: String,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// Unix epoch seconds when the row was read from SQLite (`created_at` column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<f64>,
    /// Presentation / routing metadata (persisted in SQLite; mapped to `WireEvent::Notification`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_id: String,
}

impl Default for NotificationEvent {
    fn default() -> Self {
        Self {
            category: String::new(),
            kind: String::new(),
            severity: String::new(),
            payload: serde_json::Value::Null,
            dedupe_key: None,
            created_at: None,
            title: String::new(),
            body: String::new(),
            source_kind: String::new(),
            source_id: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notification_event_serde() {
        let ev = NotificationEvent {
            category: "task".to_string(),
            kind: "done".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"id": "t1"}),
            dedupe_key: Some("key-1".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("task"));
        assert!(json.contains("key-1"));

        let back: NotificationEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.category, "task");
        assert_eq!(back.dedupe_key, Some("key-1".to_string()));
    }

    #[test]
    fn test_notification_event_without_dedupe_key() {
        let ev = NotificationEvent {
            category: "system".to_string(),
            kind: "restart".to_string(),
            severity: "warn".to_string(),
            payload: serde_json::json!({}),
            dedupe_key: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("dedupe_key"));
    }

    #[test]
    fn test_notification_event_severity_levels() {
        for sev in &["info", "warn", "error", "critical"] {
            let ev = NotificationEvent {
                category: "test".to_string(),
                kind: "k".to_string(),
                severity: sev.to_string(),
                payload: serde_json::json!({}),
                dedupe_key: None,
                ..Default::default()
            };
            let json = serde_json::to_string(&ev).unwrap();
            assert!(json.contains(sev));
        }
    }

    #[test]
    fn test_notification_event_equality() {
        let ev1 = NotificationEvent {
            category: "a".to_string(),
            kind: "b".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"x": 1}),
            dedupe_key: Some("k".to_string()),
            ..Default::default()
        };
        let ev2 = NotificationEvent {
            category: "a".to_string(),
            kind: "b".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"x": 1}),
            dedupe_key: Some("k".to_string()),
            ..Default::default()
        };
        assert_eq!(ev1.category, ev2.category);
        assert_eq!(ev1.dedupe_key, ev2.dedupe_key);
    }

    #[test]
    fn test_notification_created_at_from_sqlite() {
        let t = notification_created_at_from_sqlite("2026-04-20 12:34:56").unwrap();
        assert!(t > 1_000_000_000.0);
        assert!(notification_created_at_from_sqlite("").is_none());
        assert!(notification_created_at_from_sqlite("   ").is_none());
    }

    #[test]
    fn test_notification_event_title_body_serde() {
        let ev = NotificationEvent {
            category: "c".to_string(),
            kind: "k".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"x": 1}),
            title: "T".to_string(),
            body: "B".to_string(),
            source_kind: "tool".to_string(),
            source_id: "tc1".to_string(),
            ..Default::default()
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains("T"));
        let back: NotificationEvent = serde_json::from_str(&j).unwrap();
        assert_eq!(back.title, "T");
        assert_eq!(back.source_id, "tc1");
    }
}
