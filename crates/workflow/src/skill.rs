//! Skills: instruction text an agent gets only when the task calls for it,
//! so what only some tasks need isn't paid for on every run.

use dispatch::pattern;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The most skills one agent may have.
pub const MAX_SKILLS: usize = 16;

/// The longest a skill's text may be.
pub const MAX_SKILL_CHARS: usize = 4000;

/// Instructions for some tasks, added when the task matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    /// Its name, unique within the agent.
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-z][a-z0-9_]*$"))]
    pub name: String,
    /// When it applies: a pattern matched against the task as the agent
    /// receives it, ignoring case.
    #[schemars(length(min = 1, max = 256))]
    pub when: String,
    /// When it doesn't, even if `when` matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub unless: Option<String>,
    /// What the agent is told when it applies.
    #[schemars(length(min = 1, max = MAX_SKILL_CHARS))]
    pub instructions: String,
}

impl Skill {
    /// Whether it applies to `task`. A pattern that doesn't compile never
    /// applies; [`check_skills`] refuses one before it runs.
    #[must_use]
    pub fn applies(&self, task: &str) -> bool {
        let Ok(when) = pattern(&self.when) else {
            return false;
        };
        let Ok(unless) = self.unless.as_deref().map(pattern).transpose() else {
            return false;
        };
        when.is_match(task) && !unless.is_some_and(|unless| unless.is_match(task))
    }
}

/// Checks a list of skills: the names are unique and the patterns compile.
///
/// # Errors
///
/// The first problem, in a sentence.
pub fn check_skills(skills: &[Skill]) -> Result<(), String> {
    if skills.len() > MAX_SKILLS {
        return Err(format!("at most {MAX_SKILLS} skills"));
    }
    for (index, skill) in skills.iter().enumerate() {
        for text in std::iter::once(&skill.when).chain(&skill.unless) {
            pattern(text).map_err(|why| format!("the skill {}: {why}", skill.name))?;
        }
        if skills
            .iter()
            .take(index)
            .any(|earlier| earlier.name == skill.name)
        {
            return Err(format!("two skills are named {}", skill.name));
        }
    }
    Ok(())
}

/// The instructions `system_prompt` becomes for `task`: the skills that
/// apply, named, after it. Returns the text and the skills' names.
#[must_use]
pub fn with_skills(system_prompt: &str, skills: &[Skill], task: &str) -> (String, Vec<String>) {
    let mut text = system_prompt.to_owned();
    let mut names = Vec::new();
    for skill in skills.iter().filter(|skill| skill.applies(task)) {
        text.push_str("\n\nSkill ");
        text.push_str(&skill.name);
        text.push_str(":\n");
        text.push_str(&skill.instructions);
        names.push(skill.name.clone());
    }
    (text, names)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn skill(when: &str, unless: Option<&str>) -> Skill {
        Skill {
            name: "citing".to_owned(),
            when: when.to_owned(),
            unless: unless.map(str::to_owned),
            instructions: "Cite each source.".to_owned(),
        }
    }

    #[test]
    fn a_skill_is_added_only_when_the_task_matches() {
        let skills = [skill(r"\bsources?\b", Some("no sources"))];
        let (text, names) = with_skills("You help.", &skills, "Find SOURCES on otters");
        assert_eq!(text, "You help.\n\nSkill citing:\nCite each source.");
        assert_eq!(names, ["citing"]);
        let (text, names) = with_skills("You help.", &skills, "otters, no sources please");
        assert_eq!((text.as_str(), names.len()), ("You help.", 0));
        let (text, _) = with_skills("You help.", &skills, "otters");
        assert_eq!(text, "You help.");
    }

    #[test]
    fn a_bad_pattern_or_a_repeated_name_is_refused() {
        assert_eq!(check_skills(&[skill("a", None)]), Ok(()));
        assert!(
            check_skills(&[skill("(", None)])
                .unwrap_err()
                .contains("citing")
        );
        assert!(check_skills(&[skill("a", Some("["))]).is_err());
        let twice = [skill("a", None), skill("b", None)];
        assert!(check_skills(&twice).unwrap_err().contains("two"));
        assert!(!skill("(", None).applies("("));
    }
}
