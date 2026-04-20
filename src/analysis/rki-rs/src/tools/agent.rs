use crate::agent::AgentSpec;
use crate::subagents::ForegroundSubagentRunner;
use crate::tools::{ContentBlock, Tool, ToolContext, ToolMetrics, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde_json::Value;

pub struct AgentTool;

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        "Create or resume a subagent"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": { "type": "string" },
                "prompt": { "type": "string" },
                "subagent_type": { "type": "string", "default": "default" },
                "model": { "type": "string" },
                "resume": { "type": "string" },
                "run_in_background": { "type": "boolean", "default": false },
                "timeout": { "type": "integer", "default": 300 }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let run_in_background = args
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if run_in_background {
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);
            let spec = crate::background::types::TaskSpec {
                id: uuid::Uuid::new_v4().to_string(),
                kind: crate::background::types::TaskKind::Agent {
                    description,
                    prompt,
                },
                created_at: chrono::Utc::now(),
                dependencies: vec![],
                max_retries: 0,
                timeout_s: Some(timeout),
            };
            let task_id = ctx.runtime.bg_manager.submit(spec).await?;
            return Ok(ToolOutput {
                result: ToolResult {
                    r#type: "success".to_string(),
                    content: vec![ContentBlock::Text {
                        text: format!("Background agent task submitted: {}", task_id),
                    }],
                    summary: "Background agent started".to_string(),
                },
                artifacts: vec![],
                metrics: ToolMetrics::default(),
            });
        }

        let hub = ctx
            .hub
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No hub available for subagent"))?;
        let agent_spec = AgentSpec {
            name: "subagent".to_string(),
            system_prompt: "You are a helpful subagent.".to_string(),
            tools: vec![],
            capabilities: vec![],
            ..Default::default()
        };

        let parent_tool_call_id = ctx
            .token
            .tool_call_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let result = ForegroundSubagentRunner::run(
            &ctx.runtime,
            hub,
            parent_tool_call_id,
            agent_spec,
            prompt,
        )
        .await?;

        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text: result }],
                summary: "Subagent completed".to_string(),
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
    async fn test_agent_tool_background_submission() {
        let ctx = test_ctx();
        let tool = AgentTool;
        let args = serde_json::json!({
            "description": "bg task",
            "prompt": "do something",
            "run_in_background": true
        });
        let out = tool.call(args, &ctx).await.unwrap();
        assert_eq!(out.result.summary, "Background agent started");
        if let ContentBlock::Text { text } = &out.result.content[0] {
            assert!(text.contains("Background agent task submitted"));
        } else {
            panic!("Expected Text block");
        }
    }

    #[tokio::test]
    async fn test_agent_tool_foreground_requires_wire() {
        let ctx = test_ctx();
        let tool = AgentTool;
        let args = serde_json::json!({
            "description": "fg task",
            "prompt": "do something",
            "run_in_background": false
        });
        // Without a hub, foreground subagent should fail
        let result = tool.call(args, &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No hub available"));
    }

    #[test]
    fn test_agent_tool_schema() {
        let tool = AgentTool;
        let schema = tool.schema();
        assert!(schema["properties"].get("description").is_some());
        assert!(schema["properties"].get("prompt").is_some());
        assert!(schema["properties"].get("run_in_background").is_some());
    }
}
