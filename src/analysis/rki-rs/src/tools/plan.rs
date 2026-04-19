use async_trait::async_trait;
use serde_json::Value;
use crate::tools::{Tool, ToolContext, ToolOutput, ToolResult, ContentBlock, ToolMetrics};
use crate::tools::function_toolkit::FunctionTool;

/// Stateless function tool: enter_plan_mode (§7.2 deviation prototype).
pub fn enter_plan_mode_tool() -> FunctionTool {
    FunctionTool::new(
        "enter_plan_mode",
        "Enter plan mode (read-only research). No tools will be used.",
        serde_json::json!({ "type": "object", "properties": {} }),
        |_args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                ctx.runtime.enter_plan_mode().await;
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "Entered plan mode. Tools and destructive operations are paused. The agent will think step by step without using tools.".to_string(),
                        }],
                        summary: "Plan mode ON".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based EnterPlanModeTool (deprecated, kept for backward compatibility).
pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str { "enter_plan_mode" }
    fn description(&self) -> &str { "Enter plan mode (read-only research). No tools will be used." }
    fn schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        ctx.runtime.enter_plan_mode().await;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text {
                    text: "Entered plan mode. Tools and destructive operations are paused. The agent will think step by step without using tools.".to_string(),
                }],
                summary: "Plan mode ON".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

/// Stateless function tool: exit_plan_mode (§7.2 deviation prototype).
pub fn exit_plan_mode_tool() -> FunctionTool {
    FunctionTool::new(
        "exit_plan_mode",
        "Exit plan mode and resume normal tool-using operation.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "options": {
                    "type": "object",
                    "properties": {
                        "approve": { "type": "boolean" }
                    }
                }
            }
        }),
        |_args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                ctx.runtime.exit_plan_mode().await;
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "Exited plan mode. Normal operation resumed.".to_string(),
                        }],
                        summary: "Plan mode OFF".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based ExitPlanModeTool (deprecated, kept for backward compatibility).
pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str { "exit_plan_mode" }
    fn description(&self) -> &str { "Exit plan mode and resume normal tool-using operation." }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "options": {
                    "type": "object",
                    "properties": {
                        "approve": { "type": "boolean" }
                    }
                }
            }
        })
    }

    async fn call(&self, _args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        ctx.runtime.exit_plan_mode().await;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text {
                    text: "Exited plan mode. Normal operation resumed.".to_string(),
                }],
                summary: "Plan mode OFF".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;
    use crate::config::Config;
    use crate::approval::ApprovalRuntime;
    use crate::wire::RootWireHub;
    use crate::session::Session;
    use crate::store::Store;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let runtime = Runtime::new(
            Config::default(),
            Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            approval, hub, store,
        );
        ToolContext { runtime, hub: None, token: crate::token::ContextToken::new("test", "turn") }
    }

    #[tokio::test]
    async fn test_enter_plan_mode() {
        let ctx = test_ctx();
        let tool = enter_plan_mode_tool();
        let out = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(out.result.summary, "Plan mode ON");
        assert!(ctx.runtime.is_plan_mode().await);
        assert_eq!(tool.name(), "enter_plan_mode");
    }

    #[tokio::test]
    async fn test_exit_plan_mode() {
        let ctx = test_ctx();
        ctx.runtime.enter_plan_mode().await;
        let tool = exit_plan_mode_tool();
        let out = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(out.result.summary, "Plan mode OFF");
        assert!(!ctx.runtime.is_plan_mode().await);
        assert_eq!(tool.name(), "exit_plan_mode");
    }

    #[test]
    fn test_enter_plan_tool_schema() {
        let tool = EnterPlanModeTool;
        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn test_exit_plan_tool_name() {
        let tool = ExitPlanModeTool;
        assert_eq!(tool.name(), "exit_plan_mode");
        assert!(tool.description().contains("resume"));
    }
}
