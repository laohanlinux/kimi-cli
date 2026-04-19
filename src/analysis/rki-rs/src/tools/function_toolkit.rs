use async_trait::async_trait;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

use crate::tools::{Tool, ToolContext, ToolOutput};

/// A tool implemented as a pure async function (§7.2 deviation prototype).
/// Eliminates the need for struct-based Tool impls; tools are stateless
/// functions with dependency injection via ToolContext.
#[allow(clippy::type_complexity)]
pub struct FunctionTool {
    name: String,
    description: String,
    schema: Value,
    handler: Box<
        dyn Fn(Value, &ToolContext) -> Pin<Box<dyn Future<Output = anyhow::Result<ToolOutput>> + Send>>
            + Send
            + Sync,
    >,
}

impl FunctionTool {
    /// Create a new function tool from its metadata and handler.
    pub fn new<
        F: Fn(Value, &ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<ToolOutput>> + Send + 'static,
    >(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
        handler: F,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            handler: Box::new(move |args, ctx| Box::pin(handler(args, ctx))),
        }
    }


}

#[async_trait]
impl Tool for FunctionTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        (self.handler)(args, ctx).await
    }
}

/// Builder for constructing function tools with fluent API.
pub struct FunctionToolBuilder {
    name: String,
    description: String,
    schema: Option<Value>,
}

impl FunctionToolBuilder {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema: None,
        }
    }

    pub fn schema(mut self, schema: Value) -> Self {
        self.schema = Some(schema);
        self
    }

    #[allow(dead_code)]
    pub fn schema_from_type<T: serde::Serialize>(mut self, value: T) -> anyhow::Result<Self> {
        self.schema = Some(serde_json::to_value(value)?);
        Ok(self)
    }

    pub fn handler<
        F: Fn(Value, &ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<ToolOutput>> + Send + 'static,
    >(
        self,
        handler: F,
    ) -> FunctionTool {
        FunctionTool::new(
            self.name,
            self.description,
            self.schema
                .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} })),
            handler,
        )
    }
}

/// Macro for quick function tool definition.
/// Usage: `tool_fn!("echo", "Echo back input", echo_tool)`
#[macro_export]
macro_rules! tool_fn {
    ($name:expr, $desc:expr, $handler:expr) => {
        $crate::tools::function_toolkit::FunctionTool::new(
            $name,
            $desc,
            serde_json::json!({ "type": "object", "properties": {} }),
            $handler,
        )
    };
    ($name:expr, $desc:expr, $schema:expr, $handler:expr) => {
        $crate::tools::function_toolkit::FunctionTool::new(
            $name,
            $desc,
            $schema,
            $handler,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ContentBlock;
    use crate::tools::{ToolResult, ToolMetrics};

    #[tokio::test]
    async fn test_function_tool_basic() {
        let tool = FunctionTool::new(
            "echo",
            "Echo back the input",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                }
            }),
            |args: Value, _ctx: &ToolContext| async move {
                let msg = args["message"].as_str().unwrap_or("");
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text {
                            text: msg.to_string(),
                        }],
                        summary: "Echoed".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            },
        );

        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.description(), "Echo back the input");

        let ctx = ToolContext {
            runtime: crate::runtime::Runtime::new(
                crate::config::Config {
                    max_steps_per_turn: Some(10),
                    max_context_size: Some(128_000),
                    ..crate::config::Config::default()
                },
                crate::session::Session::create(
                    &crate::store::Store::open(std::path::Path::new(":memory:")).unwrap(),
                    std::env::current_dir().unwrap(),
                )
                .unwrap(),
                std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                crate::store::Store::open(std::path::Path::new(":memory:")).unwrap(),
            ),
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
        };

        let result = tool
            .call(serde_json::json!({ "message": "hello" }), &ctx)
            .await
            .unwrap();
        assert_eq!(result.result.r#type, "success");
        assert_eq!(result.result.summary, "Echoed");
        match &result.result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            _ => panic!("Expected text block"),
        }
    }

    #[tokio::test]
    async fn test_function_tool_builder() {
        let tool = FunctionToolBuilder::new("add", "Add two numbers")
            .schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "b": { "type": "integer" }
                }
            }))
            .handler(|args: Value, _ctx: &ToolContext| async move {
                let a = args["a"].as_i64().unwrap_or(0);
                let b = args["b"].as_i64().unwrap_or(0);
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text {
                            text: format!("{}", a + b),
                        }],
                        summary: format!("Sum: {}", a + b),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            });

        let ctx = ToolContext {
            runtime: crate::runtime::Runtime::new(
                crate::config::Config {
                    max_steps_per_turn: Some(10),
                    max_context_size: Some(128_000),
                    ..crate::config::Config::default()
                },
                crate::session::Session::create(
                    &crate::store::Store::open(std::path::Path::new(":memory:")).unwrap(),
                    std::env::current_dir().unwrap(),
                )
                .unwrap(),
                std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                crate::store::Store::open(std::path::Path::new(":memory:")).unwrap(),
            ),
            hub: None,
            token: crate::token::ContextToken::new("test", "test-turn"),
        };

        let result = tool.call(serde_json::json!({ "a": 3, "b": 4 }), &ctx).await.unwrap();
        match &result.result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "7"),
            _ => panic!("Expected text block"),
        }
    }

    #[test]
    fn test_function_tool_builder_default_schema() {
        let tool = FunctionToolBuilder::new("noop", "No operation")
            .handler(|_args: Value, _ctx: &ToolContext| async move {
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![],
                        summary: "Done".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            });

        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
    }

    #[tokio::test]
    async fn test_tool_fn_macro() {
        let tool = tool_fn!("macro_echo", "Macro echo", |args: Value, _ctx: &ToolContext| async move {
            Ok(ToolOutput {
                result: ToolResult {
                    r#type: "success".to_string(),
                    content: vec![ContentBlock::Text { text: args.to_string() }],
                    summary: "Echo".to_string(),
                },
                artifacts: vec![],
                metrics: ToolMetrics::default(),
            })
        });

        assert_eq!(tool.name(), "macro_echo");
        assert_eq!(tool.description(), "Macro echo");
    }
}
