//! What an agent runs with on one task: its own instructions and the skills
//! that apply, the same for a chat's profile, a workflow step or a probe.

use crate::{Agent, with_skills};

/// What an agent runs with on one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    /// Its instructions, with the skills that apply.
    pub text: String,
    /// The skills included, by name.
    pub skills: Vec<String>,
}

/// The instructions `agent` runs with on `task`: its own, and the skills
/// that apply.
#[must_use]
pub fn instructions(agent: &Agent, task: &str) -> Instructions {
    let (text, skills) = with_skills(&agent.system_prompt, &agent.skills, task);
    Instructions { text, skills }
}
