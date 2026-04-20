use crate::tools::{ContentBlock, Tool, ToolContext, ToolMetrics, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

pub struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Execute shell commands"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": { "type": "integer", "default": 300 },
                "run_in_background": { "type": "boolean", "default": false }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);
        if command.is_empty() {
            anyhow::bail!("Empty command");
        }
        let approved = ctx
            .runtime
            .approval
            .request_tool(
                "".to_string(),
                "shell",
                &args,
                command.to_string(),
                command.to_string(),
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
        let start = std::time::Instant::now();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            Command::new("sh").arg("-c").arg(command).output(),
        )
        .await??;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let text = format!("{}{}", stdout, stderr);
        let success = output.status.success();
        let exit_code = output.status.code();
        let summary = if success {
            format!(
                "Command succeeded with exit code {}",
                exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        } else {
            format!(
                "Command failed with exit code {}",
                exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        };
        Ok(ToolOutput {
            result: ToolResult {
                r#type: if success {
                    "success".to_string()
                } else {
                    "error".to_string()
                },
                content: vec![ContentBlock::Text { text }],
                summary,
            },
            artifacts: vec![],
            metrics: ToolMetrics {
                elapsed_ms,
                exit_code,
            },
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
    use crate::wire::RootWireHub;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        let hub = RootWireHub::new();
        let approval = Arc::new(ApprovalRuntime::new(hub.clone(), true, vec![]));
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
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
            token: crate::token::ContextToken::new("test", "test-turn"),
        }
    }

    #[tokio::test]
    async fn test_shell_echo() {
        let tool = ShellTool;
        let ctx = test_ctx();
        let args = serde_json::json!({ "command": "echo hello_rki" });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        assert!(
            output
                .result
                .summary
                .contains("Command succeeded with exit code 0")
        );
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.contains("hello_rki"));
        }
    }

    #[tokio::test]
    async fn test_shell_empty_command_fails() {
        let tool = ShellTool;
        let ctx = test_ctx();
        let args = serde_json::json!({ "command": "" });
        let result = tool.call(args, &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_shell_failed_command() {
        let tool = ShellTool;
        let ctx = test_ctx();
        let args = serde_json::json!({ "command": "exit 1" });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "error");
        assert!(
            output
                .result
                .summary
                .contains("Command failed with exit code 1")
        );
    }

    #[tokio::test]
    async fn test_shell_stderr_included() {
        let tool = ShellTool;
        let ctx = test_ctx();
        let args = serde_json::json!({ "command": "echo err_msg >&2" });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        if let ContentBlock::Text { text } = &output.result.content[0] {
            assert!(text.contains("err_msg"));
        }
    }

    #[tokio::test]
    async fn test_shell_schema_has_required_command() {
        let schema = ShellTool.schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "command"));
    }
}
