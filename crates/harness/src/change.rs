//! Changes the system makes to its own agents, and the rules it must meet
//! before it makes one. The owner is told of every change, can revert any of
//! them, and has the last word: an agent the owner reverted or edited by hand
//! is held, and a harness the owner reverted is never adopted again.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::attempt::{Attempt, Status};
use crate::cite::Cite;

/// The shortest time between two changes to one agent.
pub const MIN_HOURS_BETWEEN_CHANGES: i64 = 24;
/// How long a judge model's passed known-answer test is trusted.
pub const CALIBRATION_DAYS: i64 = 7;
/// The largest agent file kept with a change.
pub const MAX_FILE_CHARS: usize = 64_000;

/// Where a change stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ChangeStatus {
    /// The agent runs with it.
    Live,
    /// The owner put the old file back.
    Reverted,
    /// A later change, or the owner's own edit, took its place.
    Replaced,
}

/// One change the system made to an agent's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Change {
    /// The agent.
    #[schemars(length(max = 64))]
    pub agent: String,
    /// The attempt whose evidence supports it.
    #[schemars(length(max = 36))]
    pub attempt_id: String,
    /// When it was made.
    pub at: DateTime<Utc>,
    /// The harness it replaced.
    #[schemars(length(max = 64))]
    pub from_harness: String,
    /// The harness it put in place.
    #[schemars(length(max = 64))]
    pub to_harness: String,
    /// The file as it was, so it can be put back exactly.
    #[schemars(length(max = MAX_FILE_CHARS))]
    pub old_file: String,
    /// The file as the system wrote it.
    #[schemars(length(max = MAX_FILE_CHARS))]
    pub new_file: String,
    /// What changed, as the editor put it.
    #[schemars(length(max = 1000))]
    pub summary: String,
    /// The checked steps that show the change helped.
    #[schemars(length(max = 8))]
    pub evidence: Vec<Cite>,
    /// The judge model whose decision it was.
    #[schemars(length(max = 256))]
    pub judge_model: String,
    /// Where it stands.
    pub status: ChangeStatus,
    /// When it stopped being live, if it has.
    pub ended_at: Option<DateTime<Utc>>,
    /// Whether the owner was told, and how.
    #[schemars(length(max = 2048))]
    pub notice: String,
}

/// An agent the system leaves alone until the owner releases it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hold {
    /// The agent.
    #[schemars(length(min = 1, max = 64))]
    pub agent: String,
    /// Since when.
    pub since: DateTime<Utc>,
    /// Why.
    #[schemars(length(max = 2048))]
    pub reason: String,
}

/// What the adoption rules look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts<'a> {
    /// The attempt.
    pub attempt: &'a Attempt,
    /// The agent's harness now.
    pub current_harness: &'a str,
    /// Why the agent is held, if it is.
    pub hold: Option<&'a Hold>,
    /// Harnesses of this agent the owner reverted.
    pub reverted: &'a BTreeSet<String>,
    /// When this agent was last changed by the system.
    pub last_change: Option<DateTime<Utc>>,
    /// When the composer's model last passed its known-answer test.
    pub calibrated_at: Option<DateTime<Utc>>,
    /// Now.
    pub now: DateTime<Utc>,
}

/// Checks the rules a supported edit must meet before the system adopts it.
///
/// # Errors
///
/// The first rule it fails, in a sentence.
pub fn check_adoption(facts: &Facts<'_>) -> Result<(), String> {
    let attempt = facts.attempt;
    let Some(candidate) = attempt.supported() else {
        return Err("the evidence supports no change".to_owned());
    };
    if attempt.status != Status::Supported {
        return Err("the attempt didn't end supported".to_owned());
    }
    if let Some(hold) = facts.hold {
        return Err(format!("the agent is held: {}", hold.reason));
    }
    if attempt.base_harness.as_deref() != Some(facts.current_harness) {
        return Err("the agent changed while it was being improved".to_owned());
    }
    let Some(probe) = &candidate.probe else {
        return Err("the candidate was never run".to_owned());
    };
    if facts.reverted.contains(&probe.harness) {
        return Err("the owner reverted this harness before".to_owned());
    }
    if let Some(last) = facts.last_change
        && facts.now.signed_duration_since(last) < Duration::hours(MIN_HOURS_BETWEEN_CHANGES)
    {
        return Err(format!(
            "the agent was changed less than {MIN_HOURS_BETWEEN_CHANGES} hours ago"
        ));
    }
    match facts.calibrated_at {
        Some(at) if facts.now.signed_duration_since(at) <= Duration::days(CALIBRATION_DAYS) => {
            Ok(())
        }
        _ => Err("the judge model hasn't passed its known-answer test".to_owned()),
    }
}
