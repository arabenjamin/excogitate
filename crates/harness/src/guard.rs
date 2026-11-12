//! What a candidate's instructions may not be, whatever a judge says. These
//! are checked in code, because each is a way judges have gone wrong in
//! practice.

use workflow::Agent;

/// Characters a candidate may add to the instructions it replaces. An agent's
/// prompt is paid on every run, and a small model's prompt is a size budget.
pub const MAX_GROWTH_CHARS: usize = 2000;
/// A task shorter than this isn't checked for, since it could appear by chance.
const MIN_TASK_CHARS: usize = 24;
/// Characters a candidate may add to the skills it replaces. A skill is
/// paid for only on the tasks it matches, so it may grow more.
pub const MAX_SKILL_GROWTH_CHARS: usize = 4000;

fn squeeze(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Checks a candidate's instructions against the ones they'd replace.
///
/// # Errors
///
/// Why the candidate can't be used, in a sentence.
pub fn check_instructions(candidate: &str, current: &str, task: &str) -> Result<(), String> {
    if candidate.trim().is_empty() {
        return Err("the instructions are empty".to_owned());
    }
    let (new, old) = (candidate.chars().count(), current.chars().count());
    if new > old.saturating_add(MAX_GROWTH_CHARS) {
        return Err(format!(
            "the instructions grew by {} characters; at most {MAX_GROWTH_CHARS} may be added",
            new - old
        ));
    }
    leaks("the instructions contain", candidate, task)
}

/// Why `text` can't go into an agent, if it can't: it holds the judge's input
/// or the task it was tested on. `what` names it, with its verb, such as
/// "the instructions contain".
fn leaks(what: &str, text: &str, task: &str) -> Result<(), String> {
    // A judge that pastes its own input into the agent leaks the test into
    // every later run.
    if text.contains("===== BEGIN") || text.contains("===== END") {
        return Err(format!("{what} the judge's input"));
    }
    let task = squeeze(task);
    if task.chars().count() >= MIN_TASK_CHARS && squeeze(text).contains(&task) {
        return Err(format!("{what} the task they were tested on"));
    }
    Ok(())
}

fn skill_chars(agent: &Agent) -> usize {
    agent
        .skills
        .iter()
        .map(|skill| skill.instructions.chars().count())
        .sum()
}

/// Checks a candidate agent against the one it would replace: its
/// instructions as [`check_instructions`] does, its skills for growth and
/// leaks, and that it keeps every current guardrail as it is.
///
/// # Errors
///
/// Why the candidate can't be used, in a sentence.
pub fn check_harness(candidate: &Agent, current: &Agent, task: &str) -> Result<(), String> {
    check_instructions(&candidate.system_prompt, &current.system_prompt, task)?;
    let (new, old) = (skill_chars(candidate), skill_chars(current));
    if new > old.saturating_add(MAX_SKILL_GROWTH_CHARS) {
        return Err(format!(
            "the skills grew by {} characters; at most {MAX_SKILL_GROWTH_CHARS} may be added",
            new - old
        ));
    }
    for skill in &candidate.skills {
        let text = format!(
            "{}\n{}\n{}",
            skill.when,
            skill.unless.as_deref().unwrap_or(""),
            skill.instructions
        );
        leaks(&format!("the skill {} contains", skill.name), &text, task)?;
    }
    if let Some(dropped) = current
        .guardrails
        .iter()
        .find(|kept| !candidate.guardrails.contains(kept))
    {
        return Err(format!(
            "it removes or changes the guardrail {}; a candidate may only add guardrails",
            dropped.name
        ));
    }
    for added in candidate
        .guardrails
        .iter()
        .filter(|rule| !current.guardrails.contains(rule))
    {
        let pattern = added.deny.as_ref().map_or("", |deny| deny.matches.as_str());
        let text = format!("{}\n{pattern}", added.because);
        leaks(
            &format!("the guardrail {} contains", added.name),
            &text,
            task,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    const TASK: &str = "Summarize the quarterly report for the board.";

    #[test]
    fn a_sound_edit_passes() {
        assert_eq!(
            check_instructions("You help. Cite sources.", "You help.", TASK),
            Ok(())
        );
    }

    #[test]
    fn what_a_judge_must_not_do_is_refused() {
        let long = "x".repeat(MAX_GROWTH_CHARS + 20);
        let pasted_task = format!("You help.\n{}", TASK.to_uppercase());
        for bad in [
            "   ",
            long.as_str(),
            "You help.\n===== BEGIN TASK =====",
            pasted_task.as_str(),
        ] {
            assert!(check_instructions(bad, "You help.", TASK).is_err(), "{bad}");
        }
    }

    fn agent(value: serde_json::Value) -> Agent {
        serde_json::from_value(value).unwrap()
    }

    fn current() -> Agent {
        agent(serde_json::json!({
            "system_prompt": "You help.",
            "tools": ["memory"],
            "guardrails": [{ "name": "look", "tool": "memory", "because": "Look first.",
                             "required": true }]
        }))
    }

    #[test]
    fn a_candidate_may_add_guardrails_and_skills() {
        let mut edit = current();
        edit.guardrails.push(
            serde_json::from_value(serde_json::json!({
                "name": "few", "tool": "memory", "because": "Enough.", "max_calls": 3
            }))
            .unwrap(),
        );
        edit.skills.push(
            serde_json::from_value(serde_json::json!({
                "name": "cite", "when": "sources", "instructions": "Cite them."
            }))
            .unwrap(),
        );
        assert_eq!(check_harness(&edit, &current(), TASK), Ok(()));
    }

    #[test]
    fn a_candidate_may_not_drop_or_loosen_a_guardrail() {
        let mut dropped = current();
        dropped.guardrails.clear();
        assert!(
            check_harness(&dropped, &current(), TASK)
                .unwrap_err()
                .contains("look")
        );
        let mut loosened = current();
        loosened.guardrails[0].required = false;
        loosened.guardrails[0].max_calls = Some(1000);
        assert!(check_harness(&loosened, &current(), TASK).is_err());
    }

    #[test]
    fn skills_are_held_to_growth_and_leaks() {
        let skill = |instructions: String| {
            let mut edit = current();
            edit.skills.push(workflow::Skill {
                name: "s".to_owned(),
                when: "board".to_owned(),
                unless: None,
                instructions,
            });
            check_harness(&edit, &current(), TASK)
        };
        assert!(
            skill("x".repeat(MAX_SKILL_GROWTH_CHARS + 1))
                .unwrap_err()
                .contains("grew")
        );
        assert!(
            skill(format!("Do this: {TASK}"))
                .unwrap_err()
                .contains("task")
        );
        assert!(skill("===== BEGIN RUN R".to_owned()).is_err());
        assert_eq!(skill("Be brief.".to_owned()), Ok(()));
    }

    #[test]
    fn a_short_task_is_not_looked_for() {
        assert_eq!(
            check_instructions("You help with math.", "You help.", "math"),
            Ok(())
        );
    }
}
