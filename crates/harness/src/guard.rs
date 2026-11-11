//! What a candidate's instructions may not be, whatever a judge says. These
//! are checked in code, because each is a way judges have gone wrong in
//! practice.

/// Characters a candidate may add to the instructions it replaces. An agent's
/// prompt is paid on every run, and a small model's prompt is a size budget.
pub const MAX_GROWTH_CHARS: usize = 2000;
/// A task shorter than this isn't checked for, since it could appear by chance.
const MIN_TASK_CHARS: usize = 24;

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
    // A judge that pastes its own input into the agent leaks the test into
    // every later run.
    if candidate.contains("===== BEGIN") || candidate.contains("===== END") {
        return Err("the instructions contain the judge's input".to_owned());
    }
    let task = squeeze(task);
    if task.chars().count() >= MIN_TASK_CHARS && squeeze(candidate).contains(&task) {
        return Err("the instructions contain the task they were tested on".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn a_short_task_is_not_looked_for() {
        assert_eq!(
            check_instructions("You help with math.", "You help.", "math"),
            Ok(())
        );
    }
}
