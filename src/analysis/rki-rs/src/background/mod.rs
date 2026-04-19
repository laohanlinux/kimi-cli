//! Background task system with dependency management and heartbeat.
//!
//! `BackgroundTaskManager` queues tasks, tracks dependencies, and recovers
//! pending work across process restarts.

pub mod executor;
pub mod manager;
pub mod types;

pub use manager::{task_events_stream, BackgroundTaskManager};
pub use types::*;
