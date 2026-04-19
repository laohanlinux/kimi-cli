//! Subagent lifecycle: store, runner, and wire forwarding.
//!
//! Subagents are isolated agent instances spawned by the `AgentTool`.

pub mod runner;
pub mod store;

pub use runner::ForegroundSubagentRunner;
