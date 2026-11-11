//! Harness²: the system improves its own agents from evidence.
//!
//! An attempt probes an agent as it is on a task, has an editor write
//! candidates from that run, probes each candidate, and has a composer compare
//! the runs. Judges are models, so nothing they say is taken on trust: every
//! citation must be a quote from a step of a run, and a run that ends worse
//! than the reference counts as a regression whatever a judge says.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod attempt;
mod cite;
mod guard;
mod records;
mod render;
mod roles;

pub use attempt::{Attempt, Candidate, Checked, Kind, Loop, Probed, Request, Settings, Status};
pub use cite::Cite;
pub use guard::{MAX_GROWTH_CHARS, check_instructions};
pub use records::{ATTEMPT_TYPE, declarations, record_of};
pub use render::Rendered;
pub use roles::{
    Brief, ComposerReply, Decision, Draft, EditorReply, Effect, Judged, Judges, PURPOSE, Verdict,
};
