//! User turn input validation (§1.2 L16): empty messages and modality hints for text-only models.

use crate::config::Config;
use crate::message::ContentPart;

/// Built-in vision capability catalog.
///
/// Entries are matched as **exact** names or **prefixes** (the model id must start with the
/// pattern followed by `-` or `.` or be identical). First match wins.
///
/// This table is richer than the previous flat negative-only heuristic and gives predictable
/// defaults for common model families without requiring every variant in `vision_by_model`.
static VISION_CATALOG: &[(&str, bool)] = &[
    // Vision-positive families
    ("gpt-4o", true),       // gpt-4o, gpt-4o-mini, gpt-4o-2024-08-06, gpt-4o-latest …
    ("gpt-4-turbo", true),  // gpt-4-turbo, gpt-4-turbo-preview …
    ("claude-3", true),     // claude-3-opus, claude-3-sonnet, claude-3-haiku, claude-3-5-sonnet …
    ("kimi-k2", true),      // kimi-k2 and kimi-k2-* variants
    ("gemini-1.5", true),   // gemini-1.5-pro, gemini-1.5-flash …
    ("gemini-2.0", true),   // gemini-2.0-flash, gemini-2.0-pro …
    // Vision-negative families / specials
    ("echo", false),
    ("mock-llm", false),
    ("gpt-3.5", false),     // gpt-3.5-turbo, gpt-3.5-turbo-16k …
    ("text-davinci", false),
    ("text-embedding", false),
    ("embedding", false),   // broad fallback: anything starting with "embedding-"
    ("rerank", false),
    ("moderation", false),
    ("llama-2", false),     // llama-2 is generally text-only; vision variants use distinct names
];

fn catalog_match(model: &str) -> Option<bool> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return Some(true);
    }
    for (pattern, supports) in VISION_CATALOG {
        let p = pattern.to_ascii_lowercase();
        if m == p || m.starts_with(&(p.clone() + "-")) || m.starts_with(&(p.clone() + ".")) {
            return Some(*supports);
        }
    }
    // Broad fallback for common utility model classes that are never multimodal chat.
    if m.contains("embedding") || m.contains("rerank") || m.contains("moderation") {
        return Some(false);
    }
    None
}

/// Whether the configured **model id** is treated as vision-capable for input validation.
///
/// Local / deterministic providers (`echo`, mocks) are **false** so image-like user text is
/// rejected. API-style model ids default to **true**. Set `Config.ignore_vision_model_hint` or
/// `KIMI_IGNORE_VISION_MODEL_HINT=1` to use only the `supports_vision` flag.
pub fn model_supports_vision_hint(model: &str) -> bool {
    catalog_match(model).unwrap_or(true)
}

/// Lookup `[models.vision_by_model]` merged into [`Config::vision_by_model`].
///
/// Keys are matched in this order:
/// 1. **Exact** case-insensitive match.
/// 2. **Prefix** match for keys ending with `*` (e.g. `gpt-4o*` matches `gpt-4o-2024-08-06`).
pub fn catalog_supports_vision_for_model(config: &Config, model: &str) -> Option<bool> {
    let m = model.trim();
    let m_lower = m.to_ascii_lowercase();

    // 1. Exact match
    if let Some(v) = config
        .vision_by_model
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(m))
        .map(|(_, v)| *v)
    {
        return Some(v);
    }

    // 2. Prefix wildcard (key ends with '*')
    config
        .vision_by_model
        .iter()
        .filter(|(k, _)| k.ends_with('*'))
        .find(|(k, _)| {
            let prefix = &k[..k.len() - 1];
            m_lower.starts_with(&prefix.to_ascii_lowercase())
        })
        .map(|(_, v)| *v)
}

/// Effective vision support for a specific **model id**: `supports_vision`, optional
/// `[models.vision_by_model]` entry for that id, then per-id hint (§1.2 L16).
///
/// Use this when the active provider model may differ from `config.default_model` (e.g. `--model`).
pub fn resolve_supports_vision_for_model(config: &Config, model: &str) -> bool {
    if !config.supports_vision {
        return false;
    }
    if config.ignore_vision_model_hint {
        return true;
    }
    let model = model.trim();
    if let Some(v) = catalog_supports_vision_for_model(config, model) {
        return v;
    }
    model_supports_vision_hint(model)
}

/// Same as [`resolve_supports_vision_for_model`] with `config.default_model`.
pub fn resolve_supports_vision(config: &Config) -> bool {
    resolve_supports_vision_for_model(config, &config.default_model)
}

pub fn looks_like_embedded_image(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("data:image/") {
        return true;
    }
    if lower.contains("<img") {
        return true;
    }
    // Markdown images: ![alt](url)
    if lower.contains("![") && lower.contains("](") {
        return true;
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInputRejection {
    Empty,
    VisionContentNotSupported,
}

impl std::fmt::Display for UserInputRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserInputRejection::Empty => write!(f, "Message is empty."),
            UserInputRejection::VisionContentNotSupported => write!(
                f,
                "This model is configured as text-only (supports_vision=false); input contains non-text media or image-like references."
            ),
        }
    }
}

impl std::error::Error for UserInputRejection {}

fn validate_trimmed_text_and_media(
    trimmed_concat_text: &str,
    has_url_media: bool,
    supports_vision: bool,
) -> Result<(), UserInputRejection> {
    if trimmed_concat_text.is_empty() && !has_url_media {
        return Err(UserInputRejection::Empty);
    }
    if has_url_media && !supports_vision {
        return Err(UserInputRejection::VisionContentNotSupported);
    }
    if !supports_vision && looks_like_embedded_image(trimmed_concat_text) {
        return Err(UserInputRejection::VisionContentNotSupported);
    }
    Ok(())
}

/// Validate a multimodal user turn (`TurnInput.parts` / [`crate::message::UserMessage`]) before append / LLM.
///
/// - **Empty:** no parts, or only whitespace text / think with no image/audio/video URLs.
/// - **Text-only models:** rejects `ImageUrl` / `AudioUrl` / `VideoUrl` parts and markdown/data-URL
///   patterns in combined text (same rules as [`validate_turn_user_input`]).
pub fn validate_turn_content_parts(
    parts: &[ContentPart],
    supports_vision: bool,
) -> Result<(), UserInputRejection> {
    if parts.is_empty() {
        return Err(UserInputRejection::Empty);
    }
    let mut text_like = String::new();
    let mut has_url_media = false;
    for p in parts {
        match p {
            ContentPart::Text { text } | ContentPart::Think { text } => {
                if !text_like.is_empty() {
                    text_like.push('\n');
                }
                text_like.push_str(text);
            }
            ContentPart::ImageUrl { .. }
            | ContentPart::AudioUrl { .. }
            | ContentPart::VideoUrl { .. } => {
                has_url_media = true;
            }
        }
    }
    validate_trimmed_text_and_media(text_like.trim(), has_url_media, supports_vision)
}

/// Validate non-slash user text before it is appended to context / sent to the LLM.
pub fn validate_turn_user_input(
    user_input: &str,
    supports_vision: bool,
) -> Result<(), UserInputRejection> {
    let trimmed = user_input.trim();
    validate_trimmed_text_and_media(trimmed, false, supports_vision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_empty_rejected() {
        assert_eq!(
            validate_turn_user_input("", true),
            Err(UserInputRejection::Empty)
        );
        assert_eq!(
            validate_turn_user_input("   \n\t ", true),
            Err(UserInputRejection::Empty)
        );
    }

    #[test]
    fn test_plain_text_ok() {
        assert!(validate_turn_user_input("hello", true).is_ok());
        assert!(validate_turn_user_input("hello", false).is_ok());
    }

    #[test]
    fn test_markdown_image_rejected_when_text_only() {
        assert!(validate_turn_user_input("see ![x](http://a/b.png)", true).is_ok());
        assert_eq!(
            validate_turn_user_input("see ![x](http://a/b.png)", false),
            Err(UserInputRejection::VisionContentNotSupported)
        );
    }

    #[test]
    fn test_data_url_rejected_when_text_only() {
        let s = "x data:image/png;base64,abc";
        assert_eq!(
            validate_turn_user_input(s, false),
            Err(UserInputRejection::VisionContentNotSupported)
        );
    }

    #[test]
    fn test_model_vision_hint_echo() {
        assert!(!model_supports_vision_hint("echo"));
        assert!(!model_supports_vision_hint("Echo"));
        assert!(model_supports_vision_hint("gpt-4o"));
        assert!(model_supports_vision_hint("kimi-k2"));
    }

    #[test]
    fn test_model_vision_hint_embedding_models_text_only() {
        assert!(!model_supports_vision_hint("text-embedding-3-small"));
        assert!(!model_supports_vision_hint("openai-moderation-latest"));
        assert!(!model_supports_vision_hint("cohere-rerank-v3"));
    }

    #[test]
    fn test_vision_catalog_prefix_matches() {
        // Vision-positive prefixes
        assert!(model_supports_vision_hint("gpt-4o"));
        assert!(model_supports_vision_hint("gpt-4o-mini"));
        assert!(model_supports_vision_hint("gpt-4o-2024-08-06"));
        assert!(model_supports_vision_hint("gpt-4-turbo"));
        assert!(model_supports_vision_hint("gpt-4-turbo-preview"));
        assert!(model_supports_vision_hint("claude-3-opus"));
        assert!(model_supports_vision_hint("claude-3-5-sonnet-20241022"));
        assert!(model_supports_vision_hint("kimi-k2"));
        assert!(model_supports_vision_hint("kimi-k2-vision"));
        assert!(model_supports_vision_hint("gemini-1.5-pro"));
        assert!(model_supports_vision_hint("gemini-2.0-flash"));

        // Vision-negative prefixes
        assert!(!model_supports_vision_hint("gpt-3.5-turbo"));
        assert!(!model_supports_vision_hint("gpt-3.5-turbo-16k"));
        assert!(!model_supports_vision_hint("text-davinci-003"));
        assert!(!model_supports_vision_hint("llama-2-7b"));
        assert!(!model_supports_vision_hint("llama-2-70b-chat"));

        // Unknown / unlisted → defaults to true (API-style safe default)
        assert!(model_supports_vision_hint("some-custom-model"));
        assert!(model_supports_vision_hint("gpt-5"));
    }

    #[test]
    fn test_vision_catalog_by_model_wildcard() {
        let mut c = Config::default();
        c.supports_vision = true;
        c.ignore_vision_model_hint = false;

        // Exact match still works
        c.vision_by_model.insert("custom-v1".to_string(), true);
        assert_eq!(
            catalog_supports_vision_for_model(&c, "custom-v1"),
            Some(true)
        );
        assert_eq!(
            catalog_supports_vision_for_model(&c, "custom-v2"),
            None // no wildcard yet
        );

        // Prefix wildcard
        c.vision_by_model.insert("custom-*".to_string(), false);
        assert_eq!(
            catalog_supports_vision_for_model(&c, "custom-v2"),
            Some(false)
        );
        assert_eq!(
            catalog_supports_vision_for_model(&c, "custom-v2-beta"),
            Some(false)
        );

        // Exact match should still take precedence over wildcard
        assert_eq!(
            catalog_supports_vision_for_model(&c, "custom-v1"),
            Some(true)
        );
    }

    #[test]
    fn test_resolve_supports_vision_and_flag() {
        let mut c = Config::default();
        assert!(
            !resolve_supports_vision(&c),
            "echo + default flag uses model hint off"
        );
        c.supports_vision = false;
        assert!(!resolve_supports_vision(&c));
        c.supports_vision = true;
        c.default_model = "gpt-4o".to_string();
        assert!(resolve_supports_vision(&c));
    }

    #[test]
    fn test_resolve_supports_vision_ignore_model_hint() {
        let mut c = Config::default();
        c.ignore_vision_model_hint = true;
        assert!(
            resolve_supports_vision(&c),
            "echo allowed when hint ignored"
        );
    }

    #[test]
    fn test_vision_catalog_overrides_model_hint() {
        let mut c = Config::default();
        c.supports_vision = true;
        c.ignore_vision_model_hint = false;
        c.default_model = "echo".to_string();
        assert!(
            !resolve_supports_vision(&c),
            "echo uses built-in hint off by default"
        );
        c.vision_by_model.insert("echo".to_string(), true);
        assert!(
            resolve_supports_vision(&c),
            "[models.vision_by_model] should force vision on"
        );
    }

    #[test]
    fn test_resolve_supports_vision_for_model_independent_of_default_model() {
        let mut c = Config::default();
        c.supports_vision = true;
        c.ignore_vision_model_hint = false;
        c.default_model = "echo".to_string();
        assert!(
            !resolve_supports_vision_for_model(&c, "echo"),
            "echo hint off"
        );
        assert!(
            resolve_supports_vision_for_model(&c, "gpt-4o"),
            "gpt-4o uses API-style hint on even when default_model is echo"
        );
    }

    #[test]
    fn test_catalog_supports_vision_case_insensitive() {
        let mut c = Config::default();
        c.vision_by_model.insert("Kimi-K2".to_string(), false);
        assert_eq!(
            catalog_supports_vision_for_model(&c, "kimi-k2"),
            Some(false)
        );
    }

    #[test]
    fn test_validate_turn_content_parts_empty_slice() {
        assert_eq!(
            validate_turn_content_parts(&[], true),
            Err(UserInputRejection::Empty)
        );
    }

    #[test]
    fn test_validate_turn_content_parts_image_url_text_only() {
        let parts = [ContentPart::ImageUrl {
            url: "https://x/a.png".to_string(),
        }];
        assert_eq!(
            validate_turn_content_parts(&parts, false),
            Err(UserInputRejection::VisionContentNotSupported)
        );
        assert!(validate_turn_content_parts(&parts, true).is_ok());
    }

    #[test]
    fn test_validate_turn_content_parts_audio_video_rejected_when_text_only() {
        assert_eq!(
            validate_turn_content_parts(
                &[ContentPart::AudioUrl {
                    url: "https://x/a.mp3".to_string()
                }],
                false
            ),
            Err(UserInputRejection::VisionContentNotSupported)
        );
        assert_eq!(
            validate_turn_content_parts(
                &[ContentPart::VideoUrl {
                    url: "https://x/a.mp4".to_string()
                }],
                false
            ),
            Err(UserInputRejection::VisionContentNotSupported)
        );
    }

    #[test]
    fn test_validate_turn_content_parts_text_only_whitespace_is_empty() {
        assert_eq!(
            validate_turn_content_parts(
                &[
                    ContentPart::Text {
                        text: "  \n".to_string()
                    },
                    ContentPart::Think {
                        text: "\t".to_string()
                    }
                ],
                true
            ),
            Err(UserInputRejection::Empty)
        );
    }

    #[test]
    fn test_validate_turn_content_parts_mixed_text_and_image_ok_when_vision() {
        let parts = [
            ContentPart::Text {
                text: "what is this?".to_string(),
            },
            ContentPart::ImageUrl {
                url: "https://x/b.png".to_string(),
            },
        ];
        assert!(validate_turn_content_parts(&parts, true).is_ok());
    }

    #[test]
    fn test_validate_turn_content_parts_markdown_in_text_when_text_only() {
        let parts = [ContentPart::Text {
            text: "see ![x](http://a/b.png)".to_string(),
        }];
        assert_eq!(
            validate_turn_content_parts(&parts, false),
            Err(UserInputRejection::VisionContentNotSupported)
        );
    }
}
