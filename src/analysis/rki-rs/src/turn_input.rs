//! One user turn as [`ContentPart`] chunks (§1.2 L16 multimodal).
//!
//! ## CLI / stdin JSON (one line)
//! - Plain text → single text part.
//! - JSON object with non-empty **`parts`**: OpenAI-style [`ContentPart`] array.
//! - JSON object with **`text`**: shorthand string turn.
//! - JSON **`{"role":"user",...}`** as in [`crate::message::Message`].
//! - JSON array of **`ContentPart`** (root-level `[{...}]`).
//! - Wrapper **`{"user": ...}`** (one level) for nested tools.

use crate::message::{ContentPart, Message};

/// Single turn from the UI / API before it becomes a [`crate::message::UserMessage`] in context.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub parts: Vec<ContentPart>,
}

impl TurnInput {
    pub fn new(parts: Vec<ContentPart>) -> Self {
        Self { parts }
    }

    pub fn text(s: impl Into<String>) -> Self {
        Self {
            parts: vec![ContentPart::Text { text: s.into() }],
        }
    }

    /// Combined user-visible text (for slash parsing); media turns may be non-empty without text.
    pub fn text_for_slash(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } | ContentPart::Think { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Short summary for wire / logs.
    pub fn text_summary(&self) -> String {
        let mut lines: Vec<&str> = Vec::new();
        let mut media = 0usize;
        for p in &self.parts {
            match p {
                ContentPart::Text { text } | ContentPart::Think { text } => {
                    if !text.is_empty() {
                        lines.push(text);
                    }
                }
                ContentPart::ImageUrl { .. }
                | ContentPart::AudioUrl { .. }
                | ContentPart::VideoUrl { .. } => media += 1,
            }
        }
        let mut s = lines.join("\n");
        if media > 0 {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&format!("[+{media} media]"));
        }
        if s.is_empty() && media > 0 {
            return format!("[{media} media attachment(s)]");
        }
        s
    }

    /// Omit redundant `parts` on the wire when the turn is a single text blob.
    pub fn parts_for_wire(&self) -> Vec<ContentPart> {
        if matches!(self.parts.as_slice(), [ContentPart::Text { .. }]) {
            return vec![];
        }
        self.parts.clone()
    }
}

impl From<&str> for TurnInput {
    fn from(s: &str) -> Self {
        Self::text(s)
    }
}

impl From<&String> for TurnInput {
    fn from(s: &String) -> Self {
        Self::text(s.as_str())
    }
}

impl From<String> for TurnInput {
    fn from(s: String) -> Self {
        Self::text(s)
    }
}

/// Parse one stdin / `--print` line into a [`TurnInput`]: plain text or JSON (see module docs).
pub fn parse_cli_turn_line(line: &str) -> anyhow::Result<TurnInput> {
    let t = line.trim();
    if t.is_empty() {
        anyhow::bail!("empty input");
    }
    if !(t.starts_with('{') || t.starts_with('[')) {
        return Ok(TurnInput::text(t.to_string()));
    }
    let v: serde_json::Value =
        serde_json::from_str(t).map_err(|e| anyhow::anyhow!("invalid JSON turn: {e}"))?;
    parse_turn_from_value(&v)
}

fn parse_turn_from_value(v: &serde_json::Value) -> anyhow::Result<TurnInput> {
    if let Some(arr) = v.as_array() {
        let parts: Vec<ContentPart> = serde_json::from_value(serde_json::Value::Array(arr.clone()))
            .map_err(|e| anyhow::anyhow!("invalid ContentPart array: {e}"))?;
        if parts.is_empty() {
            anyhow::bail!("top-level parts array is empty");
        }
        return Ok(TurnInput::new(parts));
    }
    if let Some(parts_v) = v.get("parts") {
        if let Some(arr) = parts_v.as_array() {
            if !arr.is_empty() {
                let parts: Vec<ContentPart> = serde_json::from_value(parts_v.clone())
                    .map_err(|e| anyhow::anyhow!("invalid `parts` field: {e}"))?;
                return Ok(TurnInput::new(parts));
            }
        }
    }
    if let Some(s) = v.get("text").and_then(|x| x.as_str()) {
        if !s.is_empty() {
            return Ok(TurnInput::text(s.to_string()));
        }
    }
    if let Some(s) = v.get("content").and_then(|x| x.as_str()) {
        if v.get("parts").is_none() && !s.is_empty() {
            return Ok(TurnInput::text(s.to_string()));
        }
    }
    if let Ok(m) = serde_json::from_value::<Message>(v.clone()) {
        if let Message::User(um) = m {
            let parts = um.into_parts();
            if !parts.is_empty() {
                return Ok(TurnInput::new(parts));
            }
        }
    }
    if let Some(inner) = v.get("user") {
        return parse_turn_from_value(inner);
    }
    anyhow::bail!(
        "expected JSON array of content parts, or object with non-empty `parts`, `text`, user message, or `user` wrapper"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cli_plain() {
        let t = parse_cli_turn_line("  hello  ").unwrap();
        assert_eq!(t.parts.len(), 1);
    }

    #[test]
    fn test_parse_cli_json_text() {
        let t = parse_cli_turn_line(r#"{"text":"hi"}"#).unwrap();
        assert_eq!(t.text_for_slash(), "hi");
    }

    #[test]
    fn test_parse_cli_json_parts() {
        let raw = r#"{"parts":[{"type":"text","text":"a"},{"type":"image_url","url":"https://x/p.png"}]}"#;
        let t = parse_cli_turn_line(raw).unwrap();
        assert_eq!(t.parts.len(), 2);
    }

    #[test]
    fn test_parse_cli_json_array() {
        let raw = r#"[{"type":"text","text":"only"}]"#;
        let t = parse_cli_turn_line(raw).unwrap();
        assert_eq!(t.text_for_slash(), "only");
    }

    #[test]
    fn test_parse_cli_user_message() {
        let raw = r#"{"role":"user","content":"legacy"}"#;
        let t = parse_cli_turn_line(raw).unwrap();
        assert_eq!(t.text_for_slash(), "legacy");
    }
}
