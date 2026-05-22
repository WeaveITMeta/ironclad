//! Shared record types for the history store.
//!
//! The store implementation lives in [`super::fjall_store`]; this module holds
//! types shared across it.

use rust_decimal::Decimal;
use uuid::Uuid;

/// Record for an LLM call to be persisted.
#[derive(Debug, Clone)]
pub struct LlmCallRecord<'a> {
    pub job_id: Option<Uuid>,
    pub conversation_id: Option<Uuid>,
    pub provider: &'a str,
    pub model: &'a str,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost: Decimal,
    pub purpose: Option<&'a str>,
}
