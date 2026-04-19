//! Differential context tree with immutable nodes and checkpoint tracking.
//!
//! Supports branching, compaction, and reverting to checkpoints.

use crate::message::Message;
use std::collections::HashMap;

/// Immutable node in the context tree (§5.5 deviation).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ContextNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub messages: Vec<Message>,
    pub token_count: usize,
    pub checkpoint: bool,
    pub compacted: bool,
}

impl ContextNode {
    pub fn new(
        id: impl Into<String>,
        parent_id: Option<String>,
        messages: Vec<Message>,
        token_count: usize,
    ) -> Self {
        Self {
            id: id.into(),
            parent_id,
            messages,
            token_count,
            checkpoint: false,
            compacted: false,
        }
    }

    #[allow(dead_code)]
    pub fn is_checkpoint(&self) -> bool {
        self.checkpoint || self.messages.iter().any(|m| matches!(m, Message::Checkpoint { .. }))
    }
}

/// Persistent context tree with immutable nodes (§5.5 deviation).
/// Compaction creates new successor nodes; originals preserved for undo.
pub struct ContextTree {
    nodes: HashMap<String, ContextNode>,
    head_id: String,
    root_id: String,
    next_checkpoint: u64,
}

impl ContextTree {
    pub fn new() -> Self {
        let root = ContextNode::new("root", None, vec![], 0);
        let root_id = root.id.clone();
        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root);
        Self {
            nodes,
            head_id: root_id.clone(),
            root_id,
            next_checkpoint: 0,
        }
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        let mut tree = Self::new();
        for msg in messages {
            tree.append(msg);
        }
        tree
    }

    /// Append messages to create a new head node. Old node is preserved.
    pub fn append(&mut self, msg: Message) -> String {
        let new_id = format!("node-{}", self.nodes.len());
        let parent = self.nodes.get(&self.head_id).unwrap();
        let mut messages = parent.messages.clone();
        messages.push(msg.clone());
        let token_count = parent.token_count + estimate_tokens(&msg);
        let node = ContextNode {
            id: new_id.clone(),
            parent_id: Some(self.head_id.clone()),
            messages,
            token_count,
            checkpoint: matches!(msg, Message::Checkpoint { .. }),
            compacted: parent.compacted,
        };
        self.head_id = new_id.clone();
        self.nodes.insert(new_id.clone(), node);
        new_id
    }

    /// Append multiple messages in a single node.
    #[allow(dead_code)]
    pub fn extend(&mut self, msgs: Vec<Message>) -> String {
        let new_id = format!("node-{}", self.nodes.len());
        let parent = self.nodes.get(&self.head_id).unwrap();
        let mut messages = parent.messages.clone();
        let mut added_tokens = 0;
        for msg in &msgs {
            messages.push(msg.clone());
            added_tokens += estimate_tokens(msg);
        }
        let node = ContextNode {
            id: new_id.clone(),
            parent_id: Some(self.head_id.clone()),
            messages,
            token_count: parent.token_count + added_tokens,
            checkpoint: msgs.iter().any(|m| matches!(m, Message::Checkpoint { .. })),
            compacted: parent.compacted,
        };
        self.head_id = new_id.clone();
        self.nodes.insert(new_id.clone(), node);
        new_id
    }

    /// Revert head to the node containing the given checkpoint ID.
    /// Returns true if a matching checkpoint was found.
    /// Reverts to the node where the checkpoint was FIRST created
    /// (closest to root), preserving all messages up to that point.
    pub fn revert_to_checkpoint(&mut self, checkpoint_id: u64) -> bool {
        // Walk from root to head to find the first node containing this checkpoint
        let path = self.path_to(&self.head_id);
        for node in path {
            if node.messages.iter().any(|m| {
                matches!(m, Message::Checkpoint { id } if *id == checkpoint_id)
            }) {
                self.head_id = node.id.clone();
                return true;
            }
        }
        false
    }

    /// Create a compacted successor node. Original preserved for undo.
    pub fn compact(&mut self) -> String {
        let head = self.nodes.get(&self.head_id).unwrap();
        if head.messages.len() <= 4 {
            return self.head_id.clone();
        }
        let new_id = format!("node-{}-compact", self.nodes.len());
        let summary = Message::System {
            content: "<system>Earlier messages have been compacted.</system>".to_string(),
        };
        let kept = head.messages.iter().rev().take(4).cloned().collect::<Vec<_>>();
        let mut messages = vec![summary];
        messages.extend(kept.into_iter().rev());
        let node = ContextNode {
            id: new_id.clone(),
            parent_id: Some(self.head_id.clone()),
            messages,
            token_count: head.token_count, // approximate
            checkpoint: false,
            compacted: true,
        };
        self.head_id = new_id.clone();
        self.nodes.insert(new_id.clone(), node);
        new_id
    }

    /// Create a branch from any node for speculative execution (subagents).
    pub fn branch(&self, from_node_id: &str) -> Option<Self> {
        let _from = self.nodes.get(from_node_id)?;
        let mut branch = Self::new();
        // Copy the path from root to from_node
        let path = self.path_to(from_node_id);
        for node in path {
            branch.nodes.insert(node.id.clone(), node.clone());
        }
        branch.head_id = from_node_id.to_string();
        branch.root_id = self.root_id.clone();
        branch.next_checkpoint = self.next_checkpoint;
        Some(branch)
    }

    /// Flatten path from root to head for LLM consumption.
    pub fn linearize(&self) -> Vec<Message> {
        self.nodes.get(&self.head_id)
            .map(|n| n.messages.clone())
            .unwrap_or_default()
    }

    pub fn head_id(&self) -> &str { &self.head_id }
    pub fn head(&self) -> Option<&ContextNode> { self.nodes.get(&self.head_id) }
    pub fn token_count(&self) -> usize {
        self.head().map(|n| n.token_count).unwrap_or(0)
    }
    pub fn next_checkpoint(&mut self) -> u64 {
        let id = self.next_checkpoint;
        self.next_checkpoint += 1;
        id
    }

    pub fn set_next_checkpoint(&mut self, value: u64) {
        self.next_checkpoint = value;
    }

    /// Walk from node back to root, returning the path root→node.
    fn path_to(&self, node_id: &str) -> Vec<&ContextNode> {
        let mut path = Vec::new();
        let mut current = Some(node_id);
        while let Some(id) = current {
            if let Some(node) = self.nodes.get(id) {
                path.push(node);
                current = node.parent_id.as_deref();
            } else {
                break;
            }
        }
        path.reverse();
        path
    }

    /// Find the last checkpoint ID before or at the current head.
    pub fn last_checkpoint(&self) -> Option<u64> {
        let mut current_id = Some(self.head_id.clone());
        while let Some(id) = current_id {
            if let Some(node) = self.nodes.get(&id) {
                for msg in node.messages.iter().rev() {
                    if let Message::Checkpoint { id } = msg {
                        return Some(*id);
                    }
                }
                current_id = node.parent_id.clone();
            } else {
                break;
            }
        }
        None
    }
}

fn estimate_tokens(msg: &Message) -> usize {
    match msg {
        Message::System { content } => content.len() / 4,
        Message::User(u) => u.approx_chars() / 4,
        Message::Assistant { content, tool_calls } => {
            let text_len = content.as_ref().map(|s| s.len()).unwrap_or(0);
            let tc_len = tool_calls.as_ref().map(|tcs| {
                tcs.iter().map(|tc| tc.function.arguments.len()).sum::<usize>()
            }).unwrap_or(0);
            (text_len + tc_len) / 4
        }
        Message::Tool { content, .. } => {
            let text: String = content.iter().map(|b| match b {
                crate::message::ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            }).collect();
            text.len() / 4
        }
        Message::ToolEvent(ev) => {
            let text: String = ev.content.iter().map(|b| match b {
                crate::message::ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            }).collect();
            text.len() / 4
        }
        Message::SystemPrompt { content } => content.len() / 4,
        Message::Checkpoint { .. } => 0,
        Message::Usage { .. } => 0,
        Message::Compaction { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_append_and_linearize() {
        let mut tree = ContextTree::new();
        tree.append(Message::User(crate::message::UserMessage::text("hello")));
        tree.append(Message::Assistant { content: Some("hi".to_string()), tool_calls: None });

        let msgs = tree.linearize();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0], Message::User(_)));
        assert!(matches!(msgs[1], Message::Assistant { .. }));
    }

    #[test]
    fn test_tree_revert() {
        let mut tree = ContextTree::new();
        tree.append(Message::User(crate::message::UserMessage::text("first")));
        let cp_id = tree.next_checkpoint();
        tree.append(Message::Checkpoint { id: cp_id });
        tree.append(Message::User(crate::message::UserMessage::text("second")));
        tree.append(Message::Assistant { content: Some("reply".to_string()), tool_calls: None });

        assert_eq!(tree.linearize().len(), 4);
        assert!(tree.revert_to_checkpoint(cp_id));
        assert_eq!(tree.linearize().len(), 2); // first + checkpoint
    }

    #[test]
    fn test_tree_compact() {
        let mut tree = ContextTree::new();
        for i in 0..10 {
            tree.append(Message::User(crate::message::UserMessage::text(format!("msg{}", i))));
        }
        assert_eq!(tree.linearize().len(), 10);

        tree.compact();
        let msgs = tree.linearize();
        assert!(msgs.len() < 10);
        assert!(msgs.iter().any(|m| matches!(m, Message::System { .. })));
    }

    #[test]
    fn test_tree_branch() {
        let mut tree = ContextTree::new();
        tree.append(Message::User(crate::message::UserMessage::text("shared")));
        let branch_point = tree.head_id().to_string();
        tree.append(Message::User(crate::message::UserMessage::text("parent-only")));

        let branch = tree.branch(&branch_point).unwrap();
        assert_eq!(branch.linearize().len(), 1);
        assert!(matches!(branch.linearize()[0], Message::User(_)));
    }

    #[test]
    fn test_tree_checkpoint_tracking() {
        let mut tree = ContextTree::new();
        let cp1 = tree.next_checkpoint();
        tree.append(Message::Checkpoint { id: cp1 });
        tree.append(Message::User(crate::message::UserMessage::text("after cp1")));
        let cp2 = tree.next_checkpoint();
        tree.append(Message::Checkpoint { id: cp2 });

        assert_eq!(tree.last_checkpoint(), Some(cp2));
        tree.revert_to_checkpoint(cp1);
        assert_eq!(tree.last_checkpoint(), Some(cp1));
    }

    #[test]
    fn test_tree_token_count_accumulates() {
        let mut tree = ContextTree::new();
        tree.append(Message::User(crate::message::UserMessage::text("hello world")));
        tree.append(Message::Assistant { content: Some("response here".to_string()), tool_calls: None });
        let head = tree.nodes.get(&tree.head_id).unwrap();
        // Token count is chars / 4 (conservative heuristic)
        assert!(head.token_count > 0);
    }

    #[test]
    fn test_tree_revert_preserves_checkpoint() {
        let mut tree = ContextTree::new();
        tree.append(Message::User(crate::message::UserMessage::text("before")));
        let cp = tree.next_checkpoint();
        tree.append(Message::Checkpoint { id: cp });
        tree.append(Message::User(crate::message::UserMessage::text("after")));

        tree.revert_to_checkpoint(cp);
        let msgs = tree.linearize();
        assert_eq!(msgs.len(), 2); // before + checkpoint
        assert!(matches!(msgs[1], Message::Checkpoint { id } if id == cp));
    }

    #[test]
    fn test_tree_empty_linearize() {
        let tree = ContextTree::new();
        assert!(tree.linearize().is_empty());
        assert_eq!(tree.last_checkpoint(), None);
    }

    #[test]
    fn test_tree_append_system_prompt() {
        let mut tree = ContextTree::new();
        tree.append(Message::SystemPrompt { content: "sys".to_string() });
        assert_eq!(tree.linearize().len(), 1);
    }
}
