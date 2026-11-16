//! Harness²: the system improves its own agents from evidence.
//!
//! An attempt probes an agent as it is on a task, has an editor write
//! candidates from that run, probes each candidate, and has a composer compare
//! the runs. Judges are models, so nothing they say is taken on trust: every
//! citation must be a quote from a step of a run, and a run that ends worse
//! than the reference counts as a regression whatever a judge says.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod attempt;
mod calibrate;
mod change;
mod cite;
mod feedback;
mod flow;
mod guard;
mod lesson;
mod pick;
mod records;
mod render;
mod roles;

pub use attempt::{
    Adoption, Attempt, Candidate, Checked, Kind, Loop, PROFILE_PART, Probed, Request, RouteProbe,
    Settings, Status, WRITER_PART,
};
pub use calibrate::{Calibration, CaseResult};
pub use change::{
    CALIBRATION_DAYS, Change, ChangeStatus, Facts, Hold, MAX_FILE_CHARS, MIN_HOURS_BETWEEN_CHANGES,
    check_adoption,
};
pub use cite::Cite;
pub use feedback::{FEEDBACK_TYPE, Feedback, FeedbackRequest, Mark, feedback_id};
pub use flow::{
    StepEdit, Target, apply_edits, check_edited_reads, check_workflow_edit, merge_edits, read_tools,
};
pub use guard::{MAX_GROWTH_CHARS, MAX_SKILL_GROWTH_CHARS, check_harness, check_instructions};
pub use lesson::{
    LESSON_TYPE, Lesson, LessonStatus, MAX_SHOWN, lesson_id, lessons_for, situation, status_filter,
};
pub use pick::{
    Evidence, Finding, MARKED_BAD, MIN_REPEATS, Marked, Pick, Recent, best, chat_evidence,
    chat_failures, evidence, failure, pick, task,
};
pub use records::{
    ATTEMPT_TYPE, CALIBRATION_TYPE, CHANGE_TYPE, HOLD_TYPE, declarations, hold_id, record,
    record_of,
};
pub use render::Rendered;
pub use roles::{
    Brief, ComposerReply, Decision, Draft, EditorReply, Effect, Judged, Judges, PURPOSE, Verdict,
    WorkflowDraft, WorkflowEditorReply,
};
