use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Bash { command: String },
    Agent { description: String, prompt: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub kind: TaskKind,
    pub created_at: DateTime<Utc>,
    /// Task IDs that must complete before this task starts (§8.3 deviation).
    pub dependencies: Vec<String>,
    /// Bash tasks only: on non-zero exit, re-`submit` with a fresh id up to this many times when `DistributedQueue` is enabled.
    #[serde(default)]
    pub max_retries: u32,
    /// Wall-clock cap for bash `wait` / agent `execute` (Python `TaskSpec.timeout_s` / `agent_task_timeout_s`).
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed { exit_code: Option<i32> },
    Failed { reason: String },
    Cancelled,
    Lost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRef {
    pub id: String,
    pub spec: TaskSpec,
    pub status: TaskStatus,
    /// Parity with Python `TaskRuntime.timed_out`: failure was due to an outer timeout (distinct `terminal_reason`).
    #[serde(default)]
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEvent {
    Started,
    Output { text: String },
    Completed { exit_code: Option<i32> },
    Failed { reason: String },
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_spec_serde_roundtrip() {
        let spec = TaskSpec {
            id: "t1".to_string(),
            kind: TaskKind::Bash {
                command: "echo hi".to_string(),
            },
            created_at: Utc::now(),
            dependencies: vec!["dep1".to_string()],
            max_retries: 0,
            timeout_s: None,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: TaskSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "t1");
        assert_eq!(back.dependencies, vec!["dep1"]);
        assert_eq!(back.max_retries, 0);
        assert_eq!(back.timeout_s, None);
    }

    #[test]
    fn test_task_status_serde() {
        let s = TaskStatus::Completed { exit_code: Some(0) };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("completed"));
        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskStatus::Completed { exit_code: Some(0) }));
    }

    #[test]
    fn test_task_event_serde() {
        let ev = TaskEvent::Output {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: TaskEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskEvent::Output { text } if text == "hello"));
    }

    #[test]
    fn test_task_status_failed_serde() {
        let s = TaskStatus::Failed {
            reason: "oom".to_string(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("failed"));
        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskStatus::Failed { reason } if reason == "oom"));
    }

    #[test]
    fn test_task_ref_clone() {
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
        let tr = TaskRef {
            id: "t1".to_string(),
            spec: spec.clone(),
            status: TaskStatus::Pending,
            timed_out: false,
        };
        let tr2 = tr.clone();
        assert_eq!(tr2.id, "t1");
        assert!(matches!(tr2.status, TaskStatus::Pending));
    }
}
