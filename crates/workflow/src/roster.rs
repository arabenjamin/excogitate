//! The roster: the agents an agent with `delegate` can hand work to. It's
//! added to that agent's instructions whenever it runs, as a chat, a
//! workflow step or a probe, so each sees the same list.

use std::fmt::Write as _;

use store::{DefinitionKind, PageRequest};

use crate::engine::Engine;
use crate::{Agent, DELEGATE_TOOL, with_skills};

/// What an agent runs with on one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    /// Its instructions, with the skills that apply and its roster.
    pub text: String,
    /// The skills included, by name.
    pub skills: Vec<String>,
}

/// The most agents listed.
pub const ROSTER_LIMIT: u32 = 100;

fn roster_text(lines: &[String], cut: bool) -> String {
    if lines.is_empty() {
        return "There are no agents to hand work to with `delegate` yet.".to_owned();
    }
    let mut text = format!(
        "The agents you can hand work to with `delegate`, by name:\n{}",
        lines.join("\n")
    );
    if cut {
        let _ = write!(
            text,
            "\n(Only the first {ROSTER_LIMIT} are listed; there are more.)"
        );
    }
    text
}

impl Engine {
    /// The agents `own` can hand work to, read now, so a new or changed
    /// agent is there on the next run. An agent that can delegate itself is
    /// left out, as `delegate` would refuse it.
    ///
    /// # Errors
    ///
    /// Says why, if the definitions can't be read.
    pub async fn roster(&self, own: &str) -> Result<String, String> {
        let page = PageRequest::new(ROSTER_LIMIT, 0).map_err(|error| error.to_string())?;
        let listed = self
            .store
            .list_definitions(DefinitionKind::Agent, page)
            .await
            .map_err(|error| schema::bounded(&error.to_string()))?;
        let mut lines = Vec::new();
        for definition in &listed.items {
            let Ok(agent) = serde_json::from_value::<Agent>(definition.body.clone()) else {
                continue;
            };
            let name = &definition.key.name;
            if name == own || agent.tools.iter().any(|tool| tool == DELEGATE_TOOL) {
                continue;
            }
            let what = if agent.description.is_empty() {
                "(no description)"
            } else {
                agent.description.as_str()
            };
            lines.push(format!("- {name}: {what}"));
        }
        Ok(roster_text(&lines, listed.has_more))
    }

    /// The instructions agent `name` runs with on `task`: its own, the
    /// skills that apply, and the roster if it can delegate.
    ///
    /// # Errors
    ///
    /// Says why, if the roster can't be read.
    pub async fn instructions(
        &self,
        name: &str,
        agent: &Agent,
        task: &str,
    ) -> Result<Instructions, String> {
        let (mut text, skills) = with_skills(&agent.system_prompt, &agent.skills, task);
        if agent.tools.iter().any(|tool| tool == DELEGATE_TOOL) {
            text.push_str("\n\n");
            text.push_str(&self.roster(name).await?);
        }
        Ok(Instructions { text, skills })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_roster_says_so_and_a_cut_one_says_that() {
        assert!(roster_text(&[], false).contains("no agents"));
        let lines = vec!["- a: does a".to_owned()];
        assert!(!roster_text(&lines, false).contains("more"));
        assert!(roster_text(&lines, true).contains("there are more"));
    }
}
