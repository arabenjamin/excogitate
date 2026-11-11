//! Citations: a judge's evidence must be a quote from a step it was shown.
//! A quote that isn't there is a claim, and a claim is never evidence.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::render::Rendered;

/// The shortest quote accepted, in characters. Shorter ones match by chance.
const MIN_QUOTE_CHARS: usize = 8;

/// A step in a run, and the words in it that show something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cite {
    /// The run's label, such as `R`.
    #[schemars(length(min = 1, max = 4))]
    pub run: String,
    /// The step's number in that run.
    #[schemars(range(max = 4096))]
    pub step: u32,
    /// Words copied exactly from that step.
    #[schemars(length(min = 1, max = 300))]
    pub quote: String,
}

fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Cite {
    /// Checks that the quote is in the step it names.
    ///
    /// # Errors
    ///
    /// Why it isn't evidence, in a sentence.
    pub fn check(&self, runs: &[&Rendered]) -> Result<(), String> {
        let label = &self.run;
        let run = runs
            .iter()
            .find(|run| run.label == *label)
            .ok_or_else(|| format!("there is no run {label}"))?;
        let number = usize::try_from(self.step).unwrap_or(usize::MAX);
        let step = run
            .steps
            .get(number)
            .ok_or_else(|| format!("run {label} has no step {}", self.step))?;
        if run.behavior.get(number) != Some(&true) {
            return Err(format!(
                "step {label}{} is what the run was given, not what it did",
                self.step
            ));
        }
        let quote = squeeze(&self.quote);
        if quote.chars().count() < MIN_QUOTE_CHARS {
            return Err(format!(
                "a quote must be at least {MIN_QUOTE_CHARS} characters"
            ));
        }
        if squeeze(step).contains(&quote) {
            Ok(())
        } else {
            Err(format!("step {label}{} doesn't say that", self.step))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Rendered {
        Rendered {
            label: "R".to_owned(),
            outcome: Some("answered".to_owned()),
            calls: "answered".to_owned(),
            steps: vec![
                "instructions: You help.".to_owned(),
                "model said: I   don't\nknow. (asked for 0 tool calls)".to_owned(),
            ],
            behavior: vec![false, true],
            cut: false,
        }
    }

    fn cite(step: u32, quote: &str) -> Cite {
        Cite {
            run: "R".to_owned(),
            step,
            quote: quote.to_owned(),
        }
    }

    #[test]
    fn a_quote_matches_whatever_the_spacing() {
        assert_eq!(cite(1, "I don't know.").check(&[&run()]), Ok(()));
    }

    #[test]
    fn only_a_real_quote_of_what_the_run_did_is_evidence() {
        let shown = run();
        for (bad, why) in [
            (cite(1, "I know it."), "doesn't say that"),
            (cite(9, "I don't know."), "has no step 9"),
            (cite(0, "You help."), "not what it did"),
            (cite(1, "know."), "at least 8"),
            (
                Cite {
                    run: "Z".to_owned(),
                    ..cite(1, "I don't know.")
                },
                "no run Z",
            ),
        ] {
            let error = bad.check(&[&shown]).unwrap_err();
            assert!(error.contains(why), "{error}");
        }
    }
}
