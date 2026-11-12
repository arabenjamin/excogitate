//! The lesson library. Each adopted change leaves a lesson: the situation
//! it was made in, what was changed, and the evidence. A lesson is written
//! only when a change is adopted, so a judge's mistaken verdict never
//! becomes one, and it's retired when its change is reverted or edited
//! away. It's looked up when an agent is being improved, never inherited:
//! the editor sees the few that fit, as hints to weigh against the runs.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use store::{FieldFilter, FieldOp, FieldValue, PageRequest, RecordId, RecordQuery, Store};
use uuid::Uuid;

use crate::cite::Cite;

/// The record type of a lesson.
pub const LESSON_TYPE: &str = "harness.lesson";

/// The most lessons an editor is shown.
pub const MAX_SHOWN: usize = 3;

/// The most lessons read when looking for those that fit.
const MAX_READ: u32 = 1000;

/// Whether a lesson still counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LessonStatus {
    /// Its change is in effect.
    Active,
    /// Its change was reverted or edited away, so it no longer counts.
    Retired,
}

/// What one adopted change taught.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lesson {
    /// The agent that was changed.
    #[schemars(length(max = 64))]
    pub agent: String,
    /// The change that taught it.
    #[schemars(length(max = 36))]
    pub change_id: String,
    /// When it was adopted.
    pub at: DateTime<Utc>,
    /// When it applies: the task, and the problem reported, if one was.
    #[schemars(length(max = 2400))]
    pub situation: String,
    /// What was changed, as the editor put it.
    #[schemars(length(max = 1000))]
    pub lesson: String,
    /// The moments in the runs that supported it.
    #[schemars(length(max = 8))]
    pub evidence: Vec<Cite>,
    /// Whether it still counts.
    pub status: LessonStatus,
}

/// A lesson's id: one per change, so retiring it finds it.
#[must_use]
pub fn lesson_id(change_id: RecordId) -> RecordId {
    let seed = format!("{LESSON_TYPE}|{change_id}");
    RecordId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
}

/// The situation an attempt was about, as a lesson keeps it.
#[must_use]
pub fn situation(task: &str, why: Option<&str>) -> String {
    let task: String = task.chars().take(2000).collect();
    match why {
        Some(why) => {
            let why: String = why.chars().take(380).collect();
            format!("{task} (reported: {why})")
        }
        None => task,
    }
}

/// The words that say what something is about: lowercase, four letters or
/// more, so small words don't make unrelated lessons look alike.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect()
}

/// How well `lesson` fits an attempt on `agent` about `wanted`: the share
/// of its words the attempt shares, with a lesson for the same agent ahead
/// of any other. Zero means it doesn't fit.
fn fit(lesson: &Lesson, agent: &str, wanted: &BTreeSet<String>) -> f64 {
    let own = words(&format!("{} {}", lesson.situation, lesson.lesson));
    if own.is_empty() {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "word counts are small enough to be exact"
    )]
    let shared = own.intersection(wanted).count() as f64 / own.len() as f64;
    let same_agent = if lesson.agent == agent { 1.0 } else { 0.0 };
    if shared == 0.0 && same_agent == 0.0 {
        return 0.0;
    }
    same_agent + shared
}

/// A lesson as the editor reads it.
fn shown(lesson: &Lesson) -> String {
    let when: String = lesson.situation.chars().take(300).collect();
    format!(
        "- From change {} to {}. When: {when}. What helped: {}",
        lesson.change_id, lesson.agent, lesson.lesson
    )
}

/// The lessons that fit, best first, as `(change id, text)`: at most
/// [`MAX_SHOWN`], and none if nothing fits or the library can't be read.
pub async fn lessons_for(
    store: &dyn Store,
    agent: &str,
    task: &str,
    why: Option<&str>,
) -> Vec<(String, String)> {
    let Ok(page) = PageRequest::new(MAX_READ, 0) else {
        return Vec::new();
    };
    let query = RecordQuery::new(LESSON_TYPE, page).with_filter(
        "status",
        FieldOp::Eq,
        FieldValue::Text("active".to_owned()),
    );
    let Ok(found) = store.query_records(query).await else {
        return Vec::new();
    };
    let wanted = words(&situation(task, why));
    let mut fitting: Vec<(f64, Lesson)> = found
        .items
        .into_iter()
        .filter_map(|record| serde_json::from_value::<Lesson>(record.data).ok())
        .map(|lesson| (fit(&lesson, agent, &wanted), lesson))
        .filter(|(score, _)| *score > 0.0)
        .collect();
    // Best fit first; among equals, the newest.
    fitting.sort_by(|(a, one), (b, other)| b.total_cmp(a).then(other.at.cmp(&one.at)));
    fitting
        .into_iter()
        .take(MAX_SHOWN)
        .map(|(_, lesson)| (lesson.change_id.clone(), shown(&lesson)))
        .collect()
}

/// A filter for a lesson's status, for listing.
#[must_use]
pub fn status_filter(status: LessonStatus) -> FieldFilter {
    let name = match status {
        LessonStatus::Active => "active",
        LessonStatus::Retired => "retired",
    };
    FieldFilter::new("status", FieldOp::Eq, FieldValue::Text(name.to_owned()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
mod tests {
    use super::*;

    fn lesson(agent: &str, situation: &str, what: &str) -> Lesson {
        Lesson {
            agent: agent.to_owned(),
            change_id: "c".to_owned(),
            at: Utc::now(),
            situation: situation.to_owned(),
            lesson: what.to_owned(),
            evidence: Vec::new(),
            status: LessonStatus::Active,
        }
    }

    #[test]
    fn a_lesson_fits_by_shared_words_and_its_own_agent_first() {
        let wanted = words("Find the article about otters and start reading it");
        let same = lesson(
            "intake",
            "Queue a source",
            "Start the reading after confirming",
        );
        let alike = lesson(
            "other",
            "Find an article about otters",
            "Start reading what you find",
        );
        let unrelated = lesson("other", "Sum the invoices", "Use the calculator");
        assert!(fit(&same, "intake", &wanted) > fit(&alike, "intake", &wanted));
        assert!(fit(&alike, "intake", &wanted) > 0.0);
        assert!(fit(&unrelated, "intake", &wanted).abs() < f64::EPSILON);
    }

    #[test]
    fn short_words_dont_count() {
        assert!(words("a an the of to it").is_empty());
        assert_eq!(
            words("Otters, OTTERS!"),
            BTreeSet::from(["otters".to_owned()])
        );
    }

    #[test]
    fn the_situation_keeps_the_reported_problem() {
        assert_eq!(
            situation("Do it", Some("it didn't")),
            "Do it (reported: it didn't)"
        );
        assert_eq!(situation("Do it", None), "Do it");
    }
}
