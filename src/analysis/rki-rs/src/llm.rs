//! LLM abstraction layer with provider implementations.
//!
//! `ChatProvider` is the core trait. Built-in providers: `EchoProvider`,
//! `OpenAIProvider`, `AnthropicProvider`.

pub mod anthropic;
pub mod openai;

use crate::message::{ContentPart, Message, ToolCall};
use async_trait::async_trait;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn generate(
        &self,
        system_prompt: Option<String>,
        history: Vec<Message>,
        tools: Vec<serde_json::Value>,
    ) -> anyhow::Result<Box<dyn LLMGeneration>>;
}

#[async_trait]
pub trait LLMGeneration: Send {
    async fn next_chunk(&mut self) -> Option<ContentPart>;
    async fn tool_calls(&mut self) -> Vec<ToolCall>;
    async fn usage(&mut self) -> Option<(usize, usize)>;
}

pub struct HttpGeneration {
    chunks: Vec<ContentPart>,
    tool_calls: Vec<ToolCall>,
    usage: Option<(usize, usize)>,
}

impl HttpGeneration {
    pub fn new(
        chunks: Vec<ContentPart>,
        tool_calls: Vec<ToolCall>,
        usage: Option<(usize, usize)>,
    ) -> Self {
        Self {
            chunks,
            tool_calls,
            usage,
        }
    }
}

#[async_trait]
impl LLMGeneration for HttpGeneration {
    async fn next_chunk(&mut self) -> Option<ContentPart> {
        if !self.chunks.is_empty() {
            Some(self.chunks.remove(0))
        } else {
            None
        }
    }

    async fn tool_calls(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.tool_calls)
    }

    async fn usage(&mut self) -> Option<(usize, usize)> {
        self.usage
    }
}

pub struct StreamingGeneration {
    rx: tokio::sync::mpsc::Receiver<ContentPart>,
}

impl StreamingGeneration {
    pub fn new(rx: tokio::sync::mpsc::Receiver<ContentPart>) -> Self {
        Self { rx }
    }
}

#[async_trait]
impl LLMGeneration for StreamingGeneration {
    async fn next_chunk(&mut self) -> Option<ContentPart> {
        self.rx.recv().await
    }

    async fn tool_calls(&mut self) -> Vec<ToolCall> {
        vec![]
    }

    async fn usage(&mut self) -> Option<(usize, usize)> {
        None
    }
}

/// Structured error from an LLM provider, carrying HTTP status code for
/// precise retry classification (§1.2 L23).
#[derive(Debug, Clone)]
pub struct ProviderError {
    pub status_code: u16,
    pub body: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Provider error {}: {}", self.status_code, self.body)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// Whether this error is retryable per Python kimi-cli tenacity set:
    /// 429, 5xx, timeout, connection, empty response.
    pub fn is_retryable(&self) -> bool {
        matches!(self.status_code, 429 | 500..=599)
    }

    /// Whether this error indicates an OAuth token may need refresh.
    pub fn is_unauthorized(&self) -> bool {
        self.status_code == 401
    }
}

/// Retry configuration for LLM calls.
#[allow(dead_code)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 1000,
            max_delay_ms: 30_000,
        }
    }
}

fn is_retryable_error(e: &anyhow::Error) -> bool {
    // Prefer structured provider error for precise status-code matching.
    if let Some(pe) = e.downcast_ref::<ProviderError>() {
        return pe.is_retryable();
    }
    // Fallback for transport-layer or legacy string errors.
    let text = e.to_string().to_ascii_lowercase();
    text.contains("429")
        || text.contains("timeout")
        || text.contains("connection")
        || text.contains("empty response")
}

/// Execute an async operation with exponential backoff retry.
#[allow(dead_code)]
pub async fn with_retry<F, Fut, T>(config: &RetryConfig, operation: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut last_error = None;
    for attempt in 0..=config.max_retries {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                let retryable = is_retryable_error(&e);
                if !retryable || attempt == config.max_retries {
                    return Err(e);
                }
                let delay = std::cmp::min(
                    config.base_delay_ms * 2u64.pow(attempt as u32),
                    config.max_delay_ms,
                );
                tracing::warn!(
                    "LLM call failed (attempt {}/{}): {}. Retrying in {}ms...",
                    attempt + 1,
                    config.max_retries + 1,
                    e,
                    delay
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                last_error = Some(e);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Retry exhausted")))
}

/// Create a provider using the identity layer for credential resolution.
pub async fn create_provider(
    model: &str,
    identity: std::sync::Arc<crate::identity::IdentityManager>,
    session_id: Option<String>,
) -> anyhow::Result<Box<dyn ChatProvider>> {
    if model.starts_with("claude") {
        let (api_key, key_name) =
            resolve_key(identity.clone(), "ANTHROPIC_API_KEY", "KIMI_API_KEY").await?;
        let base_url = resolve_base_url("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
        let mut provider =
            anthropic::AnthropicProvider::new(api_key, base_url, model.to_string());
        provider = provider.with_identity(identity.clone(), key_name);
        if let Some(sid) = session_id {
            provider = provider.with_session_id(sid);
        }
        Ok(Box::new(provider))
    } else {
        let (api_key, key_name) =
            resolve_key(identity.clone(), "OPENAI_API_KEY", "KIMI_API_KEY").await?;
        let base_url = resolve_base_url("OPENAI_BASE_URL", "https://api.openai.com");
        let mut provider = openai::OpenAIProvider::new(api_key, base_url, model.to_string());
        provider = provider.with_identity(identity.clone(), key_name);
        if let Some(sid) = session_id {
            provider = provider.with_session_id(sid);
        }
        Ok(Box::new(provider))
    }
}

async fn resolve_key(
    identity: std::sync::Arc<crate::identity::IdentityManager>,
    primary: &str,
    fallback: &str,
) -> anyhow::Result<(String, String)> {
    if let Ok(Some(cred)) = identity.get_key(primary).await {
        return Ok((cred.value, primary.to_string()));
    }
    if let Ok(Some(cred)) = identity.get_key(fallback).await {
        return Ok((cred.value, fallback.to_string()));
    }
    // Final fallback: direct env var (for backward compat)
    for var in [primary, fallback] {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            return Ok((val, var.to_string()));
        }
    }
    anyhow::bail!(
        "No API key found. Set {} or {} environment variable.",
        primary,
        fallback
    )
}

fn resolve_base_url(env_var: &str, default: &str) -> String {
    std::env::var(env_var)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub struct EchoProvider;

#[async_trait]
impl ChatProvider for EchoProvider {
    async fn generate(
        &self,
        _system_prompt: Option<String>,
        _history: Vec<Message>,
        _tools: Vec<serde_json::Value>,
    ) -> anyhow::Result<Box<dyn LLMGeneration>> {
        Ok(Box::new(HttpGeneration::new(
            vec![ContentPart::Text {
                text: "Hello from echo provider.".to_string(),
            }],
            vec![],
            Some((0, 0)),
        )))
    }
}

/// Deterministic provider that returns scripted responses based on prompt content.
/// Used for testing orchestrators that need specific LLM responses.
#[cfg(test)]
pub struct ScriptedProvider {
    responses: Vec<(String, String)>, // (substring_match, response_text)
    default: String,
}

#[cfg(test)]
impl ScriptedProvider {
    pub fn new(default: impl Into<String>) -> Self {
        Self {
            responses: Vec::new(),
            default: default.into(),
        }
    }

    pub fn with_response(
        mut self,
        match_text: impl Into<String>,
        response: impl Into<String>,
    ) -> Self {
        self.responses.push((match_text.into(), response.into()));
        self
    }
}

#[cfg(test)]
#[async_trait]
impl ChatProvider for ScriptedProvider {
    async fn generate(
        &self,
        system_prompt: Option<String>,
        history: Vec<Message>,
        _tools: Vec<serde_json::Value>,
    ) -> anyhow::Result<Box<dyn LLMGeneration>> {
        let prompt_text = system_prompt.unwrap_or_default();
        let history_text: String = history.iter().map(|m| format!("{:?}", m)).collect();
        let combined = format!("{} {}", prompt_text, history_text);

        let response = self
            .responses
            .iter()
            .find(|(m, _)| combined.contains(m))
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| self.default.clone());

        Ok(Box::new(HttpGeneration::new(
            vec![ContentPart::Text { text: response }],
            vec![],
            Some((0, 0)),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_retry_succeeds_eventually() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_ms: 10,
            max_delay_ms: 100,
        };
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_clone = attempts.clone();
        let result = with_retry(&config, move || {
            let a = attempts_clone.clone();
            async move {
                let count = a.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if count < 3 {
                    anyhow::bail!("429 Too Many Requests")
                }
                Ok("success")
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_gives_up_on_non_retryable() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result = with_retry(&config, || async {
            let r: anyhow::Result<&str> = Err(anyhow::anyhow!("400 Bad Request"));
            r
        })
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_provider_error_retryable_status_codes() {
        assert!(ProviderError { status_code: 429, body: "".into() }.is_retryable());
        assert!(ProviderError { status_code: 500, body: "".into() }.is_retryable());
        assert!(ProviderError { status_code: 502, body: "".into() }.is_retryable());
        assert!(ProviderError { status_code: 503, body: "".into() }.is_retryable());
        assert!(!ProviderError { status_code: 400, body: "".into() }.is_retryable());
        assert!(!ProviderError { status_code: 401, body: "".into() }.is_retryable());
        assert!(!ProviderError { status_code: 403, body: "".into() }.is_retryable());
        assert!(!ProviderError { status_code: 404, body: "".into() }.is_retryable());
    }

    #[test]
    fn test_provider_error_unauthorized() {
        assert!(ProviderError { status_code: 401, body: "".into() }.is_unauthorized());
        assert!(!ProviderError { status_code: 403, body: "".into() }.is_unauthorized());
    }

    #[tokio::test]
    async fn test_retry_with_structured_provider_error() {
        let config = RetryConfig {
            max_retries: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_clone = attempts.clone();
        let result = with_retry(&config, move || {
            let a = attempts_clone.clone();
            async move {
                let count = a.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if count < 2 {
                    Err(ProviderError {
                        status_code: 503,
                        body: "overloaded".into(),
                    }.into())
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_gives_up_on_structured_4xx() {
        let config = RetryConfig {
            max_retries: 2,
            base_delay_ms: 1,
            max_delay_ms: 10,
        };
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_clone = attempts.clone();
        let result: Result<&str, _> = with_retry(&config, move || {
            let a = attempts_clone.clone();
            async move {
                a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(ProviderError {
                    status_code: 400,
                    body: "bad request".into(),
                }.into())
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_http_generation_chunks() {
        let mut g = HttpGeneration::new(
            vec![
                ContentPart::Text {
                    text: "hello".to_string(),
                },
                ContentPart::Text {
                    text: "world".to_string(),
                },
            ],
            vec![],
            Some((10, 20)),
        );
        assert_eq!(
            g.next_chunk().await,
            Some(ContentPart::Text {
                text: "hello".to_string()
            })
        );
        assert_eq!(
            g.next_chunk().await,
            Some(ContentPart::Text {
                text: "world".to_string()
            })
        );
        assert_eq!(g.next_chunk().await, None);
        assert_eq!(g.usage().await, Some((10, 20)));
    }

    #[tokio::test]
    async fn test_echo_provider() {
        let provider = EchoProvider;
        let mut g = provider.generate(None, vec![], vec![]).await.unwrap();
        assert_eq!(
            g.next_chunk().await,
            Some(ContentPart::Text {
                text: "Hello from echo provider.".to_string()
            })
        );
        assert_eq!(g.tool_calls().await, vec![]);
    }

    #[tokio::test]
    async fn test_scripted_provider_matches() {
        let provider = ScriptedProvider::new("default").with_response("magic_word", "matched!");
        let mut g = provider
            .generate(Some("magic_word".to_string()), vec![], vec![])
            .await
            .unwrap();
        assert_eq!(
            g.next_chunk().await,
            Some(ContentPart::Text {
                text: "matched!".to_string()
            })
        );
    }

    #[tokio::test]
    async fn test_scripted_provider_fallback() {
        let provider = ScriptedProvider::new("fallback");
        let mut g = provider
            .generate(Some("other".to_string()), vec![], vec![])
            .await
            .unwrap();
        assert_eq!(
            g.next_chunk().await,
            Some(ContentPart::Text {
                text: "fallback".to_string()
            })
        );
    }

    #[test]
    fn test_resolve_base_url_from_env() {
        unsafe {
            std::env::set_var("TEST_BASE_URL", "https://example.com");
        }
        assert_eq!(
            resolve_base_url("TEST_BASE_URL", "https://default.com"),
            "https://example.com"
        );
    }

    #[test]
    fn test_resolve_base_url_default() {
        assert_eq!(
            resolve_base_url("NONEXISTENT_BASE_URL_XYZ", "https://default.com"),
            "https://default.com"
        );
    }
}
