use crate::background::TaskStatus;
use crate::tools::function_toolkit::FunctionTool;
use crate::tools::{ContentBlock, Tool, ToolContext, ToolMetrics, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde_json::Value;

/// Stateless function tool: task_list (§7.2 deviation prototype).
pub fn task_list_tool() -> FunctionTool {
    FunctionTool::new(
        "task_list",
        "List background tasks",
        serde_json::json!({
            "type": "object",
            "properties": {
                "active_only": { "type": "boolean", "default": true },
                "limit": { "type": "integer", "default": 20 }
            }
        }),
        |args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                let active_only = args
                    .get("active_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let tasks = ctx.runtime.bg_manager.list().await;
                let filtered: Vec<_> = tasks
                    .into_iter()
                    .filter(|t| {
                        !active_only
                            || matches!(t.status, TaskStatus::Pending | TaskStatus::Running)
                    })
                    .take(limit)
                    .collect();
                let text = filtered
                    .iter()
                    .map(|t| format!("{} {:?}", t.id, t.status))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text { text }],
                        summary: format!("{} tasks", filtered.len()),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based TaskListTool (deprecated, kept for backward compatibility).
pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        "List background tasks"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "active_only": { "type": "boolean", "default": true },
                "limit": { "type": "integer", "default": 20 }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let active_only = args
            .get("active_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let tasks = ctx.runtime.bg_manager.list().await;
        let filtered: Vec<_> = tasks
            .into_iter()
            .filter(|t| {
                !active_only || matches!(t.status, TaskStatus::Pending | TaskStatus::Running)
            })
            .take(limit)
            .collect();
        let text = filtered
            .iter()
            .map(|t| format!("{} {:?}", t.id, t.status))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text }],
                summary: format!("{} tasks", filtered.len()),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

/// Stateless function tool: task_output (§7.2 deviation prototype).
pub fn task_output_tool() -> FunctionTool {
    FunctionTool::new(
        "task_output",
        "Get background task output",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "block": { "type": "boolean", "default": false },
                "timeout": { "type": "integer", "default": 30 }
            },
            "required": ["task_id"]
        }),
        |args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let output = ctx.runtime.bg_manager.output(task_id).await?;
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text { text: output }],
                        summary: "Task output".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based TaskOutputTool (deprecated, kept for backward compatibility).
pub struct TaskOutputTool;

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }
    fn description(&self) -> &str {
        "Get background task output"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "block": { "type": "boolean", "default": false },
                "timeout": { "type": "integer", "default": 30 }
            },
            "required": ["task_id"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        let output = ctx.runtime.bg_manager.output(task_id).await?;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text: output }],
                summary: "Task output".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

/// Stateless function tool: task_stop (§7.2 deviation prototype).
pub fn task_stop_tool() -> FunctionTool {
    FunctionTool::new(
        "task_stop",
        "Stop a background task",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "reason": { "type": "string" }
            },
            "required": ["task_id"]
        }),
        |args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let approved = ctx
                    .runtime
                    .approval
                    .request_tool(
                        "".to_string(),
                        "task_stop",
                        &args,
                        format!("Stop background task {}", task_id),
                        format!("Stop task {}", task_id),
                    )
                    .await?;
                if !approved {
                    return Ok(ToolOutput {
                        result: ToolResult {
                            r#type: "error".to_string(),
                            content: vec![ContentBlock::Text {
                                text: "Approval rejected".to_string(),
                            }],
                            summary: "Approval rejected".to_string(),
                        },
                        artifacts: vec![],
                        metrics: ToolMetrics::default(),
                    });
                }
                ctx.runtime.bg_manager.cancel(task_id).await?;
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text {
                            text: format!("Stopped {}", task_id),
                        }],
                        summary: "Task stopped".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based TaskStopTool (deprecated, kept for backward compatibility).
pub struct TaskStopTool;

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "task_stop"
    }
    fn description(&self) -> &str {
        "Stop a background task"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "reason": { "type": "string" }
            },
            "required": ["task_id"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        let approved = ctx
            .runtime
            .approval
            .request_tool(
                "".to_string(),
                "task_stop",
                &args,
                format!("Stop background task {}", task_id),
                format!("Stop task {}", task_id),
            )
            .await?;
        if !approved {
            return Ok(ToolOutput {
                result: ToolResult {
                    r#type: "error".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Approval rejected".to_string(),
                    }],
                    summary: "Approval rejected".to_string(),
                },
                artifacts: vec![],
                metrics: ToolMetrics::default(),
            });
        }
        ctx.runtime.bg_manager.cancel(task_id).await?;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text {
                    text: format!("Stopped {}", task_id),
                }],
                summary: "Task stopped".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalRuntime;
    use crate::config::Config;
    use crate::runtime::Runtime;
    use crate::session::Session;
    use crate::store::Store;
    use crate::wire::RootWireHub;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let runtime = Runtime::new(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval,
            hub,
            store,
        );
        ToolContext {
            runtime,
            hub: None,
            token: crate::token::ContextToken::new("test", "turn"),
        }
    }

    #[tokio::test]
    async fn test_task_list_empty() {
        let ctx = test_ctx();
        let tool = task_list_tool();
        let out = tool
            .call(serde_json::json!({"active_only": true, "limit": 20}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.result.summary, "0 tasks");
        assert_eq!(tool.name(), "task_list");
    }

    #[tokio::test]
    async fn test_task_stop_missing_returns_success() {
        // cancel() does not error on missing tasks; it silently updates store status
        let ctx = test_ctx();
        let tool = task_stop_tool();
        let result = tool
            .call(serde_json::json!({"task_id": "missing"}), &ctx)
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().result.summary.contains("Task stopped"));
        assert_eq!(tool.name(), "task_stop");
    }

    #[tokio::test]
    async fn test_task_list_inactive_included_when_active_only_false() {
        let ctx = test_ctx();
        let tool = TaskListTool;
        let out = tool
            .call(serde_json::json!({"active_only": false, "limit": 20}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.result.summary, "0 tasks");
    }

    #[test]
    fn test_task_stop_tool_schema() {
        let tool = TaskStopTool;
        let schema = tool.schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "task_id"));
    }
}
