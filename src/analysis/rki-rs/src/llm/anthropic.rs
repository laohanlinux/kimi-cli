use crate::llm::{ChatProvider, HttpGeneration, ProviderError, StreamingGeneration};
use crate::message::{
    ContentPart, FunctionCall, Message, ToolCall, UserMessage, content_to_string,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;

pub struct AnthropicProvider {
    client: Client,
    api_key: std::sync::Mutex<String>,
    base_url: String,
    model: String,
    /// Session affinity for Anthropic (metadata.user_id).
    session_id: Option<String>,
    identity: Option<std::sync::Arc<crate::identity::IdentityManager>>,
    key_name: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            client: Client::new(),
            api_key: std::sync::Mutex::new(api_key),
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            session_id: None,
            identity: None,
            key_name: String::new(),
        }
    }

    pub fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_identity(
        mut self,
        identity: std::sync::Arc<crate::identity::IdentityManager>,
        key_name: String,
    ) -> Self {
        self.identity = Some(identity);
        self.key_name = key_name;
        self
    }

    /// Send a request, refreshing the token once on 401 if identity is configured.
    async fn send_with_refresh(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<reqwest::Response> {
        let key = self.api_key.lock().unwrap().clone();
        let resp = self
            .client
            .post(url)
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await?;

        if resp.status().as_u16() == 401
            && let Some(ref identity) = self.identity
                && let Ok(Some(cred)) = identity.get_key(&self.key_name).await
                    && let Ok(new_cred) = identity.refresh(&cred).await {
                        {
                            let mut api_key = self.api_key.lock().unwrap();
                            *api_key = new_cred.value.clone();
                        }
                        let resp2 = self
                            .client
                            .post(url)
                            .header("x-api-key", &new_cred.value)
                            .header("anthropic-version", "2023-06-01")
                            .json(body)
                            .send()
                            .await?;
                        return Ok(resp2);
                    }
        Ok(resp)
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
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                if let Some(c) = content {
                    blocks.push(serde_json::json!({"type": "text", "text": c}));
                }
                if let Some(tcs) = tool_calls {
                    for tc in tcs {
                        let input: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::json!({}));
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
            Message::Tool {
                tool_call_id,
                content,
            } => {
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
        .send_with_refresh(
            &format!("{}/v1/messages", provider.base_url),
            &body,
        )
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        return Err(ProviderError { status_code: status, body: text }.into());
    }

    let data: serde_json::Value = resp.json().await?;

    let mut chunks = Vec::new();
    let mut tool_calls = Vec::new();
    if let Some(content) = data["content"].as_array() {
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(text) = block["text"].as_str() {
                        chunks.push(ContentPart::Text {
                            text: text.to_string(),
                        });
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
        .send_with_refresh(
            &format!("{}/v1/messages", provider.base_url),
            &body,
        )
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        return Err(ProviderError { status_code: status, body: text }.into());
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
    use crate::identity::CredentialStore;
    use crate::message::{ContentBlock, ToolEvent, ToolStatus};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_build_messages_user_and_assistant() {
        let msgs = build_messages(vec![
            Message::User(UserMessage::text("hello")),
            Message::Assistant {
                content: Some("hi".to_string()),
                tool_calls: None,
            },
        ]);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["text"], "hello");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_build_messages_skips_inline_system() {
        let msgs = build_messages(vec![
            Message::System {
                content: "sys".to_string(),
            },
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
            content: vec![ContentBlock::Text {
                text: "out".to_string(),
            }],
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

    #[tokio::test]
    async fn test_anthropic_provider_401_without_identity_returns_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
        });

        let provider = AnthropicProvider::new(
            "bad_key".to_string(),
            format!("http://127.0.0.1:{}", port),
            "claude-3".to_string(),
        );
        let result = provider.generate(None, vec![], vec![]).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        let pe = err.downcast_ref::<crate::llm::ProviderError>();
        assert!(pe.is_some(), "Expected ProviderError, got: {}", err);
        assert_eq!(pe.unwrap().status_code, 401);
    }

    #[tokio::test]
    async fn test_anthropic_provider_401_with_identity_refreshes_and_retries() {
        use crate::identity::{ApiKeyProvider, Credential, FileCredentialStore, IdentityManager};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            // First request: 401
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;

            // Second request (after refresh): 200 with SSE streaming
            let (mut stream2, _) = listener.accept().await.unwrap();
            let mut buf2 = [0u8; 4096];
            let _ = stream2.read(&mut buf2).await;
            let sse = "event: message_start\r\ndata: {\"type\":\"message_start\"}\r\n\r\nevent: content_block_delta\r\ndata: {\"delta\":{\"text\":\"hello\"}}\r\n\r\nevent: message_stop\r\ndata: {}\r\n\r\n";
            let response2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{}",
                sse
            );
            let _ = stream2.write_all(response2.as_bytes()).await;
        });

        let temp = tempfile::tempdir().unwrap();
        let shared_store = FileCredentialStore::new(temp.path()).unwrap();
        let mut manager = IdentityManager::new(Box::new(FileCredentialStore::new(temp.path()).unwrap()));

        let api_provider = ApiKeyProvider::new("test", Box::new(FileCredentialStore::new(temp.path()).unwrap()), "ANTHROPIC_API_KEY");
        manager.register_provider("test", Box::new(api_provider));

        let cred = Credential {
            key: "ANTHROPIC_API_KEY".to_string(),
            value: "old_key".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        shared_store.set("ANTHROPIC_API_KEY", &cred).await.unwrap();
        let refreshed = Credential {
            key: "ANTHROPIC_API_KEY".to_string(),
            value: "new_key".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        shared_store.set("ANTHROPIC_API_KEY", &refreshed).await.unwrap();

        let provider = AnthropicProvider::new(
            "old_key".to_string(),
            format!("http://127.0.0.1:{}", port),
            "claude-3".to_string(),
        )
        .with_identity(std::sync::Arc::new(manager), "ANTHROPIC_API_KEY".to_string());

        let result = provider.generate(None, vec![], vec![]).await;
        assert!(result.is_ok(), "Expected success after refresh+retry");
        let mut generation = result.ok().unwrap();
        let chunk = generation.next_chunk().await;
        assert_eq!(
            chunk,
            Some(ContentPart::Text {
                text: "hello".to_string()
            })
        );
    }
}
