//! Pluggable text embeddings for semantic memory (§8.5).
//!
//! - [`HashEmbeddingProvider`] — deterministic, L2-normalized (no I/O).
//! - [`HttpEmbeddingProvider`] — optional HTTP (`KIMI_EMBEDDING_URL`); OpenAI-style or `{ "embedding": [...] }` JSON; falls back to hash on errors.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use serde_json::Value;

/// Produces a fixed-size dense vector for a text span.
pub trait EmbeddingProvider: Send + Sync {
    /// Vector dimension (must match every [`Self::embed`] return).
    fn dim(&self) -> usize;

    /// L2-normalized embedding for `text`.
    fn embed(&self, text: &str) -> Vec<f32>;

    /// Embed many spans. Default: one [`Self::embed`] per string. [`HttpEmbeddingProvider`] batches OpenAI `input` arrays.
    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// Deterministic embeddings from `text` (no I/O). Dimension ≥ 4.
pub struct HashEmbeddingProvider {
    dim: usize,
}

impl HashEmbeddingProvider {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(4) }
    }
}

impl EmbeddingProvider for HashEmbeddingProvider {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        let mut s = h.finish();
        for i in 0..self.dim {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u = (s.wrapping_shr((i % 47) as u32) & 0xffff) as f32;
            v[i] = (u / 32768.0) - 1.0;
        }
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n < 1e-6 {
            v[0] = 1.0;
            return v;
        }
        for x in &mut v {
            *x /= n;
        }
        v
    }
}

/// Cosine similarity in **[-1, 1]**; `None` if lengths differ or vectors are degenerate.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < 1e-8 || nb < 1e-8 {
        return None;
    }
    Some((dot / (na * nb)).clamp(-1.0, 1.0))
}

/// OpenAI batch response: `data[].embedding` in the same order as `input`.
pub fn parse_openai_embedding_batch(value: &Value) -> Option<Vec<Vec<f32>>> {
    let data = value.get("data")?.as_array()?;
    let mut out = Vec::with_capacity(data.len());
    for item in data {
        let arr = item.get("embedding")?.as_array()?;
        let v: Vec<f32> = arr
            .iter()
            .filter_map(|x| x.as_f64().map(|d| d as f32))
            .collect();
        if v.is_empty() {
            return None;
        }
        out.push(v);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Parse common embedding JSON shapes: OpenAI `data[0].embedding` or a top-level `embedding` array.
pub fn parse_embedding_json(value: &Value) -> Option<Vec<f32>> {
    if let Some(arr) = value
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|o| o.get("embedding"))
        .and_then(|e| e.as_array())
    {
        let v: Vec<f32> = arr
            .iter()
            .filter_map(|x| x.as_f64().map(|d| d as f32))
            .collect();
        if !v.is_empty() {
            return Some(v);
        }
    }
    let arr = value.get("embedding")?.as_array()?;
    let v: Vec<f32> = arr
        .iter()
        .filter_map(|x| x.as_f64().map(|d| d as f32))
        .collect();
    if v.is_empty() { None } else { Some(v) }
}

pub(crate) fn normalize_to_dim(mut v: Vec<f32>, dim: usize) -> Vec<f32> {
    if dim == 0 {
        return vec![];
    }
    match v.len().cmp(&dim) {
        std::cmp::Ordering::Greater => v.truncate(dim),
        std::cmp::Ordering::Less => {
            v.resize(dim, 0.0);
        }
        std::cmp::Ordering::Equal => {}
    }
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n < 1e-8 {
        let mut z = vec![0.0f32; dim];
        z[0] = 1.0;
        return z;
    }
    for x in &mut v {
        *x /= n;
    }
    v
}

/// HTTP embedding client. On failure or malformed JSON, delegates to [`HashEmbeddingProvider`].
pub struct HttpEmbeddingProvider {
    client: reqwest::blocking::Client,
    url: String,
    api_key: Option<String>,
    model: String,
    /// `openai` sends `{ "model", "input" }`; `rki` sends `{ "text" }`.
    style: String,
    dim: usize,
    hash_fallback: HashEmbeddingProvider,
}

impl HttpEmbeddingProvider {
    /// Reads `KIMI_EMBEDDING_URL` (required), optional `KIMI_EMBEDDING_DIM`, `KIMI_EMBEDDING_API_KEY`,
    /// `KIMI_EMBEDDING_MODEL` (default `text-embedding-3-small`), `KIMI_EMBEDDING_STYLE` (`openai` | `rki`, default `openai`).
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("KIMI_EMBEDDING_URL").ok()?;
        let url = url.trim().to_string();
        if url.is_empty() {
            return None;
        }
        let dim = std::env::var("KIMI_EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1536)
            .max(4);
        let model = std::env::var("KIMI_EMBEDDING_MODEL")
            .unwrap_or_else(|_| "text-embedding-3-small".to_string());
        let style = std::env::var("KIMI_EMBEDDING_STYLE")
            .unwrap_or_else(|_| "openai".to_string())
            .to_lowercase();
        let api_key = std::env::var("KIMI_EMBEDDING_API_KEY").ok();
        Some(Self::new(url, dim, model, style, api_key))
    }

    pub fn new(
        url: impl Into<String>,
        dim: usize,
        model: impl Into<String>,
        style: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        let dim = dim.max(4);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest blocking client");
        Self {
            client,
            url: url.into(),
            api_key,
            model: model.into(),
            style: style.into().to_lowercase(),
            dim,
            hash_fallback: HashEmbeddingProvider::new(dim),
        }
    }

    fn post_request_value(&self, body: Value) -> Option<Value> {
        let mut req = self.client.post(&self.url).json(&body);
        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key.trim()));
        }
        let resp = req.send().ok()?;
        if !resp.status().is_success() {
            tracing::warn!(
                "embedding HTTP {}: {}",
                resp.status(),
                resp.text().unwrap_or_default()
            );
            return None;
        }
        resp.json().ok()
    }

    fn post_json(&self, body: Value) -> Option<Vec<f32>> {
        let v = self.post_request_value(body)?;
        parse_embedding_json(&v)
    }
}

impl EmbeddingProvider for HttpEmbeddingProvider {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let parsed = if self.style == "rki" {
            self.post_json(serde_json::json!({ "text": text }))
        } else {
            self.post_json(serde_json::json!({
                "model": self.model,
                "input": text,
            }))
        };
        match parsed {
            Some(raw) => normalize_to_dim(raw, self.dim),
            None => {
                tracing::debug!("embedding HTTP fallback to hash for this span");
                self.hash_fallback.embed(text)
            }
        }
    }

    fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if texts.is_empty() {
            return vec![];
        }
        if self.style == "openai" {
            if let Some(v) = self.post_request_value(serde_json::json!({
                "model": self.model,
                "input": texts,
            })) {
                if let Some(rows) = parse_openai_embedding_batch(&v) {
                    if rows.len() == texts.len() {
                        return rows
                            .into_iter()
                            .map(|r| normalize_to_dim(r, self.dim))
                            .collect();
                    }
                }
            }
            tracing::debug!("OpenAI embedding batch failed or length mismatch; per-text fallback");
            return texts.iter().map(|t| self.embed(t)).collect();
        }
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_parallel_unit_vectors() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b).unwrap() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = vec![0.0f32, 1.0];
        let b = vec![1.0f32, 0.0];
        assert!(cosine_similarity(&a, &b).unwrap().abs() < 1e-5);
    }

    #[test]
    fn hash_embedding_l2_normalized() {
        let p = HashEmbeddingProvider::new(16);
        let v = p.embed("hello world");
        assert_eq!(v.len(), 16);
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4, "expected unit L2 norm, got {}", n);
    }

    #[test]
    fn parse_openai_embedding_shape() {
        let j = serde_json::json!({
            "data": [{"embedding": [0.0, 3.0, 4.0]}]
        });
        let v = parse_embedding_json(&j).unwrap();
        assert_eq!(v.len(), 3);
        assert!((v[1] - 3.0).abs() < 1e-5);
    }

    #[test]
    fn parse_rki_embedding_shape() {
        let j = serde_json::json!({ "embedding": [1.0, 0.0] });
        let v = parse_embedding_json(&j).unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn parse_openai_batch_two_rows() {
        let j = serde_json::json!({
            "data": [
                {"embedding": [1.0, 0.0, 0.0]},
                {"embedding": [0.0, 1.0, 0.0]}
            ]
        });
        let rows = parse_openai_embedding_batch(&j).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 3);
    }

    #[test]
    fn http_provider_falls_back_on_unreachable() {
        let p = HttpEmbeddingProvider::new("http://127.0.0.1:1/embed", 8, "dummy", "openai", None);
        let a = p.embed("alpha");
        let b = p.embed("alpha");
        assert_eq!(a.len(), 8);
        assert_eq!(a, b, "hash fallback should be deterministic for same text");
    }

    #[test]
    fn normalize_to_dim_truncates_and_unit() {
        let v = super::normalize_to_dim(vec![3.0f32, 4.0, 0.0, 0.0], 2);
        assert_eq!(v.len(), 2);
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4);
    }
}
