//! # rki-rs — Rust Kimi CLI Agent
//!
//! A Rust reimplementation of the Kimi Code CLI agent, featuring:
//! - Async agent loop with pluggable orchestrators
//! - SQLite-backed session persistence
//! - Unified Wire event bus
//! - Hot-reloading typed config registry
//! - Capability-based approval system
//! - Background task management
//! - MCP (Model Context Protocol) integration

#![allow(dead_code)]
#![allow(clippy::new_without_default)]

pub mod acp;
pub mod agent;
pub mod agents_md;
pub mod approval;
pub mod background;
pub mod capability;
pub mod capability_registry;
pub mod cli;
pub mod compaction;
pub mod config;
pub mod config_registry;
pub mod config_watcher;
pub mod context;
pub mod context_tree;
pub mod error;
pub mod feature_flags;
pub mod hooks;
pub mod identity;
pub mod injection;
pub mod llm;
pub mod mcp;
pub mod memory;
pub mod message;
pub mod notification;
pub mod orchestrator;
pub mod question;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod slash;
pub mod soul;
pub mod steer;
pub mod store;
pub mod stream;
pub mod subagents;
pub mod token;
pub mod tools;
pub mod toolset;
pub mod turn_input;
pub mod ui;
pub mod user_input;
pub mod wire;
pub mod workdir_ls;
pub mod tui;
