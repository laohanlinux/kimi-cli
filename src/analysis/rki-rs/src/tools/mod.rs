//! Tool trait and built-in tool implementations.
//!
//! All tools implement `Tool`: `name`, `description`, `schema`, and async `call`.

use crate::message::{Artifact, ContentBlock, ToolMetrics};
use crate::runtime::Runtime;
use crate::token::ContextToken;
use crate::wire::RootWireHub;
use async_trait::async_trait;
use serde_json::Value;

pub mod agent;
pub mod file;
pub mod function_toolkit;
pub mod manifest;
pub mod misc;
pub mod plan;
pub mod shell;
pub mod task;
pub mod web;

pub use agent::AgentTool;
pub use file::{
    GlobTool, GrepTool, ReadFileTool, ReadMediaFileTool, StrReplaceFileTool, WriteFileTool,
};
pub use function_toolkit::{FunctionTool, FunctionToolBuilder};
pub use manifest::{ManifestTool, discover_manifests};
pub use misc::{
    AskUserQuestionTool, SendDMailTool, SetTodoListTool, ThinkTool, ask_user_question_tool,
    send_dmail_tool, set_todo_list_tool, think_tool,
};
pub use plan::{EnterPlanModeTool, ExitPlanModeTool, enter_plan_mode_tool, exit_plan_mode_tool};
pub use shell::ShellTool;
pub use task::{
    TaskListTool, TaskOutputTool, TaskStopTool, task_list_tool, task_output_tool, task_stop_tool,
};
pub use web::{FetchURLTool, SearchWebTool};

#[derive(Clone)]
#[allow(dead_code)]
pub struct ToolContext {
    pub runtime: Runtime,
    pub hub: Option<RootWireHub>,
    pub token: ContextToken,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ToolOutput {
    pub result: ToolResult,
    pub artifacts: Vec<Artifact>,
    pub metrics: ToolMetrics,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub r#type: String,
    pub content: Vec<ContentBlock>,
    pub summary: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_output_construction() {
        let output = ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
                summary: "done".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics {
                elapsed_ms: 42,
                exit_code: Some(0),
            },
        };
        assert_eq!(output.result.r#type, "success");
        assert_eq!(output.metrics.elapsed_ms, 42);
    }

    #[test]
    fn test_tool_result_error_variant() {
        let result = ToolResult {
            r#type: "error".to_string(),
            content: vec![ContentBlock::Traceback {
                text: "panic".to_string(),
            }],
            summary: "failed".to_string(),
        };
        assert!(matches!(result.content[0], ContentBlock::Traceback { .. }));
    }

    #[test]
    fn test_artifact_construction() {
        let art = Artifact {
            name: "a1".to_string(),
            path: Some("/tmp/a1".to_string()),
            mime: "text/plain".to_string(),
            data: b"hello".to_vec(),
        };
        assert_eq!(art.name, "a1");
        assert_eq!(art.mime, "text/plain");
    }

    #[test]
    fn test_tool_metrics_default() {
        let m = ToolMetrics::default();
        assert_eq!(m.elapsed_ms, 0);
        assert_eq!(m.exit_code, None);
    }
}
