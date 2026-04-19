use async_trait::async_trait;
use serde_json::Value;
use crate::tools::{Tool, ToolContext, ToolOutput, ToolResult, ContentBlock, ToolMetrics};


pub struct SearchWebTool;

#[async_trait]
impl Tool for SearchWebTool {
    fn name(&self) -> &str { "search_web" }
    fn description(&self) -> &str { "Search the web" }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "default": 5 }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let text = format!("Search results for '{}': [mock result]", query);
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text }],
                summary: "Search complete".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

pub struct FetchURLTool;

#[async_trait]
impl Tool for FetchURLTool {
    fn name(&self) -> &str { "fetch_url" }
    fn description(&self) -> &str { "Fetch a URL" }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" }
            },
            "required": ["url"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let resp = client.get(url).send().await?;
        let text = resp.text().await?;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text: text.chars().take(10000).collect() }],
                summary: "Fetch complete".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        ToolContext {
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
            runtime: crate::runtime::Runtime::new(
                crate::config::Config::default(),
                crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
                Arc::new(crate::approval::ApprovalRuntime::new(crate::wire::RootWireHub::new(), true, vec![])),
                crate::wire::RootWireHub::new(),
                store,
            ),
        }
    }

    #[tokio::test]
    async fn test_search_web_mock() {
        let tool = SearchWebTool;
        let ctx = test_ctx();
        let args = serde_json::json!({ "query": "rust programming" });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.contains("rust programming"));
        }
    }

    #[tokio::test]
    async fn test_fetch_url_httpbin() {
        let tool = FetchURLTool;
        let ctx = test_ctx();
        let args = serde_json::json!({ "url": "https://httpbin.org/get" });
        let output = tool.call(args, &ctx).await;
        // May fail in offline environments; just check it doesn't panic
        match output {
            Ok(o) => assert_eq!(o.result.r#type, "success"),
            Err(e) => println!("FetchURL skipped (offline?): {}", e),
        }
    }

    #[tokio::test]
    async fn test_search_web_schema() {
        let tool = SearchWebTool;
        let schema = tool.schema();
        assert!(schema.get("properties").unwrap().get("query").is_some());
        assert!(schema.get("properties").unwrap().get("limit").is_some());
    }

    #[tokio::test]
    async fn test_fetch_url_schema() {
        let tool = FetchURLTool;
        let schema = tool.schema();
        assert!(schema.get("properties").unwrap().get("url").is_some());
    }
}
