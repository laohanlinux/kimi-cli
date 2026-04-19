//! Stream-based notification system for out-of-turn events.
//!
//! Notifications are persisted in SQLite and delivered at the start of
//! the next turn.

pub mod manager;
pub mod types;

pub use manager::NotificationManager;
