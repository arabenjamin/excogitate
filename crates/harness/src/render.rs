//! A trajectory as a judge reads it: the calls line first, then numbered
//! steps. A citation names a step by its number, and is checked against this
//! same text, so what a judge quotes is what it was shown.

use std::fmt::Write as _;

use dispatch::{CallStatus, Speaker, Step, Trajectory};

/// How much of one step a judge sees, in characters.
const STEP_CHARS: usize = 1500;
/// How many steps a judge sees. Later ones are left out, and it says so.
const MAX_STEPS: usize = 64;

/// A run, labelled and numbered for a judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The label a judge uses for it, such as `R` or `A`.
    pub label: String,
    /// How it ended, in a word.
    pub outcome: Option<String>,
    /// The run's calls line.
    pub calls: String,
    /// Each step's text, by step number.
    pub steps: Vec<String>,
    /// For each step, whether it's something the run did, as opposed to what
    /// it was given. Only those are evidence.
    pub behavior: Vec<bool>,
    /// True if steps were left out.
    pub cut: bool,
}

fn cut(text: &str) -> String {
    let mut kept: String = text.chars().take(STEP_CHARS).collect();
    if text.chars().nth(STEP_CHARS).is_some() {
        kept.push_str("… (cut)");
    }
    kept
}

fn word(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn speaker(role: Speaker) -> &'static str {
    match role {
        Speaker::System => "instructions",
        Speaker::User => "task",
        Speaker::Assistant => "model",
        Speaker::Tool => "tool result",
    }
}

fn step_text(step: &Step) -> String {
    match step {
        Step::Started { offered, .. } => format!("started, offered tools: {}", offered.join(", ")),
        Step::Message { role, content, .. } => format!("{}: {}", speaker(*role), cut(content)),
        Step::Model {
            reply,
            tool_calls,
            error,
            ..
        } => match error {
            Some(error) => format!("model call failed: {}", cut(error)),
            None => format!(
                "model said: {} (asked for {tool_calls} tool calls)",
                cut(reply.as_deref().unwrap_or(""))
            ),
        },
        Step::Tool {
            tool,
            status,
            arguments,
            reply,
            by_harness,
            ..
        } => {
            let by = if *by_harness {
                ", made by the harness"
            } else {
                ""
            };
            if *status == CallStatus::Stubbed {
                return format!(
                    "{tool} STUBBED{by}: this is a test run, so it wasn't run on purpose. \
                     This is not a failure. Arguments: {}",
                    cut(arguments)
                );
            }
            format!(
                "{tool} {}{by}. Arguments: {}. Reply: {}",
                word(status),
                cut(arguments),
                cut(reply.as_deref().unwrap_or(""))
            )
        }
        Step::Capability {
            capability,
            what,
            ok,
            ..
        } => {
            let went = if *ok { "worked" } else { "failed" };
            format!("  the tool's {capability} call {went}: {}", cut(what))
        }
        Step::Ended {
            outcome,
            budget_hit,
            ..
        } => match budget_hit {
            Some(limit) => format!("ended: {}, hit its {} limit", word(outcome), word(limit)),
            None => format!("ended: {}", word(outcome)),
        },
    }
}

impl Rendered {
    /// Labels and numbers a trajectory.
    #[must_use]
    pub fn new(label: &str, trajectory: &Trajectory) -> Self {
        let outcome = trajectory.steps.iter().find_map(|timed| match &timed.step {
            Step::Ended { outcome, .. } => Some(word(outcome)),
            _ => None,
        });
        let shown = trajectory.steps.iter().take(MAX_STEPS);
        Self {
            label: label.to_owned(),
            outcome,
            calls: trajectory.calls.clone(),
            steps: shown.clone().map(|timed| step_text(&timed.step)).collect(),
            behavior: shown
                .map(|timed| {
                    matches!(
                        timed.step,
                        Step::Model { .. } | Step::Tool { .. } | Step::Ended { .. }
                    )
                })
                .collect(),
            cut: trajectory.steps.len() > MAX_STEPS || !trajectory.complete,
        }
    }

    /// The run as one delimited block.
    #[must_use]
    pub fn block(&self) -> String {
        let mut text = format!(
            "===== BEGIN RUN {label} =====\nCALLS: {calls}\n",
            label = self.label,
            calls = self.calls
        );
        for (number, step) in self.steps.iter().enumerate() {
            // Writing to a String can't fail.
            let _ = writeln!(text, "[{label}{number}] {step}", label = self.label);
        }
        if self.cut {
            text.push_str("(later steps not shown)\n");
        }
        let _ = writeln!(text, "===== END RUN {} =====", self.label);
        text
    }
}
