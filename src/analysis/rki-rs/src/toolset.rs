//! Tool registry and dispatch.
//!
//! `Toolset` holds registered tools and routes `ToolCall` requests to the
//! correct implementation, injecting `ToolContext`.

use crate::tools::{Tool, ToolContext, ToolOutput};
use serde_json::Value;
use std::collections::HashMap;

pub struct Toolset {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Toolset {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub async fn handle(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {}", name))?;
        tool.call(args, ctx).await
    }

    pub fn schemas(&self) -> Vec<serde_json::Value> {
        self.tools.values().map(|t| t.schema()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::FunctionTool;
    use async_trait::async_trait;

    struct DummyTool;

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "A dummy tool"
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput {
                result: crate::tools::ToolResult {
                    r#type: "success".to_string(),
                    content: vec![crate::message::ContentBlock::Text {
                        text: "ok".to_string(),
                    }],
                    summary: "done".to_string(),
                },
                artifacts: vec![],
                metrics: crate::message::ToolMetrics::default(),
            })
        }
    }

    fn test_ctx() -> ToolContext {
        let hub = crate::wire::RootWireHub::new();
        let approval = std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
            hub.clone(),
            true,
            vec![],
        ));
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let runtime = crate::runtime::Runtime::new(
            crate::config::Config::default(),
            crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
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
    async fn test_toolset_register_and_handle() {
        let mut ts = Toolset::new();
        ts.register(Box::new(DummyTool));

        let schemas = ts.schemas();
        assert_eq!(schemas.len(), 1);

        let ctx = test_ctx();
        let result = ts.handle("dummy", serde_json::json!({}), &ctx).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().result.summary, "done");
    }

    #[tokio::test]
    async fn test_toolset_unknown_tool_fails() {
        let ts = Toolset::new();
        let ctx = test_ctx();
        let result = ts.handle("missing", serde_json::json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown tool"));
    }

    #[test]
    fn test_toolset_schemas_empty() {
        let ts = Toolset::new();
        assert!(ts.schemas().is_empty());
    }

    #[tokio::test]
    async fn test_toolset_multiple_tools() {
        let mut ts = Toolset::new();
        ts.register(Box::new(DummyTool));
        ts.register(Box::new(DummyTool));
        // Same name should overwrite; still 1 unique tool
        assert_eq!(ts.schemas().len(), 1);
    }

    /// Mirrors `main` §7.2 `fn_ping` registration for dispatch coverage.
    #[tokio::test]
    async fn test_toolset_fn_ping_function_tool() {
        let mut ts = Toolset::new();
        ts.register(Box::new(FunctionTool::new(
            "fn_ping",
            "ping",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "payload": { "type": "string" }
                }
            }),
            |args: Value, _ctx: &ToolContext| async move {
                let text = args
                    .get("payload")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pong")
                    .to_string();
                Ok(ToolOutput {
                    result: crate::tools::ToolResult {
                        r#type: "success".to_string(),
                        content: vec![crate::message::ContentBlock::Text { text }],
                        summary: "fn_ping ok".to_string(),
                    },
                    artifacts: vec![],
                    metrics: crate::message::ToolMetrics::default(),
                })
            },
        )));
        let ctx = test_ctx();
        let out = ts
            .handle("fn_ping", serde_json::json!({ "payload": "hi" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out.result.summary, "fn_ping ok");
        match &out.result.content[0] {
            crate::message::ContentBlock::Text { text } => assert_eq!(text, "hi"),
            _ => panic!("expected text"),
        }
    }
}
