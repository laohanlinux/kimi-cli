//! Multi-tier conversation memory (working, episodic, semantic).
//!
//! `MemoryHierarchy` manages promotion between tiers as context grows.
//! Semantic tier uses an inverted index with **IDF-weighted** keyword scores, substring boosts,
//! and character **n-gram Jaccard** overlap for typo-tolerant recall (§8.5; embeddings remain future work).
//! Episodic recall uses the same n-gram score on episode summaries.

use crate::llm::ChatProvider;
use crate::message::{ContentPart, Message, UserMessage};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

mod embedding;
pub use embedding::{
    EmbeddingProvider, HashEmbeddingProvider, HttpEmbeddingProvider, cosine_similarity,
    parse_embedding_json, parse_openai_embedding_batch,
};

const RECALL_NGRAM_WEIGHT: f32 = 2.5;

/// Lowercase letters/digits only (shared by episodic + semantic n-gram recall).
fn recall_norm_alnum(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn recall_char_ngrams(norm: &str, n: usize) -> HashSet<String> {
    let ch: Vec<char> = norm.chars().collect();
    if ch.len() < n {
        return HashSet::new();
    }
    let mut out = HashSet::new();
    for w in ch.windows(n) {
        out.insert(w.iter().collect());
    }
    out
}

fn recall_ngram_jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.iter().filter(|t| b.contains(*t)).count();
    let uni = a.len() + b.len() - inter;
    if uni == 0 {
        0.0
    } else {
        inter as f32 / uni as f32
    }
}

/// §8.5: character n-gram Jaccard between `query` and `document` (trigrams if long enough, else bigrams).
/// Lightweight relevance for working-memory fragments in [`MemoryHierarchy::recall`] (§8.5).
fn working_recall_fragment_score(query: &str, text: &str) -> f32 {
    let q = query.trim();
    if q.is_empty() {
        return 0.15;
    }
    let ql = q.to_lowercase();
    let tl = text.to_lowercase();
    let mut score = 0.12f32;
    if tl.contains(&ql) {
        score += 2.5;
    }
    let qt: HashSet<String> = ql
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_string())
        .collect();
    let tt: HashSet<String> = tl
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_string())
        .collect();
    if !qt.is_empty() {
        let inter = qt.iter().filter(|w| tt.contains(*w)).count() as f32;
        score += (inter / qt.len() as f32) * 1.8;
    }
    score += recall_ngram_score(query, text) * 0.35;
    score
}

fn recall_ngram_score(query: &str, document: &str) -> f32 {
    let q_norm = recall_norm_alnum(query);
    let n = if q_norm.chars().count() >= 3 {
        3usize
    } else {
        2usize
    };
    let q_grams = recall_char_ngrams(&q_norm, n);
    if q_grams.is_empty() {
        return 0.0;
    }
    let f_grams = recall_char_ngrams(&recall_norm_alnum(document), n);
    let j = recall_ngram_jaccard(&q_grams, &f_grams);
    if j > 0.0 {
        j * RECALL_NGRAM_WEIGHT
    } else {
        0.0
    }
}

/// A fragment retrieved from any memory tier.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MemoryFragment {
    pub source: MemoryTier,
    pub content: String,
    pub relevance_score: f32,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone)]
pub enum MemoryTier {
    Working,
    Episodic,
    Semantic,
}

/// Working memory: full messages, high fidelity, limited size (~20 turns).
#[derive(Debug, Clone, Default)]
pub struct WorkingMemory {
    messages: Vec<Message>,
    max_turns: usize,
}

impl WorkingMemory {
    pub fn new(max_turns: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_turns,
        }
    }

    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
        // Roughly: each user+assistant pair is a "turn"
        // When we exceed max_turns*2 messages, trigger overflow
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn is_full(&self) -> bool {
        // Count user messages as "turns"
        let turns = self
            .messages
            .iter()
            .filter(|m| matches!(m, Message::User(_)))
            .count();
        turns >= self.max_turns
    }

    /// Extract oldest messages for compaction into episodic memory.
    pub fn extract_oldest(&mut self, keep: usize) -> Vec<Message> {
        if self.messages.len() <= keep {
            return vec![];
        }
        let to_extract = self.messages.len() - keep;
        let extracted: Vec<_> = self.messages.drain(0..to_extract).collect();
        extracted
    }
}

/// Episodic memory: LLM-generated episode summaries (~100 turns).
#[derive(Debug, Clone, Default)]
pub struct EpisodicMemory {
    episodes: Vec<Episode>,
    max_episodes: usize,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Episode {
    pub summary: String,
    pub turns_covered: usize,
    pub timestamp: String,
}

impl EpisodicMemory {
    pub fn new(max_episodes: usize) -> Self {
        Self {
            episodes: Vec::new(),
            max_episodes,
        }
    }

    pub fn add_episode(&mut self, summary: impl Into<String>, turns_covered: usize) {
        self.episodes.push(Episode {
            summary: summary.into(),
            turns_covered,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
        if self.episodes.len() > self.max_episodes {
            self.episodes.remove(0);
        }
    }

    /// Rank episodes by token / substring overlap plus character n-gram Jaccard (§8.5).
    /// Empty `query` returns the most recent episodes (newest first).
    pub fn recall(&self, query: &str, limit: usize) -> Vec<MemoryFragment> {
        let q = query.trim();
        if q.is_empty() {
            return self
                .episodes
                .iter()
                .rev()
                .take(limit)
                .map(|ep| MemoryFragment {
                    source: MemoryTier::Episodic,
                    content: ep.summary.clone(),
                    relevance_score: 0.5,
                    timestamp: Some(ep.timestamp.clone()),
                })
                .collect();
        }

        let q_lower = q.to_lowercase();
        let tokens: Vec<String> = q_lower
            .split_whitespace()
            .filter(|t| t.len() > 1)
            .map(|t| t.to_string())
            .collect();

        let mut ranked: Vec<(usize, f32, &Episode)> = self
            .episodes
            .iter()
            .enumerate()
            .map(|(idx, ep)| {
                let s = ep.summary.to_lowercase();
                let mut score = 0.0f32;
                if s.contains(q_lower.as_str()) {
                    score += 3.0;
                }
                for t in &tokens {
                    if s.contains(t.as_str()) {
                        score += 1.0;
                    }
                }
                score += recall_ngram_score(q, &ep.summary);
                (idx, score, ep)
            })
            .filter(|(_, score, _)| *score > 0.0)
            .collect();

        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.0.cmp(&a.0))
        });

        ranked
            .into_iter()
            .take(limit)
            .map(|(_, score, ep)| MemoryFragment {
                source: MemoryTier::Episodic,
                content: ep.summary.clone(),
                relevance_score: score,
                timestamp: Some(ep.timestamp.clone()),
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.episodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.episodes.is_empty()
    }

    /// Extract oldest episodes for compaction into semantic memory.
    pub fn extract_oldest(&mut self, keep: usize) -> Vec<Episode> {
        if self.episodes.len() <= keep {
            return vec![];
        }
        let to_extract = self.episodes.len() - keep;
        let extracted: Vec<_> = self.episodes.drain(0..to_extract).collect();
        extracted
    }
}

/// Semantic memory: keyword-indexed facts and code references.
/// Uses an inverted index plus lightweight **TF‑IDF-style** weights (no external embedding model).
#[derive(Clone, Default)]
pub struct SemanticMemory {
    fragments: Vec<SemanticFragment>,
    index: HashMap<String, Vec<usize>>, // keyword -> fragment indices
    /// Document frequency: how many fragments contain each keyword (for IDF at recall).
    keyword_df: HashMap<String, usize>,
    /// Optional dense embeddings (§8.5); when set, cosine similarity is added to recall scores.
    embeddings: Option<Arc<dyn EmbeddingProvider>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SemanticFragment {
    pub fact: String,
    pub keywords: Vec<String>,
    pub timestamp: String,
}

#[allow(dead_code)]
impl SemanticMemory {
    pub fn new() -> Self {
        Self {
            fragments: Vec::new(),
            index: HashMap::new(),
            keyword_df: HashMap::new(),
            embeddings: None,
        }
    }

    pub fn attach_embeddings(&mut self, provider: Arc<dyn EmbeddingProvider>) {
        self.embeddings = Some(provider);
    }

    pub fn take_embeddings(&mut self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embeddings.take()
    }

    fn tokenize_fact(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| w.len() >= 3)
            .map(|w| w.to_string())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    fn tokenize_query(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| w.len() >= 3)
            .map(|w| w.to_string())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn index_fact(&mut self, fact: impl Into<String>) {
        let fact_str = fact.into();
        let keywords = Self::tokenize_fact(&fact_str);

        let idx = self.fragments.len();
        self.fragments.push(SemanticFragment {
            fact: fact_str.clone(),
            keywords: keywords.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        });

        for kw in &keywords {
            *self.keyword_df.entry(kw.clone()).or_insert(0) += 1;
            self.index.entry(kw.clone()).or_default().push(idx);
        }
    }

    pub fn recall(&self, query: &str, limit: usize) -> Vec<MemoryFragment> {
        let query_words = Self::tokenize_query(query);
        let n_docs = self.fragments.len().max(1) as f32;

        let mut scores: HashMap<usize, f32> = HashMap::new();
        for word in &query_words {
            if let Some(indices) = self.index.get(word) {
                let df = *self.keyword_df.get(word).unwrap_or(&1) as f32;
                let idf = ((n_docs + 1.0) / (df + 1.0)).ln() + 1.0;
                for &idx in indices {
                    *scores.entry(idx).or_default() += idf;
                }
            }
        }

        let qn = query.trim().to_lowercase();
        if !qn.is_empty() {
            for (i, frag) in self.fragments.iter().enumerate() {
                if frag.fact.to_lowercase().contains(&qn) {
                    *scores.entry(i).or_default() += 3.0;
                }
            }
        }

        for (i, frag) in self.fragments.iter().enumerate() {
            let bump = recall_ngram_score(query, &frag.fact);
            if bump > 0.0 {
                *scores.entry(i).or_default() += bump;
            }
        }

        const EMBED_WEIGHT: f32 = 1.8;
        // Avoid huge remote batches when the semantic tier is large (§8.5 cost gate).
        let embed_max_fragments = std::env::var("KIMI_EMBEDDING_MAX_FRAGMENTS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(64)
            .max(1);
        if let Some(prov) = &self.embeddings {
            if self.fragments.len() <= embed_max_fragments {
                let qv = prov.embed(query);
                if qv.len() == prov.dim() {
                    let facts: Vec<String> =
                        self.fragments.iter().map(|f| f.fact.clone()).collect();
                    let fvs = prov.embed_batch(&facts);
                    for (i, fv) in fvs.into_iter().enumerate() {
                        if fv.len() == qv.len()
                            && let Some(c) = embedding::cosine_similarity(&qv, &fv)
                                && c > 0.0 {
                                    *scores.entry(i).or_default() += c * EMBED_WEIGHT;
                                }
                    }
                }
            } else {
                tracing::debug!(
                    "semantic embedding cosine skipped: {} fragments > {}",
                    self.fragments.len(),
                    embed_max_fragments
                );
            }
        }

        let mut scored: Vec<_> = scores.into_iter().collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        scored
            .into_iter()
            .take(limit)
            .filter_map(|(idx, score)| {
                self.fragments.get(idx).map(|f| MemoryFragment {
                    source: MemoryTier::Semantic,
                    content: f.fact.clone(),
                    relevance_score: score,
                    timestamp: Some(f.timestamp.clone()),
                })
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.fragments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }
}

/// Multi-tier conversation memory (§8.5 deviation).
pub struct MemoryHierarchy {
    pub working: WorkingMemory,
    pub episodic: EpisodicMemory,
    pub semantic: SemanticMemory,
    llm: Option<Arc<dyn ChatProvider>>,
}

impl MemoryHierarchy {
    pub fn new() -> Self {
        Self {
            working: WorkingMemory::new(20),
            episodic: EpisodicMemory::new(100),
            semantic: SemanticMemory::new(),
            llm: None,
        }
    }

    pub fn with_llm(mut self, llm: Arc<dyn ChatProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    pub fn attach_semantic_embeddings(&mut self, provider: Arc<dyn EmbeddingProvider>) {
        self.semantic.attach_embeddings(provider);
    }

    pub fn take_semantic_embeddings(&mut self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.semantic.take_embeddings()
    }

    /// Push a message into working memory. Triggers overflow if full.
    pub fn push(&mut self, msg: Message) {
        self.working.push(msg);
    }

    /// Compact working memory into episodic, episodic into semantic.
    pub async fn compact(&mut self) {
        // Working → Episodic
        if self.working.is_full() {
            let to_compact = self.working.extract_oldest(10); // keep last 10 messages
            if !to_compact.is_empty() {
                let summary = self.summarize_messages(&to_compact).await;
                let turns = to_compact
                    .iter()
                    .filter(|m| matches!(m, Message::User(_)))
                    .count();
                self.episodic.add_episode(summary, turns);
            }
        }

        // Episodic → Semantic
        if self.episodic.len() > 50 {
            let to_extract = self.episodic.extract_oldest(30);
            for ep in to_extract {
                self.semantic.index_fact(ep.summary);
            }
        }
    }

    /// Retrieve relevant fragments from all tiers.
    pub fn recall(&self, query: &str, limit: usize) -> Vec<MemoryFragment> {
        let mut results = Vec::new();

        // Semantic memory: highest relevance
        results.extend(self.semantic.recall(query, limit));

        // Episodic memory
        results.extend(self.episodic.recall(query, limit));

        // Working memory: recent messages scored by substring / token / n-gram overlap (§8.5).
        let working_cap = limit.saturating_mul(8).max(limit).max(24);
        for msg in self.working.messages().iter().rev().take(working_cap) {
            let text = message_to_text(msg);
            if text.is_empty() {
                continue;
            }
            let relevance_score = working_recall_fragment_score(query, &text);
            results.push(MemoryFragment {
                source: MemoryTier::Working,
                content: text,
                relevance_score,
                timestamp: None,
            });
        }

        // Sort by relevance score descending
        results.sort_by(|a, b| b.relevance_score.partial_cmp(&a.relevance_score).unwrap());
        results.truncate(limit);
        results
    }

    async fn summarize_messages(&self, messages: &[Message]) -> String {
        let texts: Vec<String> = messages
            .iter()
            .map(message_to_text)
            .filter(|s| !s.is_empty())
            .collect();
        if texts.is_empty() {
            return "[Empty episode]".to_string();
        }

        // If LLM is available, use it for high-quality summarization
        if let Some(ref llm) = self.llm {
            let prompt = format!(
                "Summarize the following conversation episode in one concise sentence:\n\n{}",
                texts.join("\n")
            );
            let history = vec![Message::User(UserMessage::text(prompt))];
            match llm.generate(None, history, vec![]).await {
                Ok(mut generation) => {
                    let mut parts = Vec::new();
                    while let Some(part) = generation.next_chunk().await {
                        parts.push(part);
                    }
                    let summary: String = parts
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    if !summary.trim().is_empty() {
                        return summary.trim().to_string();
                    }
                }
                Err(e) => {
                    tracing::warn!("LLM summarization failed, falling back to heuristic: {}", e);
                }
            }
        }

        // Fallback heuristic
        if texts.len() <= 3 {
            format!("[Episode: {}]", texts.join("; "))
        } else {
            format!(
                "[Episode with {} messages: {} ... {}]",
                texts.len(),
                texts.first().unwrap(),
                texts.last().unwrap()
            )
        }
    }
}

fn message_to_text(msg: &Message) -> String {
    match msg {
        Message::System { content } => content.clone(),
        Message::User(u) => u.flatten_for_recall(),
        Message::Assistant { content, .. } => content.as_ref().cloned().unwrap_or_default(),
        Message::Tool { content, .. } => content
            .iter()
            .map(|b| match b {
                crate::message::ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        Message::ToolEvent(ev) => ev
            .content
            .iter()
            .map(|b| match b {
                crate::message::ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        Message::SystemPrompt { content } => content.clone(),
        Message::Checkpoint { .. } => String::new(),
        Message::Usage { .. } => String::new(),
        Message::Compaction { .. } => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_working_memory_overflow() {
        let mut wm = WorkingMemory::new(3);
        for i in 0..10 {
            wm.push(Message::User(UserMessage::text(format!("msg{}", i))));
        }
        assert!(wm.is_full());
        let extracted = wm.extract_oldest(4);
        assert_eq!(extracted.len(), 6); // 10 - 4 = 6 extracted
        assert_eq!(wm.len(), 4);
    }

    #[test]
    fn test_episodic_memory_recall() {
        let mut em = EpisodicMemory::new(10);
        em.add_episode("We refactored auth to use OAuth2", 5);
        em.add_episode("Fixed the database migration bug", 3);
        let results = em.recall("auth", 5);
        assert_eq!(results.len(), 1);
        assert!(results[0].content.to_lowercase().contains("auth"));
        assert!(results[0].relevance_score >= 1.0);
    }

    #[test]
    fn test_episodic_recall_ngram_disambiguates_typo() {
        let mut em = EpisodicMemory::new(10);
        em.add_episode("we tuned the frobnitz widget for failover", 2);
        em.add_episode("another line about widgets and dashboards", 2);
        let results = em.recall("frobnits widget", 2);
        assert!(
            results
                .first()
                .is_some_and(|r| r.content.contains("frobnitz")),
            "n-gram score should prefer the frobnitz episode over generic widget text, got {:?}",
            results.first()
        );
    }

    #[test]
    fn test_episodic_memory_recall_empty_query_returns_recent() {
        let mut em = EpisodicMemory::new(10);
        em.add_episode("first episode", 1);
        em.add_episode("second episode", 1);
        let results = em.recall("", 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "second episode");
    }

    #[test]
    fn test_semantic_memory_recall() {
        let mut sm = SemanticMemory::new();
        sm.index_fact("The auth module uses OAuth2 with PKCE extension");
        sm.index_fact("Database migrations are handled by sqlx");
        sm.index_fact("The API supports rate limiting per user");

        // Query with keywords that exist in the indexed facts
        let results = sm.recall("auth module", 5);
        assert!(!results.is_empty());
        assert!(results[0].content.contains("auth"));
    }

    #[tokio::test]
    async fn test_memory_hierarchy_compact() {
        let mut mh = MemoryHierarchy::new();
        // Push enough messages to trigger working memory overflow
        for i in 0..50 {
            mh.push(Message::User(UserMessage::text(format!("message {}", i))));
            mh.push(Message::Assistant {
                content: Some(format!("reply {}", i)),
                tool_calls: None,
            });
        }

        assert!(!mh.working.is_empty());
        mh.compact().await;

        // After compact, episodic memory should have episodes
        assert!(!mh.episodic.is_empty() || mh.working.len() <= 20);
    }

    #[test]
    fn test_recall_from_all_tiers() {
        let mut mh = MemoryHierarchy::new();
        mh.semantic.index_fact("The API uses REST conventions");
        mh.episodic.add_episode("We discussed REST API design", 3);
        mh.push(Message::User(UserMessage::text("What about GraphQL?")));

        let results = mh.recall("API design", 5);
        assert!(!results.is_empty());
        // Should have at least semantic and episodic results
        let has_semantic = results
            .iter()
            .any(|r| matches!(r.source, MemoryTier::Semantic));
        let has_episodic = results
            .iter()
            .any(|r| matches!(r.source, MemoryTier::Episodic));
        assert!(has_semantic || has_episodic);
    }

    #[test]
    fn test_memory_hierarchy_recall_ranks_working_by_query() {
        let mut mh = MemoryHierarchy::new();
        mh.push(Message::User(UserMessage::text("lunch menu tacos")));
        mh.push(Message::Assistant {
            content: Some("OAuth refresh token rotation for the API gateway".to_string()),
            tool_calls: None,
        });
        mh.push(Message::User(UserMessage::text("thanks")));

        let results = mh.recall("OAuth token", 5);
        let best_working = results
            .iter()
            .filter(|r| matches!(r.source, MemoryTier::Working))
            .max_by(|a, b| {
                a.relevance_score
                    .partial_cmp(&b.relevance_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        assert!(
            best_working.is_some_and(|w| w.content.contains("OAuth")),
            "working tier should score OAuth-related text highest among working fragments, got {:?}",
            best_working
        );
    }

    #[test]
    fn test_working_memory_is_full() {
        let mut wm = WorkingMemory::new(3);
        assert!(!wm.is_full());
        wm.push(Message::User(UserMessage::text("1")));
        wm.push(Message::Assistant {
            content: Some("a".to_string()),
            tool_calls: None,
        });
        wm.push(Message::User(UserMessage::text("2")));
        wm.push(Message::Assistant {
            content: Some("b".to_string()),
            tool_calls: None,
        });
        wm.push(Message::User(UserMessage::text("3")));
        assert!(wm.is_full());
    }

    #[test]
    fn test_working_memory_extract_oldest() {
        let mut wm = WorkingMemory::new(10);
        for i in 0..5 {
            wm.push(Message::User(UserMessage::text(format!("{}", i))));
        }
        let extracted = wm.extract_oldest(2);
        assert_eq!(extracted.len(), 3);
        assert_eq!(wm.len(), 2);
    }

    #[test]
    fn test_working_memory_extract_oldest_noop_when_small() {
        let mut wm = WorkingMemory::new(10);
        wm.push(Message::User(UserMessage::text("1")));
        let extracted = wm.extract_oldest(5);
        assert!(extracted.is_empty());
        assert_eq!(wm.len(), 1);
    }

    #[test]
    fn test_episodic_memory_add_and_recall() {
        let mut em = EpisodicMemory::new(10);
        em.add_episode("We refactored auth", 5);
        em.add_episode("We fixed a bug", 3);
        let results = em.recall("auth", 5);
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("auth"));
    }

    #[test]
    fn test_semantic_memory_empty_recall() {
        let sm = SemanticMemory::new();
        let results = sm.recall("anything", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_semantic_recall_idf_prefers_rarer_keyword() {
        let mut sm = SemanticMemory::new();
        for i in 0..8 {
            sm.index_fact(format!(
                "the india region deployment notes standard chunk {}",
                i
            ));
        }
        sm.index_fact("the india region deployment notes quixotic zephyr override");

        let results = sm.recall("india quixotic", 3);
        assert!(!results.is_empty());
        assert!(
            results[0].content.contains("quixotic"),
            "expected IDF to rank the rare-term fact first, got {:?}",
            results[0].content
        );
    }

    #[test]
    fn test_semantic_substring_query_boost() {
        let mut sm = SemanticMemory::new();
        sm.index_fact("unrelated oauth client credentials flow");
        sm.index_fact("totally different topic about databases");

        let results = sm.recall("oauth client credentials", 2);
        assert!(
            results.first().is_some_and(|r| r.content.contains("oauth")),
            "substring boost should rank the oauth fact first, got {:?}",
            results
        );
    }

    #[test]
    fn test_semantic_recall_calls_embed_batch_for_fragments() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingEmb {
            batch_calls: AtomicUsize,
            embed_calls: AtomicUsize,
        }
        impl embedding::EmbeddingProvider for CountingEmb {
            fn dim(&self) -> usize {
                2
            }
            fn embed(&self, _text: &str) -> Vec<f32> {
                self.embed_calls.fetch_add(1, Ordering::SeqCst);
                vec![1.0f32, 0.0]
            }
            fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
                self.batch_calls.fetch_add(1, Ordering::SeqCst);
                texts.iter().map(|_| vec![1.0f32, 0.0]).collect()
            }
        }

        let emb = Arc::new(CountingEmb {
            batch_calls: AtomicUsize::new(0),
            embed_calls: AtomicUsize::new(0),
        });
        let mut sm = SemanticMemory::new();
        sm.attach_embeddings(emb.clone());
        sm.index_fact("alpha fact one");
        sm.index_fact("beta fact two");
        let _ = sm.recall("alpha", 2);
        assert_eq!(emb.batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(emb.embed_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_semantic_embedding_cosine_ranks_with_provider() {
        struct CornerEmb;
        impl embedding::EmbeddingProvider for CornerEmb {
            fn dim(&self) -> usize {
                2
            }
            fn embed(&self, text: &str) -> Vec<f32> {
                let mut v = if text.contains("ALPHA_MARKER") {
                    vec![1.0f32, 0.0]
                } else if text.contains("BETA_MARKER") {
                    vec![0.0f32, 1.0]
                } else {
                    vec![0.5f32, 0.5f32]
                };
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                for x in &mut v {
                    *x /= n;
                }
                v
            }
        }

        let mut sm = SemanticMemory::new();
        sm.attach_embeddings(Arc::new(CornerEmb));
        sm.index_fact("ALPHA_MARKER project scope");
        sm.index_fact("BETA_MARKER project scope");
        let results = sm.recall("ALPHA_MARKER project", 2);
        assert!(
            results
                .first()
                .is_some_and(|r| r.content.contains("ALPHA_MARKER")),
            "embedding cosine bonus should rank ALPHA fact first, got {:?}",
            results.first()
        );
    }

    #[test]
    fn test_semantic_trigram_typo_resilience() {
        let mut sm = SemanticMemory::new();
        sm.index_fact("configuration service mesh linking pods");
        sm.index_fact("logging daemon volume mount paths");
        let results = sm.recall("configuraton service", 2);
        assert!(
            results
                .first()
                .is_some_and(|r| r.content.contains("configuration")),
            "n-gram overlap should prefer the near-matched configuration fact, got {:?}",
            results.first()
        );
    }

    #[tokio::test]
    async fn test_memory_hierarchy_compact_with_llm() {
        let llm = Arc::new(crate::llm::EchoProvider);
        let mut mh = MemoryHierarchy::new().with_llm(llm);
        // Push enough messages to trigger working memory overflow
        for i in 0..50 {
            mh.push(Message::User(UserMessage::text(format!("message {}", i))));
            mh.push(Message::Assistant {
                content: Some(format!("reply {}", i)),
                tool_calls: None,
            });
        }

        mh.compact().await;

        // EchoProvider returns "Hello from echo provider." as summary
        assert!(!mh.episodic.is_empty() || mh.working.len() <= 20);
        if !mh.episodic.is_empty() {
            let ep = mh.episodic.recall("", 1);
            assert_eq!(ep[0].content, "Hello from echo provider.");
        }
    }

    #[test]
    fn test_memory_hierarchy_empty_recall() {
        let mh = MemoryHierarchy::new();
        let results = mh.recall("something", 5);
        assert!(results.is_empty());
    }
}
