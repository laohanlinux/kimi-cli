use crate::llm::{ChatProvider, HttpGeneration, ProviderError, StreamingGeneration};
use crate::message::{
    ContentPart, FunctionCall, Message, ToolCall, UserMessage, content_to_string,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Client;

pub struct OpenAIProvider {
    client: Client,
    api_key: std::sync::Mutex<String>,
    base_url: String,
    model: String,
    /// Session affinity key for prompt caching (Kimi-specific).
    session_id: Option<String>,
    identity: Option<std::sync::Arc<crate::identity::IdentityManager>>,
    key_name: String,
}

impl OpenAIProvider {
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
            .header("Authorization", format!("Bearer {}", key))
            .json(body)
            .send()
            .await?;

        if resp.status().as_u16() == 401 {
            if let Some(ref identity) = self.identity {
                if let Ok(Some(cred)) = identity.get_key(&self.key_name).await {
                    if let Ok(new_cred) = identity.refresh(&cred).await {
                        {
                            let mut api_key = self.api_key.lock().unwrap();
                            *api_key = new_cred.value.clone();
                        }
                        let resp2 = self
                            .client
                            .post(url)
                            .header("Authorization", format!("Bearer {}", new_cred.value))
                            .json(body)
                            .send()
                            .await?;
                        return Ok(resp2);
                    }
                }
            }
        }
        Ok(resp)
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
        .send_with_refresh(
            &format!("{}/v1/chat/completions", provider.base_url),
            &body,
        )
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        return Err(ProviderError { status_code: status, body: text }.into());
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
        .send_with_refresh(
            &format!("{}/v1/chat/completions", provider.base_url),
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
    use crate::identity::CredentialStore;
    use crate::message::{ContentBlock, ToolEvent, ToolStatus};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    #[tokio::test]
    async fn test_openai_provider_401_without_identity_returns_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
        });

        let provider = OpenAIProvider::new(
            "bad_key".to_string(),
            format!("http://127.0.0.1:{}", port),
            "gpt-4".to_string(),
        );
        let result = provider.generate(None, vec![], vec![]).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        let pe = err.downcast_ref::<ProviderError>();
        assert!(pe.is_some(), "Expected ProviderError, got: {}", err);
        assert_eq!(pe.unwrap().status_code, 401);
    }

    #[tokio::test]
    async fn test_openai_provider_401_with_identity_refreshes_and_retries() {
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

            // Second request (after refresh): 200 with valid completion
            let (mut stream2, _) = listener.accept().await.unwrap();
            let mut buf2 = [0u8; 4096];
            let _ = stream2.read(&mut buf2).await;
            let body = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
            let response2 = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}\n\ndata: [DONE]\n\n",
                body
            );
            let _ = stream2.write_all(response2.as_bytes()).await;
        });

        let temp = tempfile::tempdir().unwrap();
        let shared_store = FileCredentialStore::new(temp.path()).unwrap();
        let mut manager = IdentityManager::new(Box::new(FileCredentialStore::new(temp.path()).unwrap()));

        let api_provider = ApiKeyProvider::new("test", Box::new(FileCredentialStore::new(temp.path()).unwrap()), "OPENAI_API_KEY");
        manager.register_provider("test", Box::new(api_provider));

        // Seed the shared store with old and new keys
        let cred = Credential {
            key: "OPENAI_API_KEY".to_string(),
            value: "old_key".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        shared_store.set("OPENAI_API_KEY", &cred).await.unwrap();
        let refreshed = Credential {
            key: "OPENAI_API_KEY".to_string(),
            value: "new_key".to_string(),
            provider: "test".to_string(),
            expires_at: None,
            refresh_token: None,
        };
        shared_store.set("OPENAI_API_KEY", &refreshed).await.unwrap();

        let provider = OpenAIProvider::new(
            "old_key".to_string(),
            format!("http://127.0.0.1:{}", port),
            "gpt-4".to_string(),
        )
        .with_identity(std::sync::Arc::new(manager), "OPENAI_API_KEY".to_string());

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