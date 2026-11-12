//! The limits on a run. Everything a loop can do has one.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What one run may use. Every field is required: a run doesn't get an
/// unlimited anything by leaving it out. Calls are capped at 512 so a receipt
/// is always bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Model calls.
    #[schemars(range(min = 1, max = 512))]
    pub max_rounds: u32,
    /// Tool calls, including the ones refused.
    #[schemars(range(min = 0, max = 512))]
    pub max_tool_calls: u32,
    /// Tool calls that write.
    #[schemars(range(min = 0, max = 512))]
    pub max_writes: u32,
    /// Tokens read and written across all model calls.
    #[schemars(range(min = 1))]
    pub max_total_tokens: u64,
    /// Money, in millionths of a dollar.
    pub max_usd_micros: u64,
    /// The size of one request to a model.
    #[schemars(range(min = 1))]
    pub max_request_bytes: u64,
    /// Wall-clock time.
    #[schemars(range(min = 1))]
    pub max_duration_ms: u64,
}

/// Which limit a run hit. A closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    /// Model calls.
    Rounds,
    /// Tool calls.
    ToolCalls,
    /// Writes.
    Writes,
    /// Tokens.
    Tokens,
    /// Money.
    Spend,
    /// Request size.
    RequestSize,
    /// Time.
    Time,
    /// The runtime was stopping, so the run ended early.
    Stopped,
}
