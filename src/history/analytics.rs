//! Analytics types for history aggregation.
//!
//! The aggregation logic lives on [`super::fjall_store::FjallHistoryStore`].

use rust_decimal::Decimal;

/// Statistics about jobs.
#[derive(Debug, Default)]
pub struct JobStats {
    pub total_jobs: u64,
    pub completed_jobs: u64,
    pub failed_jobs: u64,
    pub success_rate: f64,
    pub avg_duration_secs: f64,
    pub avg_cost: Decimal,
    pub total_cost: Decimal,
}

/// Statistics about tool usage.
#[derive(Debug)]
pub struct ToolStats {
    pub tool_name: String,
    pub total_calls: u64,
    pub successful_calls: u64,
    pub failed_calls: u64,
    pub success_rate: f64,
    pub avg_duration_ms: f64,
    pub total_cost: Decimal,
}

/// Estimation accuracy metrics.
#[derive(Debug, Default)]
pub struct EstimationAccuracy {
    pub cost_error_rate: f64,
    pub time_error_rate: f64,
    pub sample_count: u64,
}

/// Historical entry for a category.
#[derive(Debug)]
pub struct CategoryHistoryEntry {
    pub tool_names: Vec<String>,
    pub estimated_cost: Decimal,
    pub actual_cost: Option<Decimal>,
    pub estimated_time_secs: i32,
    pub actual_time_secs: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
