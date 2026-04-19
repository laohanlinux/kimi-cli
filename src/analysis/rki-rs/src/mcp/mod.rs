//! MCP (Model Context Protocol) client and tool bridging.
//!
//! Connects to external MCP servers and exposes their tools through the
//! `Tool` trait.

pub mod client;
pub mod tools;

pub use client::{MCPClient, MCPSession};
pub use tools::MCPTool;
