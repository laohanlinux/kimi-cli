//! Conversation context: append-only history with compaction and checkpointing.
//!
//! `Context` persists messages to SQLite and manages token-count estimation,
//! compaction policies, and the differential context tree.

use crate::compaction::{CompactionPolicy, SimpleCompaction};
use crate::context_tree::ContextTree;
use crate::llm::ChatProvider;
use crate::memory::MemoryHierarchy;
use crate::message::Message;
use crate::store::Store;
use std::sync::Arc;

pub struct Context {
    store: Store,
    session_id: String,
    tree: ContextTree,
    memory: MemoryHierarchy,
    compaction_policy: Box<dyn CompactionPolicy>,
}

impl Context {
    pub async fn load(store: &Store, session_id: &str) -> anyhow::Result<Self> {
        let rows = store.get_context(session_id)?;
        let mut messages = Vec::new();
        let mut next_checkpoint = 0u64;
        for row in rows {
            if let Some(msg) = row_to_message(&row) {
                if let Message::Checkpoint { id } = &msg {
                    next_checkpoint = next_checkpoint.max(*id + 1);
                }
                messages.push(msg);
            }
        }
        let mut tree = ContextTree::new();
        let mut memory = MemoryHierarchy::new();
        for msg in messages {
            tree.append(msg.clone());
            memory.push(msg);
        }
        tree.set_next_checkpoint(next_checkpoint);
        Ok(Self {
            store: store.clone(),
            session_id: session_id.to_string(),
            tree,
            memory,
            compaction_policy: Box::new(SimpleCompaction::new(4)),
        })
    }

    pub async fn append(&mut self, msg: Message) -> anyhow::Result<()> {
        let (role, content, metadata, checkpoint_id, token_count) = message_to_row(&msg);
        self.store.append_context(
            &self.session_id,
            &role,
            content.as_deref(),
            metadata.as_deref(),
            checkpoint_id,
            token_count,
        )?;
        self.tree.append(msg.clone());
        self.memory.push(msg);
        Ok(())
    }

    pub fn checkpoint(&mut self) -> u64 {
        self.tree.next_checkpoint()
    }

    pub async fn write_checkpoint(&mut self) -> anyhow::Result<u64> {
        let id = self.checkpoint();
        self.append(Message::Checkpoint { id }).await?;
        Ok(id)
    }

    pub fn history(&self) -> Vec<Message> {
        self.tree.linearize()
            .into_iter()
            .filter_map(|msg| match msg {
                // §6.4: Convert native ToolEvent to LLM-boundary Tool message
                Message::ToolEvent(ev) => Some(ev.to_tool_message()),
                // Skip internal compaction markers from LLM history
                Message::Compaction { .. } => None,
                other => Some(other),
            })
            .collect()
    }

    /// Attach a semantic embedding provider (§8.5). Used when `KIMI_EXPERIMENTAL_SEMANTIC_EMBEDDINGS` is enabled.
    pub fn attach_semantic_embeddings(&mut self, provider: Arc<dyn crate::memory::EmbeddingProvider>) {
        self.memory.attach_semantic_embeddings(provider);
    }

    /// Build LLM history augmented with relevant memory fragments (§8.5 R-style).
    /// Injects episodic/semantic memories as a system reminder before the history.
    pub fn history_with_recall(&self, query: &str, limit: usize) -> Vec<Message> {
        let mut history = self.history();
        let fragments = self.memory.recall(query, limit);
        if !fragments.is_empty() {
            let recall_text = fragments
                .iter()
                .map(|f| {
                    let tier = format!("{:?}", f.source).to_lowercase();
                    format!("[{}] {}", tier, f.content)
                })
                .collect::<Vec<_>>()
                .join("\n");
            history.insert(
                0,
                Message::System {
                    content: format!(
                        "<system>Relevant context from memory:</system>\n{}",
                        recall_text
                    ),
                },
            );
        }
        history
    }

    pub fn token_count(&self) -> usize {
        self.tree.token_count()
    }

    /// Configure compaction policy from runtime config (§4.6 config-driven).
    pub fn set_compaction_config(&mut self, min_messages: usize) {
        self.compaction_policy = Box::new(SimpleCompaction::new(min_messages));
    }

    #[allow(dead_code)]
    pub fn set_token_count(&mut self, count: usize) {
        // Token count is now derived from the tree; this is a no-op
        let _ = count;
    }

    pub async fn revert_to(&mut self, checkpoint_id: u64) -> anyhow::Result<()> {
        self.store.revert_context_to_checkpoint(
            &self.session_id,
            checkpoint_id as i64,
        )?;
        // Rebuild tree and memory from DB
        let rows = self.store.get_context(&self.session_id)?;
        let mut messages = Vec::new();
        for row in rows {
            if let Some(msg) = row_to_message(&row) {
                messages.push(msg);
            }
        }
        self.tree = ContextTree::from_messages(messages.clone());
        self.memory = MemoryHierarchy::new();
        for msg in messages {
            self.memory.push(msg);
        }
        self.compaction_policy = Box::new(SimpleCompaction::new(
            crate::config::Config::default().compaction_min_messages,
        ));
        Ok(())
    }

    pub async fn compact(&mut self, llm: Option<Arc<dyn ChatProvider>>) -> anyhow::Result<()> {
        // Use the compaction policy for LLM-aware summarization (§4.6 integration)
        let history = self.tree.linearize();
        let compacted = self.compaction_policy.compact(&history);
        self.tree = ContextTree::from_messages(compacted);
        // Also compact memory hierarchy (preserve semantic embedding provider across replace).
        let saved_emb = self.memory.take_semantic_embeddings();
        if let Some(llm) = llm {
            self.memory = std::mem::replace(&mut self.memory, MemoryHierarchy::new()).with_llm(llm);
        }
        if let Some(e) = saved_emb {
            self.memory.attach_semantic_embeddings(e);
        }
        self.memory.compact().await;

        // Persist compacted state to DB
        self.store.clear_context(&self.session_id)?;
        for msg in self.tree.linearize() {
            let (role, content, metadata, checkpoint_id, token_count) = message_to_row(&msg);
            self.store.append_context(
                &self.session_id,
                &role,
                content.as_deref(),
                metadata.as_deref(),
                checkpoint_id,
                token_count,
            )?;
        }
        Ok(())
    }

    /// Access the underlying context tree for advanced operations.
    #[allow(dead_code)]
    pub fn tree(&self) -> &ContextTree {
        &self.tree
    }

    #[allow(dead_code)]
    pub fn tree_mut(&mut self) -> &mut ContextTree {
        &mut self.tree
    }
}

fn row_to_message(row: &crate::store::ContextRow) -> Option<Message> {
    match row.role.as_str() {
        "system" => Some(Message::System {
            content: row.content.clone()?,
        }),
        "user" => {
            let raw = row.content.clone()?;
            let um = match crate::message::UserMessage::from_persistent_string(&raw) {
                Ok(u) => u,
                Err(_) => crate::message::UserMessage::text(raw),
            };
            Some(Message::User(um))
        }
        "assistant" => {
            let tool_calls = row.metadata.as_ref().and_then(|m| {
                let v: serde_json::Value = serde_json::from_str(m).ok()?;
                v.get("tool_calls").cloned()
            }).and_then(|tc| serde_json::from_value(tc).ok());
            Some(Message::Assistant {
                content: row.content.clone(),
                tool_calls,
            })
        }
        "tool" => {
            let tool_call_id = row.metadata.as_ref().and_then(|m| {
                let v: serde_json::Value = serde_json::from_str(m).ok()?;
                v.get("tool_call_id")
                    .and_then(|t| t.as_str().map(|s| s.to_string()))
            }).unwrap_or_default();
            let content = row.content.as_ref().and_then(|c| {
                serde_json::from_str::<Vec<crate::message::ContentBlock>>(c).ok()
            }).unwrap_or_else(|| vec![crate::message::ContentBlock::Text { text: row.content.clone().unwrap_or_default() }]);
            Some(Message::Tool {
                tool_call_id,
                content,
            })
        }
        "tool_event" => {
            let ev = row.metadata.as_ref().and_then(|m| {
                serde_json::from_str::<crate::message::ToolEvent>(m).ok()
            })?;
            Some(Message::ToolEvent(ev))
        }
        "_compaction" => {
            let summary = row.content.clone().unwrap_or_default();
            let preserved = row.metadata.as_ref().and_then(|m| {
                let v: serde_json::Value = serde_json::from_str(m).ok()?;
                v.get("preserved_turns").and_then(|t| t.as_u64()).map(|n| n as usize)
            }).unwrap_or(0);
            Some(Message::Compaction { summary, preserved_turns: preserved })
        }
        "_system_prompt" => Some(Message::SystemPrompt {
            content: row.content.clone()?,
        }),
        "_checkpoint" => Some(Message::Checkpoint {
            id: row.checkpoint_id? as u64,
        }),
        "_usage" => Some(Message::Usage {
            token_count: row.token_count? as usize,
        }),
        _ => None,
    }
}


fn message_to_row(msg: &Message) -> (String, Option<String>, Option<String>, Option<i64>, Option<i64>) {
    match msg {
        Message::System { content } => (
            "system".to_string(),
            Some(content.clone()),
            None,
            None,
            None,
        ),
        Message::User(um) => (
            "user".to_string(),
            Some(um.to_persistent_string()),
            None,
            None,
            None,
        ),
        Message::Assistant { content, tool_calls } => {
            let meta = tool_calls.as_ref().map(|tc| {
                serde_json::json!({ "tool_calls": tc }).to_string()
            });
            (
                "assistant".to_string(),
                content.clone(),
                meta,
                None,
                None,
            )
        }
        Message::Tool {
            tool_call_id,
            content,
        } => {
            let meta = Some(
                serde_json::json!({ "tool_call_id": tool_call_id }).to_string(),
            );
            let content_json = serde_json::to_string(content).ok();
            (
                "tool".to_string(),
                content_json,
                meta,
                None,
                None,
            )
        }
        Message::ToolEvent(ev) => {
            let meta = serde_json::to_string(ev).ok();
            let content_json = serde_json::to_string(&ev.content).ok();
            (
                "tool_event".to_string(),
                content_json,
                meta,
                None,
                None,
            )
        }
        Message::Compaction { summary, preserved_turns } => {
            let meta = Some(serde_json::json!({ "preserved_turns": preserved_turns }).to_string());
            (
                "_compaction".to_string(),
                Some(summary.clone()),
                meta,
                None,
                None,
            )
        }
        Message::SystemPrompt { content } => (
            "_system_prompt".to_string(),
            Some(content.clone()),
            None,
            None,
            None,
        ),
        Message::Checkpoint { id } => (
            "_checkpoint".to_string(),
            None,
            None,
            Some(*id as i64),
            None,
        ),
        Message::Usage { token_count } => (
            "_usage".to_string(),
            None,
            None,
            None,
            Some(*token_count as i64),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_history_with_recall_injects_memory() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let session_id = "test-session";
        store.create_session(session_id, "/tmp").unwrap();

        let mut ctx = Context::load(&store, session_id).await.unwrap();

        // Push some messages to build working memory
        for i in 0..50 {
            ctx.append(Message::User(crate::message::UserMessage::text(format!(
                "message about authentication {}",
                i
            ))))
            .await
            .unwrap();
            ctx.append(Message::Assistant { content: Some(format!("reply {}", i)), tool_calls: None }).await.unwrap();
        }

        // Compact to move messages into episodic/semantic memory
        ctx.compact(None).await.unwrap();

        // Now query about authentication - should recall memory fragments
        let history = ctx.history_with_recall("authentication", 3);
        let has_recall = history.iter().any(|m| match m {
            Message::System { content } => content.contains("Relevant context from memory"),
            _ => false,
        });
        assert!(has_recall, "Expected memory recall to be injected into history");
    }

    #[tokio::test]
    async fn test_history_with_recall_empty_when_no_memories() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let session_id = "test-session-2";
        store.create_session(session_id, "/tmp").unwrap();

        // Fresh context with no messages at all
        let ctx = Context::load(&store, session_id).await.unwrap();

        let history = ctx.history_with_recall("nonexistent topic", 3);
        let has_recall = history.iter().any(|m| match m {
            Message::System { content } => content.contains("Relevant context from memory"),
            _ => false,
        });
        assert!(!has_recall, "Expected no memory recall for completely empty memory");
    }

    #[tokio::test]
    async fn test_structured_tool_content_roundtrip() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let session_id = "test-session-3";
        store.create_session(session_id, "/tmp").unwrap();

        let mut ctx = Context::load(&store, session_id).await.unwrap();

        // Append a tool message with structured content blocks
        let structured_content = vec![
            crate::message::ContentBlock::Diff {
                before: "old line".to_string(),
                after: "new line".to_string(),
            },
            crate::message::ContentBlock::Code {
                language: Some("python".to_string()),
                code: "print('hello')".to_string(),
            },
        ];
        ctx.append(Message::Tool {
            tool_call_id: "tc-1".to_string(),
            content: structured_content.clone(),
        }).await.unwrap();

        // Reload and verify structure is preserved
        let ctx2 = Context::load(&store, session_id).await.unwrap();
        let history = ctx2.history();
        let tool_msg = history.iter().find(|m| matches!(m, Message::Tool { .. }));
        assert!(tool_msg.is_some(), "Expected tool message in history");

        if let Message::Tool { content, .. } = tool_msg.unwrap() {
            assert_eq!(content.len(), 2, "Expected 2 content blocks");
            assert!(
                matches!(content[0], crate::message::ContentBlock::Diff { .. }),
                "Expected Diff block"
            );
            assert!(
                matches!(content[1], crate::message::ContentBlock::Code { .. }),
                "Expected Code block"
            );
        }
    }

    #[tokio::test]
    async fn test_tool_event_roundtrip_and_history_conversion() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let session_id = "test-tool-event";
        store.create_session(session_id, "/tmp").unwrap();

        let mut ctx = Context::load(&store, session_id).await.unwrap();

        let ev = crate::message::ToolEvent {
            tool_call_id: "tc-42".to_string(),
            tool_name: "shell".to_string(),
            status: crate::message::ToolStatus::Completed,
            content: vec![
                crate::message::ContentBlock::Text { text: "hello".to_string() },
                crate::message::ContentBlock::Code { language: Some("sh".to_string()), code: "echo hello".to_string() },
            ],
            metrics: Some(crate::message::ToolMetrics { elapsed_ms: 120, exit_code: Some(0) }),
            elapsed_ms: Some(120),
        };
        ctx.append(Message::ToolEvent(ev.clone())).await.unwrap();

        // Raw tree stores the native ToolEvent
        let raw = ctx.tree.linearize();
        assert!(matches!(&raw[0], Message::ToolEvent(_)), "Expected ToolEvent in raw tree");

        // History converts ToolEvent -> Tool for LLM boundary
        let history = ctx.history();
        let tool_msg = history.iter().find(|m| matches!(m, Message::Tool { .. }));
        assert!(tool_msg.is_some(), "Expected Tool message in history after conversion");
        if let Message::Tool { tool_call_id, content } = tool_msg.unwrap() {
            assert_eq!(tool_call_id, "tc-42");
            assert_eq!(content.len(), 2);
        }

        // Reload and verify persistence roundtrip
        let ctx2 = Context::load(&store, session_id).await.unwrap();
        let raw2 = ctx2.tree.linearize();
        assert!(matches!(&raw2[0], Message::ToolEvent(_)), "Expected ToolEvent after reload");
        if let Message::ToolEvent(ev2) = &raw2[0] {
            assert_eq!(ev2.tool_call_id, "tc-42");
            assert_eq!(ev2.tool_name, "shell");
            assert!(matches!(ev2.status, crate::message::ToolStatus::Completed));
            assert_eq!(ev2.elapsed_ms, Some(120));
            assert_eq!(ev2.metrics.as_ref().unwrap().exit_code, Some(0));
        }
    }

    #[tokio::test]
    async fn test_compaction_message_persistence() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let session_id = "test-compaction-msg";
        store.create_session(session_id, "/tmp").unwrap();

        let mut ctx = Context::load(&store, session_id).await.unwrap();
        ctx.append(Message::Compaction { summary: "Summarized 10 messages".to_string(), preserved_turns: 2 }).await.unwrap();

        // Compaction markers are skipped in LLM history
        let history = ctx.history();
        assert!(history.iter().all(|m| !matches!(m, Message::Compaction { .. })),
            "Compaction markers should not appear in LLM history");

        // But preserved in raw storage
        let raw = ctx.tree.linearize();
        assert!(matches!(&raw[0], Message::Compaction { .. }), "Expected Compaction in raw tree");

        // Reload roundtrip
        let ctx2 = Context::load(&store, session_id).await.unwrap();
        let raw2 = ctx2.tree.linearize();
        if let Message::Compaction { summary, preserved_turns } = &raw2[0] {
            assert_eq!(summary, "Summarized 10 messages");
            assert_eq!(*preserved_turns, 2);
        }
    }

    #[tokio::test]
    async fn test_context_compaction_uses_policy() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let session_id = "test-compact";
        store.create_session(session_id, "/tmp").unwrap();

        let mut ctx = Context::load(&store, session_id).await.unwrap();

        // Add enough messages to trigger compaction interest
        for i in 0..10 {
            ctx.append(Message::User(crate::message::UserMessage::text(format!("msg{}", i))))
                .await
                .unwrap();
            ctx.append(Message::Assistant { content: Some(format!("reply{}", i)), tool_calls: None }).await.unwrap();
        }

        let before_len = ctx.history().len();
        assert_eq!(before_len, 20);

        ctx.compact(None).await.unwrap();

        let after_len = ctx.history().len();
        // SimpleCompaction preserves last 4 messages + 1 summary = 5
        assert_eq!(after_len, 5, "Expected compaction to reduce history to 5 messages");

        // First message should be the summary
        assert!(matches!(ctx.history()[0], Message::System { .. }));
    }
}
