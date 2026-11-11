//! Workflows: a fixed list of steps, each a tool or a sub-agent. A model can
//! choose what a step says, but not which steps run.
//!
//! A definition is checked completely before it can run: its shape, that each
//! tool and agent exists, each step's arguments against the tool's schema, and
//! that every reference points at something earlier. Each step goes through
//! dispatch, so it has a budget and a receipt like any other work.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod definition;
mod engine;
mod template;

pub use definition::{Agent, Step, Workflow, check_agent, check_workflow};
pub use engine::{
    Engine, EngineLimits, RunOutcome, StepReport, StepStatus, WorkflowError, WorkflowRun,
};
pub use template::{TemplateError, render_text, render_value};
