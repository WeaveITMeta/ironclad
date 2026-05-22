//! History and persistence layer.
//!
//! Stores job history, conversations, and actions in PostgreSQL for:
//! - Audit trail
//! - Learning from past executions
//! - Analytics and metrics

mod analytics;
mod fjall_store;
mod store;

pub use analytics::{CategoryHistoryEntry, EstimationAccuracy, JobStats, ToolStats};
pub use fjall_store::FjallHistoryStore;
pub use store::{LlmCallRecord, Store};
