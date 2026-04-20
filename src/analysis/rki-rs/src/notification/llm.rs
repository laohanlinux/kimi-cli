//! LLM-facing notification text — parity with Python `kimi_cli.notifications.llm.build_notification_message`.
//!
//! Produces a tagged block the model can parse; optional `<task-notification>` tail matches Python when
//! `category == "task"` and `source_kind == "background_task"`.

use crate::background::BackgroundTaskManager;
use crate::background::types::{TaskKind, TaskStatus};
use crate::message::{ContentPart, Message};
use crate::notification::task_terminal::status_payload_str;
use crate::notification::types::NotificationEvent;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

const DEFAULT_TAIL_BYTES: usize = 8000;
const DEFAULT_TAIL_LINES: usize = 40;

fn xml_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn tail_slice(s: &str, max_bytes: usize, max_lines: usize) -> String {
    let mut t = s;
    if t.len() > max_bytes {
        t = &t[t.len().saturating_sub(max_bytes)..];
    }
    let lines: Vec<&str> = t.lines().collect();
    if lines.len() > max_lines {
        lines[lines.len() - max_lines..].join("\n")
    } else {
        t.to_string()
    }
}

fn notification_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<notification id="([^"]+)""#).expect("regex"))
}

/// Same pattern as Python `extract_notification_ids` (`kimi_cli.notifications.llm`).
pub fn extract_notification_ids_from_text(text: &str) -> HashSet<String> {
    notification_id_re()
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// Scan user text parts for `<notification id="…">` markers (restored-session ack alignment).
pub fn extract_notification_ids_from_history(messages: &[Message]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for m in messages {
        if let Message::User(u) = m {
            for p in u.parts() {
                if let ContentPart::Text { text } = p {
                    ids.extend(extract_notification_ids_from_text(text));
                }
            }
        }
    }
    ids
}

fn format_task_status(s: &TaskStatus) -> &'static str {
    status_payload_str(s)
}

/// Build the user message body for one notification, matching Python structure.
pub async fn build_notification_message_for_llm(
    notif: &NotificationEvent,
    bg: Option<&BackgroundTaskManager>,
) -> String {
    let id = notif.dedupe_key.as_deref().unwrap_or("");
    let mut lines = vec![
        format!(
            r#"<notification id="{}" category="{}" type="{}" source_kind="{}" source_id="{}">"#,
            xml_escape_attr(id),
            xml_escape_attr(&notif.category),
            xml_escape_attr(&notif.kind),
            xml_escape_attr(&notif.source_kind),
            xml_escape_attr(&notif.source_id),
        ),
        format!("Title: {}", notif.title),
        format!("Severity: {}", notif.severity),
        notif.body.clone(),
    ];

    if notif.category == "task"
        && notif.source_kind == "background_task"
        && !notif.source_id.is_empty()
    {
        if let Some(mgr) = bg {
            let tasks = mgr.list().await;
            if let Some(t) = tasks.iter().find(|t| t.id == notif.source_id) {
                let kind_label = match &t.spec.kind {
                    TaskKind::Bash { .. } => "bash",
                    TaskKind::Agent { .. } => "agent",
                };
                let description = match &t.spec.kind {
                    TaskKind::Bash { command } => command.as_str(),
                    TaskKind::Agent { description, .. } => description.as_str(),
                };
                let mut task_lines = vec![
                    "<task-notification>".to_string(),
                    format!("Task ID: {}", t.id),
                    format!("Task Type: {kind_label}"),
                    format!("Description: {description}"),
                    format!("Status: {}", format_task_status(&t.status)),
                ];
                match &t.status {
                    TaskStatus::Completed { exit_code } => {
                        if let Some(c) = exit_code {
                            task_lines.push(format!("Exit code: {c}"));
                        }
                    }
                    TaskStatus::Failed { reason } => {
                        task_lines.push(format!("Failure reason: {reason}"));
                    }
                    _ => {}
                }
                if let Ok(out) = mgr.output(&notif.source_id).await {
                    let tail = tail_slice(&out, DEFAULT_TAIL_BYTES, DEFAULT_TAIL_LINES);
                    if !tail.is_empty() {
                        task_lines.push("Output tail:".to_string());
                        task_lines.push(tail);
                    }
                }
                task_lines.push("</task-notification>".to_string());
                lines.extend(task_lines);
            }
        }
    }

    lines.push("</notification>".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_build_notification_markup_minimal() {
        let n = NotificationEvent {
            category: "system".to_string(),
            kind: "ping".to_string(),
            severity: "info".to_string(),
            dedupe_key: Some("nid-1".to_string()),
            ..Default::default()
        };
        let s = build_notification_message_for_llm(&n, None).await;
        assert!(s.contains(r#"<notification id="nid-1""#));
        assert!(s.contains(r#"type="ping""#));
        assert!(s.contains("Title:"));
        assert!(s.contains("Severity: info"));
        assert!(s.ends_with("</notification>"));
    }

    #[test]
    fn test_extract_notification_ids_matches_python_pattern() {
        let text = r#"Hello <notification id="abc-123" category="x">"#;
        let ids = extract_notification_ids_from_text(text);
        assert!(ids.contains("abc-123"));
    }

    #[tokio::test]
    async fn test_escapes_quotes_in_attributes() {
        let n = NotificationEvent {
            category: "a\"b".to_string(),
            kind: "k".to_string(),
            severity: "info".to_string(),
            dedupe_key: Some("id".to_string()),
            ..Default::default()
        };
        let s = build_notification_message_for_llm(&n, None).await;
        assert!(s.contains(r#"category="a&quot;b""#), "{s}");
    }
}
