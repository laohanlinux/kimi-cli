//! Shared error types for the agent.
//!
//! `AgentError` is the top-level error enum used across tools and soul.

use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum RkiError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Config error: {0}")]
    Config(String),
    #[error("Session error: {0}")]
    Session(String),
    #[error("Tool error: {0}")]
    Tool(String),
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Approval rejected")]
    ApprovalRejected,
    #[error("Compaction error: {0}")]
    Compaction(String),
    #[error("Unknown tool: {0}")]
    UnknownTool(String),
    #[error("General error: {0}")]
    General(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let e = RkiError::Config("bad model".to_string());
        assert_eq!(e.to_string(), "Config error: bad model");

        let e = RkiError::ApprovalRejected;
        assert_eq!(e.to_string(), "Approval rejected");

        let e = RkiError::UnknownTool("foo".to_string());
        assert_eq!(e.to_string(), "Unknown tool: foo");
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let e: RkiError = io_err.into();
        assert!(e.to_string().contains("IO error"));
    }

    #[test]
    fn test_error_from_anyhow() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let e: RkiError = anyhow_err.into();
        assert!(e.to_string().contains("General error"));
    }

    #[test]
    fn test_tool_error_display() {
        let e = RkiError::Tool("bad args".to_string());
        assert_eq!(e.to_string(), "Tool error: bad args");
    }

    #[test]
    fn test_session_error_display() {
        let e = RkiError::Session("not found".to_string());
        assert_eq!(e.to_string(), "Session error: not found");
    }

    #[test]
    fn test_compaction_error_display() {
        let e = RkiError::Compaction("summary failed".to_string());
        assert_eq!(e.to_string(), "Compaction error: summary failed");
    }
}
