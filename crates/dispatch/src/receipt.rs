//! What actually happened in a run. Bounded and schema-checked like every
//! other thing that crosses a boundary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{BudgetKind, Effect};

/// How a run ended. A closed list; every run ends in exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The model gave an answer.
    Answered,
    /// The request was refused.
    Refused,
    /// A budget ran out.
    BudgetHit,
    /// A tool failed and the run couldn't go on.
    ToolFailed,
    /// No model could answer.
    ModelFailed,
}

/// What became of one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    /// It ran and returned a valid result.
    Ran,
    /// It was refused and never started.
    Refused,
    /// It started and failed, so it may have had effects.
    Failed,
    /// It started and ran out of time, so it may have had effects.
    TimedOut,
}

impl CallStatus {
    /// Whether the tool actually started.
    #[must_use]
    pub const fn started(self) -> bool {
        !matches!(self, Self::Refused)
    }
}

/// Why a call was refused. A closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    /// The tool wasn't offered to this run, or doesn't exist.
    NotOffered,
    /// The arguments were larger than any tool accepts.
    ArgumentsTooLarge,
    /// The arguments weren't JSON.
    ArgumentsNotJson,
    /// The arguments failed the tool's schema.
    ArgumentsInvalid,
    /// A budget ran out.
    Budget,
}

/// One tool call, as it really went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolEntry {
    /// The tool named. A model can invent this, so it's cut to length.
    #[schemars(length(max = 64))]
    pub tool: String,
    /// What became of it.
    pub status: CallStatus,
    /// Why it was refused.
    pub refusal: Option<RefusalKind>,
    /// What the tool can do, when the tool exists.
    pub effect: Option<Effect>,
    /// Whether the result was cut to fit the channel.
    pub truncated: bool,
    /// Whether the result came from outside and is labelled untrusted.
    pub untrusted: bool,
    /// How long it ran.
    pub duration_ms: u64,
    /// What it spent, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// What went wrong, when something did.
    #[schemars(length(max = 512))]
    pub detail: Option<String>,
}

/// One model call, as it really went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// The model that answered; absent if none did.
    #[schemars(length(max = 64))]
    pub model: Option<String>,
    /// Tokens read.
    pub input_tokens: u64,
    /// Tokens written.
    pub output_tokens: u64,
    /// True if the server didn't report usage, so these are estimates.
    pub tokens_estimated: bool,
    /// What it cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// How many models failed before this one, or failed altogether.
    pub failed_attempts: u32,
    /// What went wrong, when nothing answered.
    #[schemars(length(max = 512))]
    pub detail: Option<String>,
}

/// What the run used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    /// Model calls.
    pub rounds: u32,
    /// Tool calls, including the ones refused.
    pub tool_calls: u32,
    /// Writes started.
    pub writes: u32,
    /// Tokens read and written.
    pub tokens: u64,
    /// Money, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// Wall-clock time.
    pub duration_ms: u64,
}

/// What a run did. It goes out with the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    /// The run.
    #[schemars(length(max = 64))]
    pub run: String,
    /// How it ended.
    pub outcome: Outcome,
    /// Which limit it hit, if it hit one.
    pub budget_hit: Option<BudgetKind>,
    /// Every model call.
    #[schemars(length(max = 512))]
    pub model_calls: Vec<ModelEntry>,
    /// Every tool call, up to the budget.
    #[schemars(length(max = 512))]
    pub tool_calls: Vec<ToolEntry>,
    /// Calls made after a budget ran out. Refused, and only counted here.
    pub calls_not_recorded: u64,
    /// How many events the sink lost. The trajectory is incomplete if not 0.
    pub events_lost: u32,
    /// What it used.
    pub totals: Totals,
}
