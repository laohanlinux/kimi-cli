use async_trait::async_trait;
use serde_json::Value;
use crate::mcp::client::{MCPClient, MCPContent};
use crate::message::{ContentBlock, ToolMetrics};
use crate::tools::{Tool, ToolContext, ToolOutput, ToolResult};
use std::sync::Arc;

/// Aligns with Python kimi-cli `MCP_MAX_OUTPUT_CHARS` (§7.x / §3.4 parity).
pub const MCP_MAX_OUTPUT_CHARS: usize = 100_000;

fn truncate_mcp_text(s: &str) -> String {
    if s.chars().count() <= MCP_MAX_OUTPUT_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MCP_MAX_OUTPUT_CHARS).collect();
    format!("{head}… [truncated from {} chars]", s.chars().count())
}

#[allow(dead_code)]
pub struct MCPTool {
    name: String,
    description: String,
    schema: Value,
    client: Arc<MCPClient>,
}

impl MCPTool {
    pub fn new(name: String, description: String, schema: Value, client: Arc<MCPClient>) -> Self {
        Self { name, description, schema, client }
    }
}

#[async_trait]
impl Tool for MCPTool {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn schema(&self) -> Value { self.schema.clone() }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let result = self.client.call_tool(&self.name, args).await?;
        let mut text_parts = Vec::new();
        for content in &result.content {
            match content {
                MCPContent::Text(t) => text_parts.push(truncate_mcp_text(t)),
                MCPContent::Image { data, mime } => {
                    text_parts.push(format!("[Image: {} bytes, {}]", data.len(), mime));
                }
                MCPContent::Audio { data, mime } => {
                    text_parts.push(format!("[Audio: {} bytes, {}]", data.len(), mime));
                }
                MCPContent::Resource { uri, mime, text } => {
                    if let Some(t) = text {
                        text_parts.push(format!(
                            "[Resource: {uri}]\n{}",
                            truncate_mcp_text(t)
                        ));
                    } else {
                        let mime = mime.as_deref().unwrap_or("?");
                        text_parts.push(format!("[Resource: {uri} mime={mime}]"));
                    }
                }
            }
        }
        let text = text_parts.join("\n");
        Ok(ToolOutput {
            result: ToolResult {
                r#type: if result.is_error { "error".to_string() } else { "success".to_string() },
                content: vec![ContentBlock::Text { text }],
                summary: if result.is_error { "MCP tool failed".to_string() } else { "MCP tool completed".to_string() },
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_tool_metadata() {
        let client = Arc::new(MCPClient::new("test".to_string(), vec!["echo".to_string()]));
        let schema = serde_json::json!({"type": "object"});
        let tool = MCPTool::new(
            "fs_read".to_string(),
            "Read a file".to_string(),
            schema.clone(),
            client,
        );
        assert_eq!(tool.name(), "fs_read");
        assert_eq!(tool.description(), "Read a file");
        assert_eq!(tool.schema(), schema);
    }

    #[test]
    fn test_mcp_tool_description_empty() {
        let client = Arc::new(MCPClient::new("test".to_string(), vec![]));
        let tool = MCPTool::new(
            "noop".to_string(),
            "".to_string(),
            serde_json::json!({}),
            client,
        );
        assert_eq!(tool.description(), "");
    }

    #[test]
    fn test_mcp_tool_schema_deep() {
        let client = Arc::new(MCPClient::new("test".to_string(), vec![]));
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        });
        let tool = MCPTool::new(
            "fs".to_string(),
            "File system".to_string(),
            schema.clone(),
            client,
        );
        let s = tool.schema();
        assert_eq!(s["type"], "object");
        assert!(s["properties"].get("path").is_some());
    }

    #[test]
    fn test_truncate_mcp_text() {
        let huge = "x".repeat(MCP_MAX_OUTPUT_CHARS + 500);
        let t = super::truncate_mcp_text(&huge);
        assert!(t.contains("truncated"));
        assert!(t.chars().count() < huge.chars().count());
    }
}
