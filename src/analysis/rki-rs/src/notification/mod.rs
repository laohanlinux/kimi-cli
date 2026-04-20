//! Stream-based notification system for out-of-turn events.
//!
//! Notifications are persisted in SQLite and delivered at the start of
//! the next turn.

pub mod llm;
pub mod manager;
pub mod task_terminal;
pub mod types;

pub use manager::NotificationManager;
