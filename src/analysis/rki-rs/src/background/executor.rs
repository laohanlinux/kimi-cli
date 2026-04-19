use async_trait::async_trait;
use crate::background::types::{TaskSpec, TaskEvent, TaskKind};
use crate::runtime::Runtime;
use crate::wire::RootWireHub;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Protocol for task executors. Each executor handles a specific task kind.
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    /// Returns true if this executor can handle the given task spec.
    fn can_execute(&self, spec: &TaskSpec) -> bool;

    /// Execute the task, yielding events as they occur.
    async fn execute(&self, spec: &TaskSpec) -> Vec<TaskEvent>;
}

#[allow(dead_code)]
pub struct BashExecutor;

#[async_trait]
impl TaskExecutor for BashExecutor {
    fn can_execute(&self, spec: &TaskSpec) -> bool {
        matches!(spec.kind, TaskKind::Bash { .. })
    }

    async fn execute(&self, spec: &TaskSpec) -> Vec<TaskEvent> {
        let mut events = vec![TaskEvent::Started];
        if let TaskKind::Bash { command } = &spec.kind {
            match tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
            {
                Ok(output) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    if !text.is_empty() {
                        events.push(TaskEvent::Output { text });
                    }
                    events.push(TaskEvent::Completed {
                        exit_code: output.status.code(),
                    });
                }
                Err(e) => {
                    events.push(TaskEvent::Failed {
                        reason: e.to_string(),
                    });
                }
            }
        }
        events
    }
}


/// Executor for agent background tasks. Spawns a nested KimiSoul.
pub struct AgentExecutor {
    runtime: Runtime,
}

impl AgentExecutor {
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl TaskExecutor for AgentExecutor {
    fn can_execute(&self, spec: &TaskSpec) -> bool {
        matches!(spec.kind, TaskKind::Agent { .. })
    }

    async fn execute(&self, spec: &TaskSpec) -> Vec<TaskEvent> {
        let mut events = vec![TaskEvent::Started];
        if let TaskKind::Agent { description: _, prompt } = &spec.kind {
            let hub = RootWireHub::new();
            let approval = Arc::new(crate::approval::ApprovalRuntime::new(
                hub.clone(),
                false,
                vec![],
            ));
            let sub_runtime = Runtime::new(
                {
                    let cfg = self.runtime.config.read().await;
                    cfg.clone()
                },
                self.runtime.session.clone(),
                approval,
                hub.clone(),
                self.runtime.store.clone(),
            );
            let context = Arc::new(Mutex::new(
                match crate::context::Context::load(&sub_runtime.store, &sub_runtime.session.id).await {
                    Ok(c) => c,
                    Err(e) => {
                        events.push(TaskEvent::Failed { reason: format!("Context load failed: {}", e) });
                        return events;
                    }
                },
            ));
            let agent = crate::agent::Agent {
                spec: crate::agent::AgentSpec {
                    name: "background_agent".to_string(),
                    system_prompt: "You are a background agent.".to_string(),
                    tools: vec![],
                    capabilities: vec![],
                    ..Default::default()
                },
                system_prompt: "You are a background agent.".to_string(),
            };
            let llm: Arc<dyn crate::llm::ChatProvider> = Arc::new(crate::llm::EchoProvider);
            let soul = crate::soul::KimiSoul::new(agent, context, llm, sub_runtime);
            let mut rx = hub.subscribe();
            let output = Arc::new(Mutex::new(String::new()));
            let output_clone = output.clone();
            let forward = tokio::spawn(async move {
                while let Ok(envelope) = rx.recv().await {
                    if let Ok(line) = serde_json::to_string(&envelope.event) {
                        output_clone.lock().await.push_str(&line);
                        output_clone.lock().await.push('\n');
                    }
                }
            });
            let result = soul.run(prompt, &hub).await;
            drop(hub);
            let _ = forward.await;
            let text = output.lock().await.clone();
            events.push(TaskEvent::Output { text });
            match result {
                Ok(_) => events.push(TaskEvent::Completed { exit_code: Some(0) }),
                Err(e) => events.push(TaskEvent::Failed { reason: e.to_string() }),
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bash_executor_echo() {
        let exec = BashExecutor;
        let spec = TaskSpec {
            id: "t1".to_string(),
            kind: TaskKind::Bash { command: "echo hello_executor".to_string() },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
        };
        assert!(exec.can_execute(&spec));

        let events = exec.execute(&spec).await;
        assert!(matches!(&events[0], TaskEvent::Started));
        let has_output = events.iter().any(|e| matches!(e, TaskEvent::Output { text } if text.contains("hello_executor")));
        assert!(has_output);
        assert!(matches!(events.last().unwrap(), TaskEvent::Completed { exit_code: Some(0) }));
    }

    #[tokio::test]
    async fn test_bash_executor_failed_command() {
        let exec = BashExecutor;
        let spec = TaskSpec {
            id: "t2".to_string(),
            kind: TaskKind::Bash { command: "exit 7".to_string() },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
        };
        let events = exec.execute(&spec).await;
        assert!(matches!(events.last().unwrap(), TaskEvent::Completed { exit_code: Some(7) }));
    }

    #[tokio::test]
    async fn test_bash_executor_cannot_run_agent() {
        let exec = BashExecutor;
        let spec = TaskSpec {
            id: "t3".to_string(),
            kind: TaskKind::Agent { description: "d".to_string(), prompt: "p".to_string() },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
        };
        assert!(!exec.can_execute(&spec));
    }

    #[tokio::test]
    async fn test_bash_executor_empty_output() {
        let exec = BashExecutor;
        let spec = TaskSpec {
            id: "t4".to_string(),
            kind: TaskKind::Bash { command: "true".to_string() },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
        };
        let events = exec.execute(&spec).await;
        assert!(matches!(&events[0], TaskEvent::Started));
        // true produces no output, so last event should be Completed
        assert!(matches!(events.last().unwrap(), TaskEvent::Completed { exit_code: Some(0) }));
    }

    #[tokio::test]
    async fn test_agent_executor_can_run_agent() {
        let exec = AgentExecutor::new(crate::runtime::Runtime::new(
            crate::config::Config::default(),
            crate::session::Session::create(&crate::store::Store::open(std::path::Path::new(":memory:")).unwrap(), std::env::current_dir().unwrap()).unwrap(),
            std::sync::Arc::new(crate::approval::ApprovalRuntime::new(crate::wire::RootWireHub::new(), true, vec![])),
            crate::wire::RootWireHub::new(),
            crate::store::Store::open(std::path::Path::new(":memory:")).unwrap(),
        ));
        let spec = TaskSpec {
            id: "t5".to_string(),
            kind: TaskKind::Agent { description: "d".to_string(), prompt: "p".to_string() },
            created_at: chrono::Utc::now(),
            dependencies: vec![],
            max_retries: 0,
        };
        assert!(exec.can_execute(&spec));
    }
}
