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
mod probe;
mod roster;
mod skill;
mod template;

pub use definition::{
    Agent, AgentMemory, ForEach, MAX_ITEMS, Step, Workflow, check_agent, check_agent_sources,
    check_workflow,
};
pub use engine::{
    DELEGATE_TOOL, Engine, EngineLimits, RunOutcome, StepReport, StepStatus, WorkflowError,
    WorkflowRun, check_workflow_run,
};
/// The sources each agent's run may read; the memory tool reads it.
pub use memory::SourceChoices;
pub use probe::ProbeRun;
pub use roster::{Instructions, ROSTER_LIMIT};
pub use skill::{MAX_SKILL_CHARS, MAX_SKILLS, Skill, check_skills, with_skills};
pub use template::{TemplateError, render_text, render_value};
