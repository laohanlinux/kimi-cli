use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationEvent {
    pub category: String,
    pub kind: String,
    pub severity: String,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
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
        };
        let ev2 = NotificationEvent {
            category: "a".to_string(),
            kind: "b".to_string(),
            severity: "info".to_string(),
            payload: serde_json::json!({"x": 1}),
            dedupe_key: Some("k".to_string()),
        };
        assert_eq!(ev1.category, ev2.category);
        assert_eq!(ev1.dedupe_key, ev2.dedupe_key);
    }
}
