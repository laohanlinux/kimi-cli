//! Capability-based authorization engine.
//!
//! Maps tool names to capability strings and enforces trust-profile rules.

use serde::Deserialize;
use serde_json::Value;

/// Map a tool name to its required capability.
pub fn tool_to_capability(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "shell" => Some("process:exec"),
        "write_file" | "str_replace_file" => Some("filesystem:write"),
        "read_file" | "glob" | "grep" => Some("filesystem:read"),
        "task_stop" => Some("process:kill"),
        "agent" => Some("agent:spawn"),
        _ => None,
    }
}

/// Extract constraint context from tool arguments for capability matching.
pub fn extract_constraints(_tool_name: &str, args: &Value) -> ConstraintContext {
    let path = args.get("path").and_then(|v| v.as_str()).map(|s| {
        if s.starts_with("~/") {
            if let Some(home) = dirs::home_dir() {
                format!("{}{}", home.to_string_lossy(), &s[1..])
            } else {
                s.to_string()
            }
        } else {
            s.to_string()
        }
    });
    let command = args.get("command").and_then(|v| v.as_str()).map(|s| s.to_string());
    let host = args.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
    ConstraintContext { path, command, host }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct TrustProfile {
    pub default: String,
    pub overrides: Vec<CapabilityOverride>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
pub struct CapabilityOverride {
    pub capability: String,
    pub path: Option<String>,
    pub command_pattern: Option<String>,
    pub host_pattern: Option<String>,
    pub decision: String,
}

#[derive(Debug, Clone)]
pub struct ConstraintContext {
    pub path: Option<String>,
    pub command: Option<String>,
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Auto,
    Prompt,
    Block,
}

impl Decision {
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "auto" => Decision::Auto,
            "block" => Decision::Block,
            _ => Decision::Prompt,
        }
    }
}

pub struct CapabilityEngine {
    profile: TrustProfile,
}

impl CapabilityEngine {
    pub fn new(profile: TrustProfile) -> Self {
        Self { profile }
    }

    pub fn check(&self, capability: &str, constraints: &ConstraintContext) -> Decision {
        for ov in &self.profile.overrides {
            if ov.capability == capability {
                let matched = if let Some(pattern) = &ov.path {
                    constraints
                        .path
                        .as_ref()
                        .is_some_and(|p| glob_match(pattern, p))
                } else if let Some(pattern) = &ov.command_pattern {
                    constraints
                        .command
                        .as_ref()
                        .is_some_and(|c| regex_match(pattern, c))
                } else if let Some(pattern) = &ov.host_pattern {
                    constraints
                        .host
                        .as_ref()
                        .is_some_and(|h| regex_match(pattern, h))
                } else {
                    true
                };
                if matched {
                    return Decision::parse(&ov.decision);
                }
            }
        }
        Decision::parse(&self.profile.default)
    }
}

fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern = if pattern.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            format!("{}{}", home.to_string_lossy(), &pattern[1..])
        } else {
            pattern.to_string()
        }
    } else {
        pattern.to_string()
    };
    let regex = regex::escape(&pattern).replace(r"\*", ".*").replace(r"\?", ".");
    if let Ok(re) = regex::Regex::new(&format!("^{}$", regex)) {
        re.is_match(value)
    } else {
        false
    }
}

fn regex_match(pattern: &str, value: &str) -> bool {
    if let Ok(re) = regex::Regex::new(pattern) {
        re.is_match(value)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capability_engine() {
        let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
        let profile = TrustProfile {
            default: "prompt".to_string(),
            overrides: vec![
                CapabilityOverride {
                    capability: "filesystem:write".to_string(),
                    path: Some("~/Projects/**".to_string()),
                    command_pattern: None,
                    host_pattern: None,
                    decision: "auto".to_string(),
                },
                CapabilityOverride {
                    capability: "process:exec".to_string(),
                    path: None,
                    command_pattern: Some(r"^git ".to_string()),
                    host_pattern: None,
                    decision: "auto".to_string(),
                },
            ],
        };
        let engine = CapabilityEngine::new(profile);
        assert_eq!(
            engine.check(
                "filesystem:write",
                &ConstraintContext {
                    path: Some(format!("{}/Projects/foo", home)),
                    command: None,
                    host: None,
                }
            ),
            Decision::Auto
        );
        assert_eq!(
            engine.check(
                "filesystem:write",
                &ConstraintContext {
                    path: Some("/etc/passwd".to_string()),
                    command: None,
                    host: None,
                }
            ),
            Decision::Prompt
        );
        assert_eq!(
            engine.check(
                "process:exec",
                &ConstraintContext {
                    path: None,
                    command: Some("git status".to_string()),
                    host: None,
                }
            ),
            Decision::Auto
        );
        assert_eq!(
            engine.check(
                "process:exec",
                &ConstraintContext {
                    path: None,
                    command: Some("rm -rf /".to_string()),
                    host: None,
                }
            ),
            Decision::Prompt
        );
    }

    #[test]
    fn test_tool_to_capability() {
        assert_eq!(tool_to_capability("shell"), Some("process:exec"));
        assert_eq!(tool_to_capability("write_file"), Some("filesystem:write"));
        assert_eq!(tool_to_capability("str_replace_file"), Some("filesystem:write"));
        assert_eq!(tool_to_capability("read_file"), Some("filesystem:read"));
        assert_eq!(tool_to_capability("glob"), Some("filesystem:read"));
        assert_eq!(tool_to_capability("grep"), Some("filesystem:read"));
        assert_eq!(tool_to_capability("task_stop"), Some("process:kill"));
        assert_eq!(tool_to_capability("agent"), Some("agent:spawn"));
        assert_eq!(tool_to_capability("unknown"), None);
    }

    #[test]
    fn test_extract_constraints() {
        let args = serde_json::json!({"path": "/tmp/test.txt", "command": "ls -la"});
        let ctx = extract_constraints("shell", &args);
        assert_eq!(ctx.path, Some("/tmp/test.txt".to_string()));
        assert_eq!(ctx.command, Some("ls -la".to_string()));
        assert_eq!(ctx.host, None);

        let args = serde_json::json!({"url": "https://example.com"});
        let ctx = extract_constraints("fetch_url", &args);
        assert_eq!(ctx.host, Some("https://example.com".to_string()));
    }

    #[test]
    fn test_extract_constraints_tilde_expansion() {
        let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
        let args = serde_json::json!({"path": "~/Documents"});
        let ctx = extract_constraints("read_file", &args);
        assert_eq!(ctx.path, Some(format!("{}/Documents", home)));
    }

    #[test]
    fn test_decision_from_str() {
        assert_eq!(Decision::parse("auto"), Decision::Auto);
        assert_eq!(Decision::parse("block"), Decision::Block);
        assert_eq!(Decision::parse("prompt"), Decision::Prompt);
        assert_eq!(Decision::parse("UNKNOWN"), Decision::Prompt);
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("src/**/*.rs", "src/foo/bar.rs"));
        assert!(!glob_match("*.txt", "main.rs"));
    }

    #[test]
    fn test_regex_match() {
        assert!(regex_match(r"^git ", "git status"));
        assert!(!regex_match(r"^git ", "rm -rf /"));
        assert!(!regex_match("[invalid", "test"));
    }
}
