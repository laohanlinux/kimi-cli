//! Background task terminal notifications — parity with Python `BackgroundManager.publish_terminal_notifications`.

use crate::background::types::{TaskKind, TaskRef, TaskSpec, TaskStatus};
use crate::notification::types::NotificationEvent;

fn task_description(spec: &TaskSpec) -> String {
    match &spec.kind {
        TaskKind::Bash { command } => command.clone(),
        TaskKind::Agent { description, .. } => description.clone(),
    }
}

fn task_kind_label(kind: &TaskKind) -> &'static str {
    match kind {
        TaskKind::Bash { .. } => "bash",
        TaskKind::Agent { .. } => "agent",
    }
}

/// Python `view.runtime.status` string in notification payloads (e.g. user-cancelled tasks use `"killed"`).
pub fn status_payload_str(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Completed { .. } => "completed",
        TaskStatus::Failed { .. } => "failed",
        TaskStatus::Cancelled => "killed",
        TaskStatus::Lost => "lost",
    }
}

/// Terminal `task.*` reason for a finished task (`None` while still active).
pub fn terminal_reason_for_task(task: &TaskRef) -> Option<&'static str> {
    match &task.status {
        TaskStatus::Pending | TaskStatus::Running => None,
        TaskStatus::Completed { .. } => Some("completed"),
        TaskStatus::Failed { .. } => Some(if task.timed_out {
            "timed_out"
        } else {
            "failed"
        }),
        TaskStatus::Cancelled => Some("killed"),
        TaskStatus::Lost => Some("lost"),
    }
}

/// `terminal_reason`: `completed`, `failed`, `timed_out`, `killed`, or `lost` (Python `terminal_reason`).
pub fn build_background_task_notification(
    task: &TaskRef,
    terminal_reason: &str,
) -> NotificationEvent {
    let desc = task_description(&task.spec);
    let severity = match terminal_reason {
        "completed" => "success",
        "failed" | "timed_out" => "error",
        "killed" | "lost" => "warning",
        _ => "info",
    };
    let title = match terminal_reason {
        "completed" => format!("Background task completed: {desc}"),
        "timed_out" => format!("Background task timed out: {desc}"),
        "failed" => format!("Background task failed: {desc}"),
        "killed" => format!("Background task stopped: {desc}"),
        "lost" => format!("Background task lost: {desc}"),
        other => format!("Background task updated ({other}): {desc}"),
    };

    let status_str = status_payload_str(&task.status);
    let timed_out_flag = terminal_reason == "timed_out" || task.timed_out;
    let interrupted_flag = matches!(terminal_reason, "timed_out" | "killed");

    let mut body_lines = vec![
        format!("Task ID: {}", task.spec.id),
        format!("Status: {status_str}"),
        format!("Description: {desc}"),
    ];
    if terminal_reason != status_str
        && !matches!(task.status, TaskStatus::Running | TaskStatus::Pending)
    {
        body_lines.push(format!("Terminal reason: {terminal_reason}"));
    }
    match &task.status {
        TaskStatus::Completed { exit_code } => {
            if let Some(c) = exit_code {
                body_lines.push(format!("Exit code: {c}"));
            }
        }
        TaskStatus::Failed { reason } => {
            body_lines.push(format!("Failure reason: {reason}"));
        }
        _ => {}
    }

    let payload = serde_json::json!({
        "task_id": task.spec.id,
        "task_kind": task_kind_label(&task.spec.kind),
        "status": status_str,
        "description": desc,
        "exit_code": match &task.status {
            TaskStatus::Completed { exit_code } => serde_json::to_value(exit_code).unwrap(),
            _ => serde_json::Value::Null,
        },
        "interrupted": interrupted_flag,
        "timed_out": timed_out_flag,
        "terminal_reason": terminal_reason,
        "failure_reason": match &task.status {
            TaskStatus::Failed { reason } => serde_json::Value::String(reason.clone()),
            _ => serde_json::Value::Null,
        },
    });

    let dedupe_key = format!("background_task:{}:{terminal_reason}", task.spec.id);

    NotificationEvent {
        category: "task".to_string(),
        kind: format!("task.{terminal_reason}"),
        severity: severity.to_string(),
        payload,
        dedupe_key: Some(dedupe_key),
        title,
        body: body_lines.join("\n"),
        source_kind: "background_task".to_string(),
        source_id: task.spec.id.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::types::TaskSpec;
    use chrono::Utc;

    #[test]
    fn test_build_completed_bash() {
        let spec = TaskSpec {
            id: "t1".to_string(),
            kind: TaskKind::Bash {
                command: "echo hi".to_string(),
            },
            created_at: Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        let task = TaskRef {
            id: spec.id.clone(),
            spec,
            status: TaskStatus::Completed { exit_code: Some(0) },
            timed_out: false,
        };
        let n = build_background_task_notification(&task, "completed");
        assert_eq!(n.category, "task");
        assert!(n.kind.contains("completed"));
        assert_eq!(n.source_kind, "background_task");
        assert_eq!(n.source_id, "t1");
        assert!(n.body.contains("Task ID: t1"));
        assert!(
            n.dedupe_key
                .as_ref()
                .unwrap()
                .contains("background_task:t1:completed")
        );
        assert_eq!(n.payload["timed_out"], false);
        assert_eq!(n.payload["interrupted"], false);
    }

    #[test]
    fn test_build_timed_out_matches_python_payload_shape() {
        let spec = TaskSpec {
            id: "t-timeout".to_string(),
            kind: TaskKind::Bash {
                command: "sleep 9".to_string(),
            },
            created_at: Utc::now(),
            dependencies: vec![],
            max_retries: 0,
            timeout_s: None,
        };
        let task = TaskRef {
            id: spec.id.clone(),
            spec,
            status: TaskStatus::Failed {
                reason: "wall clock exceeded".to_string(),
            },
            timed_out: true,
        };
        let n = build_background_task_notification(&task, "timed_out");
        assert_eq!(n.kind, "task.timed_out");
        assert!(n.title.contains("timed out"));
        assert_eq!(n.severity, "error");
        assert!(n.body.contains("Terminal reason: timed_out"));
        assert_eq!(n.payload["status"], "failed");
        assert_eq!(n.payload["terminal_reason"], "timed_out");
        assert_eq!(n.payload["timed_out"], true);
        assert_eq!(n.payload["interrupted"], true);
    }
}
