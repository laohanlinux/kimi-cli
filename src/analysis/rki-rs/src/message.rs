//! Message types for LLM conversation and tool results.
//!
//! Supports text, image, tool-call, and native content parts (§6.4).

use serde::{Deserialize, Serialize, Serializer};

/// Status of a tool execution event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Started,
    Completed,
    Failed,
}

/// Rich metadata for a tool execution stored in context (§6.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEvent {
    pub tool_call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub metrics: Option<ToolMetrics>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System {
        content: String,
    },
    User(UserMessage),
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    /// Legacy tool message for LLM-boundary consumption.
    Tool {
        tool_call_id: String,
        content: Vec<ContentBlock>,
    },
    /// Native tool event with rich metadata (§6.4).
    #[serde(rename = "tool_event")]
    ToolEvent(ToolEvent),
    #[serde(rename = "_system_prompt")]
    SystemPrompt {
        content: String,
    },
    #[serde(rename = "_checkpoint")]
    Checkpoint {
        id: u64,
    },
    #[serde(rename = "_usage")]
    Usage {
        token_count: usize,
    },
    /// Compaction boundary marker.
    #[serde(rename = "_compaction")]
    Compaction {
        summary: String,
        preserved_turns: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    Think { text: String },
    ImageUrl { url: String },
    AudioUrl { url: String },
    VideoUrl { url: String },
}

#[derive(Debug, Deserialize)]
struct UserMessageDe {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    parts: Vec<ContentPart>,
}

impl From<UserMessageDe> for UserMessage {
    fn from(d: UserMessageDe) -> Self {
        let mut parts = d.parts;
        if let Some(c) = d.content.filter(|s| !s.is_empty()) {
            parts.insert(0, ContentPart::Text { text: c });
        }
        UserMessage(parts)
    }
}

/// User role content: legacy `content` string or multimodal [`ContentPart`] list (§6.4 / L16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage(Vec<ContentPart>);

impl UserMessage {
    pub fn text(t: impl Into<String>) -> Self {
        Self(vec![ContentPart::Text { text: t.into() }])
    }

    pub fn from_parts(parts: Vec<ContentPart>) -> Self {
        Self(parts)
    }

    pub fn parts(&self) -> &[ContentPart] {
        &self.0
    }

    pub fn into_parts(self) -> Vec<ContentPart> {
        self.0
    }

    /// Merge consecutive user messages (newline between text runs; §1.2 L22).
    pub fn merge_adjacent(&mut self, other: &UserMessage) {
        if other.0.is_empty() {
            return;
        }
        if self.0.is_empty() {
            self.0 = other.0.clone();
            return;
        }
        match (self.0.last_mut(), &other.0[0]) {
            (Some(ContentPart::Text { text: a }), ContentPart::Text { text: b }) => {
                a.push('\n');
                a.push_str(b);
                self.0.extend_from_slice(&other.0[1..]);
            }
            _ => {
                self.0.push(ContentPart::Text {
                    text: "\n".to_string(),
                });
                self.0.extend_from_slice(&other.0);
            }
        }
    }

    pub fn flatten_for_recall(&self) -> String {
        self.0
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } | ContentPart::Think { text } => Some(text.as_str()),
                ContentPart::ImageUrl { url }
                | ContentPart::AudioUrl { url }
                | ContentPart::VideoUrl { url } => Some(url.as_str()),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn approx_chars(&self) -> usize {
        self.flatten_for_recall().len()
    }

    /// SQLite row `content` column: plain string for text-only; JSON for multimodal.
    pub(crate) fn to_persistent_string(&self) -> String {
        if self.0.len() == 1 {
            if let ContentPart::Text { text } = &self.0[0] {
                return text.clone();
            }
        }
        serde_json::json!({ "parts": &self.0 }).to_string()
    }

    pub(crate) fn from_persistent_string(s: &str) -> Result<Self, serde_json::Error> {
        let t = s.trim_start();
        if t.starts_with('{') {
            let v: serde_json::Value = serde_json::from_str(s)?;
            let parts: Vec<ContentPart> = v
                .get("parts")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            Ok(Self(parts))
        } else {
            Ok(Self::text(s.to_string()))
        }
    }
}

impl Serialize for UserMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        if self.0.len() == 1 {
            if let ContentPart::Text { text } = &self.0[0] {
                let mut st = serializer.serialize_struct("UserMessage", 1)?;
                st.serialize_field("content", text)?;
                return st.end();
            }
        }
        let mut st = serializer.serialize_struct("UserMessage", 1)?;
        st.serialize_field("parts", &self.0)?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for UserMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        UserMessageDe::deserialize(deserializer).map(Into::into)
    }
}

/// Structured content block for tool output and context storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Code {
        language: Option<String>,
        code: String,
    },
    Image {
        data: String,
        mime: String,
    },
    Diff {
        before: String,
        after: String,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Traceback {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub path: Option<String>,
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolMetrics {
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
}

impl ToolEvent {
    /// Convert to a legacy `Message::Tool` for LLM-boundary consumption.
    pub fn to_tool_message(&self) -> Message {
        Message::Tool {
            tool_call_id: self.tool_call_id.clone(),
            content: self.content.clone(),
        }
    }
}

/// Convert a list of content blocks to a single string for LLM APIs.
pub fn content_to_string(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(text.clone()),
            ContentBlock::Code { language, code } => {
                let lang = language.as_deref().unwrap_or("");
                parts.push(format!("```{}\n{}\n```", lang, code));
            }
            ContentBlock::Image { data, mime } => {
                parts.push(format!("[Image: {} bytes, {}]", data.len(), mime));
            }
            ContentBlock::Diff { before, after } => {
                parts.push(format!("--- before\n{}\n+++ after\n{}", before, after));
            }
            ContentBlock::Table { headers, rows } => {
                let mut lines = Vec::new();
                lines.push(headers.join(" | "));
                lines.push(
                    headers
                        .iter()
                        .map(|_| "---".to_string())
                        .collect::<Vec<_>>()
                        .join(" | "),
                );
                for row in rows {
                    lines.push(row.join(" | "));
                }
                parts.push(lines.join("\n"));
            }
            ContentBlock::Traceback { text } => {
                parts.push(format!("```traceback\n{}\n```", text));
            }
        }
    }
    parts.join("\n\n")
}

/// Merge consecutive [`Message::User`] entries into one (newline-separated), per §1.2 / §2 history normalization.
pub fn merge_adjacent_user_messages(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        if let Message::User(cur) = m {
            if let Some(Message::User(prev)) = out.last_mut() {
                prev.merge_adjacent(&cur);
                continue;
            }
            out.push(Message::User(cur));
        } else {
            out.push(m);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_to_string_all_variants() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".to_string(),
            },
            ContentBlock::Code {
                language: Some("rust".to_string()),
                code: "let x = 1;".to_string(),
            },
            ContentBlock::Image {
                data: "base64data".to_string(),
                mime: "image/png".to_string(),
            },
            ContentBlock::Diff {
                before: "old".to_string(),
                after: "new".to_string(),
            },
            ContentBlock::Table {
                headers: vec!["a".to_string(), "b".to_string()],
                rows: vec![vec!["1".to_string(), "2".to_string()]],
            },
            ContentBlock::Traceback {
                text: "Error at line 1".to_string(),
            },
        ];
        let s = content_to_string(&blocks);
        assert!(s.contains("hello"));
        assert!(s.contains("```rust"));
        assert!(s.contains("[Image:"));
        assert!(s.contains("--- before"));
        assert!(s.contains("a | b"));
        assert!(s.contains("```traceback"));
    }

    #[test]
    fn test_tool_event_to_tool_message() {
        let ev = ToolEvent {
            tool_call_id: "tc-1".to_string(),
            tool_name: "shell".to_string(),
            status: ToolStatus::Completed,
            content: vec![ContentBlock::Text {
                text: "output".to_string(),
            }],
            metrics: Some(ToolMetrics {
                elapsed_ms: 100,
                exit_code: Some(0),
            }),
            elapsed_ms: Some(100),
        };
        let msg = ev.to_tool_message();
        match msg {
            Message::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "tc-1");
                assert_eq!(content.len(), 1);
            }
            other => panic!("Expected Message::Tool, got {:?}", other),
        }
    }

    #[test]
    fn test_message_serde_roundtrip() {
        let msgs = vec![
            Message::System {
                content: "sys".to_string(),
            },
            Message::User(UserMessage::text("hi")),
            Message::Assistant {
                content: Some("ok".to_string()),
                tool_calls: None,
            },
            Message::Tool {
                tool_call_id: "tc".to_string(),
                content: vec![ContentBlock::Text {
                    text: "t".to_string(),
                }],
            },
            Message::ToolEvent(ToolEvent {
                tool_call_id: "tc2".to_string(),
                tool_name: "read_file".to_string(),
                status: ToolStatus::Failed,
                content: vec![ContentBlock::Text {
                    text: "err".to_string(),
                }],
                metrics: None,
                elapsed_ms: None,
            }),
            Message::SystemPrompt {
                content: "prompt".to_string(),
            },
            Message::Checkpoint { id: 5 },
            Message::Usage { token_count: 42 },
            Message::Compaction {
                summary: "compacted".to_string(),
                preserved_turns: 3,
            },
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            let back: Message = serde_json::from_str(&json).unwrap();
            // Compare debug strings since Message doesn't impl PartialEq
            assert_eq!(
                format!("{:?}", msg),
                format!("{:?}", back),
                "Roundtrip failed for {:?}",
                msg
            );
        }
    }

    #[test]
    fn test_tool_event_json_structure() {
        let ev = ToolEvent {
            tool_call_id: "tc-1".to_string(),
            tool_name: "shell".to_string(),
            status: ToolStatus::Started,
            content: vec![],
            metrics: Some(ToolMetrics {
                elapsed_ms: 50,
                exit_code: None,
            }),
            elapsed_ms: Some(50),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["tool_call_id"], "tc-1");
        assert_eq!(json["tool_name"], "shell");
        assert_eq!(json["status"], "started");
        assert_eq!(json["elapsed_ms"], 50);
    }

    #[test]
    fn test_content_part_serde_roundtrip() {
        let parts = vec![
            ContentPart::Text {
                text: "hello".to_string(),
            },
            ContentPart::Think {
                text: "ponder".to_string(),
            },
            ContentPart::ImageUrl {
                url: "http://x/a.png".to_string(),
            },
        ];
        for part in parts {
            let json = serde_json::to_string(&part).unwrap();
            let back: ContentPart = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", part), format!("{:?}", back));
        }
    }

    #[test]
    fn test_user_message_persistent_roundtrip() {
        let u = UserMessage::text("plain");
        let s = u.to_persistent_string();
        assert_eq!(s, "plain");
        let back = UserMessage::from_persistent_string(&s).unwrap();
        assert_eq!(back, u);

        let u2 = UserMessage::from_parts(vec![
            ContentPart::Text {
                text: "caption".into(),
            },
            ContentPart::ImageUrl {
                url: "https://example.com/i.png".into(),
            },
        ]);
        let sj = u2.to_persistent_string();
        let back2 = UserMessage::from_persistent_string(&sj).unwrap();
        assert_eq!(back2, u2);
    }

    #[test]
    fn test_merge_adjacent_user_messages() {
        let merged = merge_adjacent_user_messages(vec![
            Message::User(UserMessage::text("a")),
            Message::User(UserMessage::text("b")),
            Message::Assistant {
                content: Some("mid".to_string()),
                tool_calls: None,
            },
            Message::User(UserMessage::text("c")),
        ]);
        assert_eq!(merged.len(), 3);
        match &merged[0] {
            Message::User(u) => assert_eq!(
                u.parts(),
                &[ContentPart::Text {
                    text: "a\nb".to_string()
                }]
            ),
            _ => panic!("expected merged user"),
        }
        match &merged[2] {
            Message::User(u) => assert_eq!(
                u.parts(),
                &[ContentPart::Text {
                    text: "c".to_string()
                }]
            ),
            _ => panic!("expected single user"),
        }
    }
}
