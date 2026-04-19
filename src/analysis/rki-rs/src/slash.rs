//! Slash-command registry and dispatch.
//!
//! Commands like `/skill`, `/flow`, and `/compact` are registered here.

use std::collections::HashMap;
use std::sync::Arc;

/// A parsed slash command: `/name args...`
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SlashCommand {
    pub name: String,
    pub args: Vec<String>,
    pub raw: String,
}

/// Effect of a slash command: handlers return this; `KimiSoul` applies side effects then the user message.
#[derive(Debug, Clone)]
pub enum SlashOutcome {
    Message(String),
    EnterPlan,
    ExitPlan,
    EnterRalph { max_iterations: usize },
    /// Toggle YOLO (approval bypass) for the session.
    ToggleYolo,
}

impl SlashCommand {
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let without_slash = &trimmed[1..];
        let mut parts = without_slash.split_whitespace();
        let name = parts.next()?.to_string();
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();
        Some(Self {
            name,
            args,
            raw: trimmed.to_string(),
        })
    }
}

pub type SlashHandler =
    Arc<dyn Fn(&SlashCommand, &crate::runtime::Runtime) -> anyhow::Result<SlashOutcome> + Send + Sync>;

/// Registry of slash command handlers.
#[derive(Clone)]
pub struct SlashRegistry {
    handlers: HashMap<String, SlashHandler>,
}

impl SlashRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    pub fn register<F>(&mut self, name: impl Into<String>, handler: F)
    where
        F: Fn(&SlashCommand, &crate::runtime::Runtime) -> anyhow::Result<SlashOutcome> + Send + Sync + 'static,
    {
        self.handlers.insert(name.into(), Arc::new(handler));
    }

    pub fn handle(
        &self,
        cmd: &SlashCommand,
        runtime: &crate::runtime::Runtime,
    ) -> Option<anyhow::Result<SlashOutcome>> {
        self.handlers.get(&cmd.name).map(|h| h(cmd, runtime))
    }

    pub fn names(&self) -> Vec<&str> {
        self.handlers.keys().map(|s| s.as_str()).collect()
    }
}

impl Default for SlashRegistry {
    fn default() -> Self {
        let mut reg = Self::new();
        reg.register("exit", |_cmd, _rt| Ok(SlashOutcome::Message("Exiting...".to_string())));
        reg.register("quit", |_cmd, _rt| Ok(SlashOutcome::Message("Exiting...".to_string())));
        reg.register("plan", |_cmd, _rt| Ok(SlashOutcome::EnterPlan));
        reg.register("unplan", |_cmd, _rt| Ok(SlashOutcome::ExitPlan));
        reg.register("yolo", |_cmd, _rt| Ok(SlashOutcome::ToggleYolo));
        reg.register("ralph", |cmd, _rt| {
            let max = cmd
                .args
                .first()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            Ok(SlashOutcome::EnterRalph {
                max_iterations: max,
            })
        });
        reg.register("skill", |cmd, _rt| {
            let skill = cmd.args.first().cloned().unwrap_or_default();
            Ok(SlashOutcome::Message(format!("Loading skill: {}", skill)))
        });
        reg.register("todo", |cmd, _rt| {
            let rest = cmd.args.join(" ");
            Ok(SlashOutcome::Message(format!("Todo: {}", rest)))
        });
        reg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_slash_command() {
        let cmd = SlashCommand::parse("/plan").unwrap();
        assert_eq!(cmd.name, "plan");
        assert!(cmd.args.is_empty());

        let cmd = SlashCommand::parse("/ralph 10").unwrap();
        assert_eq!(cmd.name, "ralph");
        assert_eq!(cmd.args, vec!["10"]);

        let cmd = SlashCommand::parse("/skill doc").unwrap();
        assert_eq!(cmd.name, "skill");
        assert_eq!(cmd.args, vec!["doc"]);
    }

    #[test]
    fn test_parse_not_slash() {
        assert!(SlashCommand::parse("hello").is_none());
        assert!(SlashCommand::parse("  hello").is_none());
    }

    #[test]
    fn test_registry_default_commands() {
        let reg = SlashRegistry::default();
        let names = reg.names();
        assert!(names.contains(&"exit"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"ralph"));
        assert!(names.contains(&"skill"));
    }

    #[tokio::test]
    async fn test_slash_handler_execution() {
        let reg = SlashRegistry::default();
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let rt = crate::runtime::Runtime::new(
            crate::config::Config::default(),
            crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            std::sync::Arc::new(crate::approval::ApprovalRuntime::new(crate::wire::RootWireHub::new(), true, vec![])),
            crate::wire::RootWireHub::new(),
            store,
        );

        let cmd = SlashCommand::parse("/plan").unwrap();
        assert!(matches!(
            reg.handle(&cmd, &rt).unwrap().unwrap(),
            SlashOutcome::EnterPlan
        ));

        let cmd = SlashCommand::parse("/ralph 10").unwrap();
        match reg.handle(&cmd, &rt).unwrap().unwrap() {
            SlashOutcome::EnterRalph { max_iterations } => assert_eq!(max_iterations, 10),
            other => panic!("expected EnterRalph, got {:?}", other),
        }

        let cmd = SlashCommand::parse("/skill doc").unwrap();
        match reg.handle(&cmd, &rt).unwrap().unwrap() {
            SlashOutcome::Message(s) => assert!(s.contains("skill")),
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_slash_with_multiple_args() {
        let cmd = SlashCommand::parse("/todo fix bug and write tests").unwrap();
        assert_eq!(cmd.name, "todo");
        assert_eq!(cmd.args, vec!["fix", "bug", "and", "write", "tests"]);
    }

    #[tokio::test]
    async fn test_unknown_command_returns_none() {
        let reg = SlashRegistry::default();
        let store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let rt = crate::runtime::Runtime::new(
            crate::config::Config::default(),
            crate::session::Session::create(&store, std::env::current_dir().unwrap()).unwrap(),
            std::sync::Arc::new(crate::approval::ApprovalRuntime::new(crate::wire::RootWireHub::new(), true, vec![])),
            crate::wire::RootWireHub::new(),
            store,
        );

        let cmd = SlashCommand::parse("/nonexistent").unwrap();
        assert!(reg.handle(&cmd, &rt).is_none());
    }
}
