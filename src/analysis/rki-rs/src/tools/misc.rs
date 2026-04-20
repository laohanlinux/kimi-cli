use crate::message::Message;
use crate::tools::function_toolkit::FunctionTool;
use crate::tools::{ContentBlock, Tool, ToolContext, ToolMetrics, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde_json::Value;

/// Stateless function tool: think (§7.2 deviation prototype).
/// Replaces struct-based ThinkTool with a pure async function.
pub fn think_tool() -> FunctionTool {
    FunctionTool::new(
        "think",
        "Log a thought",
        serde_json::json!({
            "type": "object",
            "properties": {
                "thought": { "type": "string" }
            },
            "required": ["thought"]
        }),
        |args: Value, _ctx: &ToolContext| async move {
            let thought = args.get("thought").and_then(|v| v.as_str()).unwrap_or("");
            tracing::info!("THINK: {}", thought);
            Ok(ToolOutput {
                result: ToolResult {
                    r#type: "success".to_string(),
                    content: vec![],
                    summary: "Thought logged".to_string(),
                },
                artifacts: vec![],
                metrics: ToolMetrics::default(),
            })
        },
    )
}

/// Legacy struct-based ThinkTool (deprecated, kept for backward compatibility).
pub struct ThinkTool;

#[async_trait]
impl Tool for ThinkTool {
    fn name(&self) -> &str {
        "think"
    }
    fn description(&self) -> &str {
        "Log a thought"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "thought": { "type": "string" }
            },
            "required": ["thought"]
        })
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let thought = args.get("thought").and_then(|v| v.as_str()).unwrap_or("");
        tracing::info!("THINK: {}", thought);
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![],
                summary: "Thought logged".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

/// Stateless function tool: set_todo_list (§7.2 deviation prototype).
pub fn set_todo_list_tool() -> FunctionTool {
    FunctionTool::new(
        "set_todo_list",
        "Set the todo list",
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["todos"]
        }),
        |args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                let todos: Vec<String> = args
                    .get("todos")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let state_path = ctx.runtime.session.dir.join("state.json");
                let mut state = if state_path.exists() {
                    let content = tokio::fs::read_to_string(&state_path).await?;
                    serde_json::from_str::<serde_json::Value>(&content)
                        .unwrap_or_else(|_| serde_json::json!({}))
                } else {
                    serde_json::json!({})
                };
                state["todos"] = serde_json::to_value(&todos)?;
                tokio::fs::write(&state_path, serde_json::to_string_pretty(&state)?).await?;
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![],
                        summary: format!("Set {} todos", todos.len()),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based SetTodoListTool (deprecated, kept for backward compatibility).
pub struct SetTodoListTool;

#[async_trait]
impl Tool for SetTodoListTool {
    fn name(&self) -> &str {
        "set_todo_list"
    }
    fn description(&self) -> &str {
        "Set the todo list"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["todos"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let todos: Vec<String> = args
            .get("todos")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let state_path = ctx.runtime.session.dir.join("state.json");
        let mut state = if state_path.exists() {
            let content = tokio::fs::read_to_string(&state_path).await?;
            serde_json::from_str::<serde_json::Value>(&content)
                .unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };
        state["todos"] = serde_json::to_value(&todos)?;
        tokio::fs::write(&state_path, serde_json::to_string_pretty(&state)?).await?;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![],
                summary: format!("Set {} todos", todos.len()),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

/// Stateless function tool: ask_user_question (§7.2 deviation prototype).
pub fn ask_user_question_tool() -> FunctionTool {
    FunctionTool::new(
        "ask_user_question",
        "Ask the user a question and wait for an answer",
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["questions"]
        }),
        |args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                let questions: Vec<String> = args
                    .get("questions")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let qs: Vec<crate::wire::Question> = questions
                    .iter()
                    .map(|q| crate::wire::Question {
                        question: q.clone(),
                        options: vec![],
                    })
                    .collect();
                let answers = ctx.runtime.question.request(qs).await?;
                let text = answers.join("\n");
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text { text }],
                        summary: "Question answered".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based AskUserQuestionTool (deprecated, kept for backward compatibility).
pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "ask_user_question"
    }
    fn description(&self) -> &str {
        "Ask the user a question and wait for an answer"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["questions"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let questions: Vec<String> = args
            .get("questions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let qs: Vec<crate::wire::Question> = questions
            .iter()
            .map(|q| crate::wire::Question {
                question: q.clone(),
                options: vec![],
            })
            .collect();
        let answers = ctx.runtime.question.request(qs).await?;
        let text = answers.join("\n");
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text { text }],
                summary: "Question answered".to_string(),
            },
            artifacts: vec![],
            metrics: ToolMetrics::default(),
        })
    }
}

/// Stateless function tool: send_dmail (§7.2 deviation prototype).
pub fn send_dmail_tool() -> FunctionTool {
    FunctionTool::new(
        "send_dmail",
        "Send a D-Mail to a past checkpoint",
        serde_json::json!({
            "type": "object",
            "properties": {
                "checkpoint_id": { "type": "integer" },
                "messages": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["checkpoint_id", "messages"]
        }),
        |args: Value, ctx: &ToolContext| {
            let ctx = ctx.clone();
            async move {
                let checkpoint_id = args
                    .get("checkpoint_id")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let messages: Vec<String> = args
                    .get("messages")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let msgs: Vec<Message> = messages
                    .into_iter()
                    .map(|m| Message::User(crate::message::UserMessage::text(m)))
                    .collect();
                ctx.runtime.denwa_renji.send(checkpoint_id, msgs).await;
                Ok(ToolOutput {
                    result: ToolResult {
                        r#type: "success".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "D-Mail sent".to_string(),
                        }],
                        summary: "D-Mail queued".to_string(),
                    },
                    artifacts: vec![],
                    metrics: ToolMetrics::default(),
                })
            }
        },
    )
}

/// Legacy struct-based SendDMailTool (deprecated, kept for backward compatibility).
pub struct SendDMailTool;

#[async_trait]
impl Tool for SendDMailTool {
    fn name(&self) -> &str {
        "send_dmail"
    }
    fn description(&self) -> &str {
        "Send a D-Mail to a past checkpoint"
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "checkpoint_id": { "type": "integer" },
                "messages": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["checkpoint_id", "messages"]
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let checkpoint_id = args
            .get("checkpoint_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let messages: Vec<String> = args
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let msgs: Vec<Message> = messages
            .into_iter()
            .map(|m| Message::User(crate::message::UserMessage::text(m)))
            .collect();
        ctx.runtime.denwa_renji.send(checkpoint_id, msgs).await;
        Ok(ToolOutput {
            result: ToolResult {
                r#type: "success".to_string(),
                content: vec![ContentBlock::Text {
                    text: "D-Mail sent".to_string(),
                }],
                summary: "D-Mail queued".to_string(),
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
                Arc::new(crate::approval::ApprovalRuntime::new(
                    crate::wire::RootWireHub::new(),
                    true,
                    vec![],
                )),
                crate::wire::RootWireHub::new(),
                store,
            ),
        }
    }

    #[tokio::test]
    async fn test_think_tool() {
        let tool = think_tool();
        let ctx = test_ctx();
        let args = serde_json::json!({ "thought": "I should refactor this" });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        assert!(output.result.summary.contains("Thought"));
        assert_eq!(tool.name(), "think");
        assert_eq!(tool.description(), "Log a thought");
    }

    #[tokio::test]
    async fn test_set_todo_list_tool() {
        let tool = set_todo_list_tool();
        let ctx = test_ctx();
        let args = serde_json::json!({ "todos": ["fix bug", "write tests"] });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        assert!(output.result.summary.contains("2"));
        assert_eq!(tool.name(), "set_todo_list");
    }

    #[tokio::test]
    async fn test_send_dmail_tool() {
        let tool = send_dmail_tool();
        let ctx = test_ctx();
        let args = serde_json::json!({ "checkpoint_id": 42, "messages": ["hello from past"] });
        let output = tool.call(args, &ctx).await.unwrap();
        assert_eq!(output.result.r#type, "success");
        assert!(output.result.summary.contains("D-Mail"));
        assert_eq!(tool.name(), "send_dmail");
    }

    #[tokio::test]
    async fn test_dmail_triggers_back_to_the_future() {
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let hub = crate::wire::RootWireHub::new();
        let approval = std::sync::Arc::new(crate::approval::ApprovalRuntime::new(
            hub.clone(),
            true,
            vec![],
        ));
        let session =
            crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap();
        let runtime = crate::runtime::Runtime::new(
            crate::config::Config::default(),
            session.clone(),
            approval,
            hub,
            store.clone(),
        );

        // Send a D-Mail
        runtime
            .denwa_renji
            .send(
                1,
                vec![crate::message::Message::User(
                    crate::message::UserMessage::text("time travel message"),
                )],
            )
            .await;

        // Claim it
        let dmail = runtime.denwa_renji.claim().await;
        assert!(dmail.is_some());
        let (cp, msgs) = dmail.unwrap();
        assert_eq!(cp, 1);
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn test_ask_user_question_tool() {
        let tool = AskUserQuestionTool;
        let ctx = test_ctx();
        let questions =
            vec![serde_json::json!({"question": "What is your name?", "options": null})];

        // Subscribe to hub to capture the question id and resolve it
        let mut rx = ctx.runtime.hub.subscribe();
        let qm = ctx.runtime.question.clone();
        let resolve_handle = tokio::spawn(async move {
            while let Ok(envelope) = rx.recv().await {
                if let crate::wire::WireEvent::QuestionRequest { id, .. } = envelope.event {
                    let _ = qm.resolve(id, vec!["Test User".to_string()]).await;
                    break;
                }
            }
        });

        let out = tool
            .call(serde_json::json!({"questions": questions}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.result.r#type, "success");
        assert!(out.result.summary.contains("Question answered"));
        let _ = resolve_handle.await;
    }
}
