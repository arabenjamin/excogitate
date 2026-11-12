//! Dispatch: the one path from what a model asks for to what actually runs.
//!
//! A [`Session`] belongs to one run. It offers the run's tools, checks every
//! call before it runs, holds the run to its [`Budget`], and writes the
//! [`Receipt`]. Nothing else can add to a receipt, so it says what happened
//! rather than what a model claimed.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod budget;
mod receipt;
mod session;
mod stop;
mod tool;
mod trajectory;

pub use budget::{Budget, BudgetKind};
pub use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
pub use receipt::{CallStatus, ModelEntry, Outcome, Receipt, RefusalKind, ToolEntry, Totals};
pub use session::{
    ChatResult, Conversation, ModelStop, Probe, Session, SessionConfig, SessionError, ToolReply,
    parse_arguments,
};
pub use stop::StopSignal;
pub use tool::{RegistryError, ToolRegistry};
pub use trajectory::{
    RunSubject, Speaker, Step, TimedStep, Trajectory, harness_id, read_trajectory,
};
