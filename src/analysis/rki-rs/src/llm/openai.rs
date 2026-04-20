use crate::llm::{ChatProvider, HttpGeneration, StreamingGeneration};
use crate::message::{
    ContentPart, FunctionCall, Message, ToolCall, UserMessage, content_to_string,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    /// Session affinity key for prompt caching (Kimi-specific).
    session_id: Option<String>,
}

impl OpenAIProvider {
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

fn openai_user_content(u: &UserMessage) -> serde_json::Value {
    let mut blocks = Vec::new();
    for p in u.parts() {
        match p {
            ContentPart::Text { text } | ContentPart::Think { text } => {
                blocks.push(serde_json::json!({"type": "text", "text": text}));
            }
            ContentPart::ImageUrl { url } => {
                blocks.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": url}
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
        return serde_json::Value::String(String::new());
    }
    if blocks.len() == 1 {
        if let Some(obj) = blocks[0].as_object() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("text") {
                return blocks[0]["text"].clone();
            }
        }
    }
    serde_json::Value::Array(blocks)
}

fn build_messages(system_prompt: Option<String>, history: Vec<Message>) -> Vec<serde_json::Value> {
    let mut messages = Vec::new();
    if let Some(sp) = system_prompt {
        messages.push(serde_json::json!({"role": "system", "content": sp}));
    }
    for msg in history {
        match msg {
            Message::System { content } => {
                messages.push(serde_json::json!({"role": "system", "content": content}));
            }
            Message::User(u) => {
                let content = openai_user_content(&u);
                messages.push(serde_json::json!({"role": "user", "content": content}));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut m = serde_json::json!({"role": "assistant"});
                if let Some(c) = content {
                    m["content"] = serde_json::Value::String(c);
                } else {
                    m["content"] = serde_json::Value::Null;
                }
                if let Some(tcs) = tool_calls {
                    let mut tcs_json = Vec::new();
                    for tc in tcs {
                        tcs_json.push(serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }
                        }));
                    }
                    m["tool_calls"] = serde_json::Value::Array(tcs_json);
                }
                messages.push(m);
            }
            Message::Tool {
                tool_call_id,
                content,
            } => {
                let text = content_to_string(&content);
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": text,
                }));
            }
            Message::ToolEvent(ev) => {
                let text = content_to_string(&ev.content);
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": &ev.tool_call_id,
                    "content": text,
                }));
            }
            _ => {}
        }
    }
    messages
}

#[async_trait]
impl ChatProvider for OpenAIProvider {
    async fn generate(
        &self,
        system_prompt: Option<String>,
        history: Vec<Message>,
        tools: Vec<serde_json::Value>,
    ) -> anyhow::Result<Box<dyn crate::llm::LLMGeneration>> {
        let messages = build_messages(system_prompt, history);

        if !tools.is_empty() {
            return non_streaming(self, messages, tools).await;
        }

        streaming(self, messages).await
    }
}

async fn non_streaming(
    provider: &OpenAIProvider,
    messages: Vec<serde_json::Value>,
    tools: Vec<serde_json::Value>,
) -> anyhow::Result<Box<dyn crate::llm::LLMGeneration>> {
    let mut body = serde_json::json!({
        "model": provider.model,
        "messages": messages,
        "tools": tools,
    });
    if let Some(ref sid) = provider.session_id {
        body["session_id"] = serde_json::Value::String(sid.clone());
    }

    let resp = provider
        .client
        .post(format!("{}/v1/chat/completions", provider.base_url))
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let text = resp.text().await?;
        anyhow::bail!("OpenAI API error: {}", text);
    }

    let data: serde_json::Value = resp.json().await?;
    let choice = &data["choices"][0];
    let message = &choice["message"];

    let mut chunks = Vec::new();
    if let Some(text) = message["content"].as_str()
        && !text.is_empty()
    {
        chunks.push(ContentPart::Text {
            text: text.to_string(),
        });
    }

    let mut tool_calls = Vec::new();
    if let Some(tcs) = message["tool_calls"].as_array() {
        for tc in tcs {
            tool_calls.push(ToolCall {
                id: tc["id"].as_str().unwrap_or("").to_string(),
                kind: tc["type"].as_str().unwrap_or("function").to_string(),
                function: FunctionCall {
                    name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                    arguments: tc["function"]["arguments"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                },
            });
        }
    }

    let usage = data["usage"].as_object().and_then(|u| {
        let prompt = u["prompt_tokens"].as_u64()? as usize;
        let completion = u["completion_tokens"].as_u64()? as usize;
        Some((prompt, completion))
    });

    Ok(Box::new(HttpGeneration::new(chunks, tool_calls, usage)))
}

async fn streaming(
    provider: &OpenAIProvider,
    messages: Vec<serde_json::Value>,
) -> anyhow::Result<Box<dyn crate::llm::LLMGeneration>> {
    let mut body = serde_json::json!({
        "model": provider.model,
        "messages": messages,
        "stream": true,
    });
    if let Some(ref sid) = provider.session_id {
        body["session_id"] = serde_json::Value::String(sid.clone());
    }

    let resp = provider
        .client
        .post(format!("{}/v1/chat/completions", provider.base_url))
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let text = resp.text().await?;
        anyhow::bail!("OpenAI API error: {}", text);
    }

    let (tx, rx) = tokio::sync::mpsc::channel(100);

    tokio::spawn(async move {
        let mut stream = resp.bytes_stream().eventsource();
        while let Some(event) = stream.next().await {
            match event {
                Ok(ev) => {
                    if ev.data == "[DONE]" {
                        break;
                    }
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&ev.data)
                        && let Some(text) = data["choices"][0]["delta"]["content"].as_str()
                        && !text.is_empty()
                    {
                        let _ = tx
                            .send(ContentPart::Text {
                                text: text.to_string(),
                            })
                            .await;
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
    fn test_build_messages_with_system_prompt() {
        let msgs = build_messages(
            Some("You are helpful.".to_string()),
            vec![Message::User(UserMessage::text("hi"))],
        );
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are helpful.");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn test_build_messages_tool_event_conversion() {
        let msgs = build_messages(
            None,
            vec![Message::ToolEvent(ToolEvent {
                tool_call_id: "tc-1".to_string(),
                tool_name: "shell".to_string(),
                status: ToolStatus::Completed,
                content: vec![ContentBlock::Text {
                    text: "output".to_string(),
                }],
                metrics: None,
                elapsed_ms: None,
            })],
        );
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "tc-1");
        assert_eq!(msgs[0]["content"], "output");
    }

    #[test]
    fn test_build_messages_assistant_with_tool_calls() {
        let msgs = build_messages(
            None,
            vec![Message::Assistant {
                content: Some("Let me check.".to_string()),
                tool_calls: Some(vec![ToolCall {
                    id: "tc-1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: r#"{"path": "/tmp"}"#.to_string(),
                    },
                }]),
            }],
        );
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "Let me check.");
        let tcs = msgs[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs[0]["function"]["name"], "read_file");
    }

    #[test]
    fn test_build_messages_no_system_prompt() {
        let msgs = build_messages(None, vec![Message::User(UserMessage::text("hi"))]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn test_build_messages_multimodal_user_content_array() {
        let msgs = build_messages(
            None,
            vec![Message::User(UserMessage::from_parts(vec![
                ContentPart::Text {
                    text: "what?".into(),
                },
                ContentPart::ImageUrl {
                    url: "https://example.com/x.png".into(),
                },
            ]))],
        );
        let content = &msgs[0]["content"];
        let arr = content.as_array().expect("multimodal user content");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
    }
}
