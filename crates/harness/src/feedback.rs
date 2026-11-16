//! The owner's word on a run. A run the owner marked bad is the strongest
//! evidence there is that something went wrong, so it counts on its own,
//! where a failure seen in a run counts only once it repeats.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use store::RecordId;
use uuid::Uuid;

/// The record type of the owner's feedback on a run.
pub const FEEDBACK_TYPE: &str = "harness.feedback";

/// What the owner thought of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    /// It went wrong. Harness² treats the run as evidence for an attempt.
    Bad,
    /// It went right. Kept, and not yet used.
    Good,
}

/// Feedback on a run, as `PUT /api/v1/runs/{id}/feedback` takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackRequest {
    /// Bad or good.
    pub mark: Mark,
    /// What was wrong, or right, in the owner's words. Harness²'s editor
    /// reads it as the reported problem.
    #[serde(default)]
    #[schemars(length(min = 1, max = 2048))]
    pub note: Option<String>,
}

/// The owner's feedback on one run, as it's kept. One per run: marking a run
/// again replaces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Feedback {
    /// The run.
    #[schemars(length(max = 36))]
    pub run: String,
    /// What ran: `chat`, `agent:<name>` or `tool:<name>`.
    #[schemars(length(max = 128))]
    pub agent: String,
    /// Bad or good.
    pub mark: Mark,
    /// The owner's words, if any.
    #[serde(default)]
    #[schemars(length(max = 2048))]
    pub note: Option<String>,
    /// When it was given.
    pub at: DateTime<Utc>,
}

/// A run's feedback id: one per run.
#[must_use]
pub fn feedback_id(run: &str) -> RecordId {
    let seed = format!("harness.feedback|{run}");
    RecordId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_has_one_feedback_id() {
        assert_eq!(feedback_id("a"), feedback_id("a"));
        assert_ne!(feedback_id("a"), feedback_id("b"));
    }

    #[test]
    fn a_request_names_its_mark() {
        let request: FeedbackRequest =
            serde_json::from_str(r#"{"mark": "bad", "note": "it missed the todo"}"#).unwrap();
        assert_eq!(request.mark, Mark::Bad);
        assert!(serde_json::from_str::<FeedbackRequest>(r#"{"mark": "meh"}"#).is_err());
        assert!(serde_json::from_str::<FeedbackRequest>(r#"{"mark": "bad", "x": 1}"#).is_err());
    }
}
