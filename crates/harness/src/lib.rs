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
mod guard;
mod records;
mod render;
mod roles;

pub use attempt::{
    Adoption, Attempt, Candidate, Checked, Kind, Loop, Probed, Request, Settings, Status,
};
pub use calibrate::{Calibration, CaseResult};
pub use change::{
    CALIBRATION_DAYS, Change, ChangeStatus, Facts, Hold, MAX_FILE_CHARS, MIN_HOURS_BETWEEN_CHANGES,
    check_adoption,
};
pub use cite::Cite;
pub use guard::{MAX_GROWTH_CHARS, check_instructions};
pub use records::{
    ATTEMPT_TYPE, CALIBRATION_TYPE, CHANGE_TYPE, HOLD_TYPE, declarations, hold_id, record,
    record_of,
};
pub use render::Rendered;
pub use roles::{
    Brief, ComposerReply, Decision, Draft, EditorReply, Effect, Judged, Judges, PURPOSE, Verdict,
};
