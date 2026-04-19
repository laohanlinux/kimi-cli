use async_trait::async_trait;
use crate::llm::{ChatProvider, HttpGeneration, StreamingGeneration};
use crate::message::{content_to_string, ContentPart, FunctionCall, Message, ToolCall, UserMessage};
use reqwest::Client;
use futures::StreamExt;
use eventsource_stream::Eventsource;

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    /// Session affinity for Anthropic (metadata.user_id).
    session_id: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            session_id: None,
        }
    }

    pub fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = Some(session_id);
        self
    }
}

fn anthropic_user_blocks(u: &UserMessage) -> Vec<serde_json::Value> {
    let mut blocks = Vec::new();
    for p in u.parts() {
        match p {
            ContentPart::Text { text } | ContentPart::Think { text } => {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
            ContentPart::ImageUrl { url } => {
                blocks.push(serde_json::json!({
                    "type": "image",
                    "source": {"type": "url", "url": url}
                }));
            }
            ContentPart::AudioUrl { url } | ContentPart::VideoUrl { url } => {
                blocks.push(serde_json::json!({
                    "type": "text",
                    "text": format!("[media] {}", url)
                }));
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(serde_json::json!({"type": "text", "text": ""}));
    }
    blocks
}

fn build_messages(history: Vec<Message>) -> Vec<serde_json::Value> {
    let mut messages = Vec::new();
    for msg in history {
        match msg {
            Message::System { content } => {
                tracing::debug!("Skipping inline system message for Anthropic: {}", content);
            }
            Message::User(u) => {
                let blocks = anthropic_user_blocks(&u);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": blocks
                }));
            }
            Message::Assistant { content, tool_calls } => {
                let mut blocks = Vec::new();
                if let Some(c) = content {
                    blocks.push(serde_json::json!({"type": "text", "text": c}));
                }
                if let Some(tcs) = tool_calls {
                    for tc in tcs {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input,
                        }));
                    }
                }
                messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
            }
            Message::Tool { tool_call_id, content } => {
                let text = content_to_string(&content);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": text,
                    }]
                }));
            }
            Message::ToolEvent(ev) => {
                let text = content_to_string(&ev.content);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": &ev.tool_call_id,
                        "content": text,
                    }]
                }));
            }
            _ => {}
        }
    }
    messages
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn generate(
        &self,
        system_prompt: Option<String>,
        history: Vec<Message>,
        tools: Vec<serde_json::Value>,
    ) -> anyhow::Result<Box<dyn crate::llm::LLMGeneration>> {
        let messages = build_messages(history);

        if !tools.is_empty() {
            return non_streaming(self, system_prompt, messages, tools).await;
        }

        streaming(self, system_prompt, messages).await
    }
}

async fn non_streaming(
    provider: &AnthropicProvider,
    system_prompt: Option<String>,
    messages: Vec<serde_json::Value>,
    tools: Vec<serde_json::Value>,
) -> anyhow::Result<Box<dyn crate::llm::LLMGeneration>> {
    let mut body = serde_json::json!({
        "model": provider.model,
        "max_tokens": 4096,
        "messages": messages,
    });
    if let Some(sp) = system_prompt {
        body["system"] = serde_json::Value::String(sp);
    }
    if let Some(ref sid) = provider.session_id {
        body["metadata"] = serde_json::json!({"user_id": sid});
    }
    if !tools.is_empty() {
        let anthropic_tools: Vec<serde_json::Value> = tools
            .into_iter()
            .map(|t| {
                let name = t["function"]["name"].as_str().unwrap_or("").to_string();
                let description = t["function"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let input_schema = t["function"]["parameters"].clone();
                serde_json::json!({
                    "name": name,
                    "description": description,
                    "input_schema": input_schema,
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(anthropic_tools);
    }

    let resp = provider
        .client
        .post(format!("{}/v1/messages", provider.base_url))
        .header("x-api-key", &provider.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let text = resp.text().await?;
        anyhow::bail!("Anthropic API error: {}", text);
    }

    let data: serde_json::Value = resp.json().await?;

    let mut chunks = Vec::new();
    let mut tool_calls = Vec::new();
    if let Some(content) = data["content"].as_array() {
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(text) = block["text"].as_str() {
                        chunks.push(ContentPart::Text { text: text.to_string() });
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            arguments: serde_json::to_string(&block["input"]).unwrap_or_default(),
                        },
                    });
                }
                _ => {}
            }
        }
    }

    let usage = data["usage"].as_object().and_then(|u| {
        let prompt = u["input_tokens"].as_u64()? as usize;
        let completion = u["output_tokens"].as_u64()? as usize;
        Some((prompt, completion))
    });

    Ok(Box::new(HttpGeneration::new(chunks, tool_calls, usage)))
}

async fn streaming(
    provider: &AnthropicProvider,
    system_prompt: Option<String>,
    messages: Vec<serde_json::Value>,
) -> anyhow::Result<Box<dyn crate::llm::LLMGeneration>> {
    let mut body = serde_json::json!({
        "model": provider.model,
        "max_tokens": 4096,
        "messages": messages,
        "stream": true,
    });
    if let Some(sp) = system_prompt {
        body["system"] = serde_json::Value::String(sp);
    }
    if let Some(ref sid) = provider.session_id {
        body["metadata"] = serde_json::json!({"user_id": sid});
    }

    let resp = provider
        .client
        .post(format!("{}/v1/messages", provider.base_url))
        .header("x-api-key", &provider.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let text = resp.text().await?;
        anyhow::bail!("Anthropic API error: {}", text);
    }

    let (tx, rx) = tokio::sync::mpsc::channel(100);

    tokio::spawn(async move {
        let mut stream = resp.bytes_stream().eventsource();
        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) => {
                    if ev.event == "message_stop" {
                        break;
                    }
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&ev.data)
                        && let Some(text) = data["delta"]["text"].as_str()
                            && !text.is_empty() {
                                let _ = tx.send(ContentPart::Text { text: text.to_string() }).await;
                            }
                }
                Err(_) => break,
            }
        }
    });

    Ok(Box::new(StreamingGeneration::new(rx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, ToolEvent, ToolStatus};

    #[test]
    fn test_build_messages_user_and_assistant() {
        let msgs = build_messages(vec![
            Message::User(UserMessage::text("hello")),
            Message::Assistant { content: Some("hi".to_string()), tool_calls: None },
        ]);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["text"], "hello");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_build_messages_skips_inline_system() {
        let msgs = build_messages(vec![
            Message::System { content: "sys".to_string() },
            Message::User(UserMessage::text("u")),
        ]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn test_build_messages_tool_event() {
        let msgs = build_messages(vec![Message::ToolEvent(ToolEvent {
            tool_call_id: "tc-1".to_string(),
            tool_name: "shell".to_string(),
            status: ToolStatus::Completed,
            content: vec![ContentBlock::Text { text: "out".to_string() }],
            metrics: None,
            elapsed_ms: None,
        })]);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[0]["content"][0]["tool_use_id"], "tc-1");
    }

    #[test]
    fn test_build_messages_assistant_with_tool_use() {
        let msgs = build_messages(vec![Message::Assistant {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "tc-1".to_string(),
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"/tmp"}"#.to_string(),
                },
            }]),
        }]);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[0]["content"][0]["name"], "read_file");
        assert_eq!(msgs[0]["content"][0]["input"]["path"], "/tmp");
    }
}
