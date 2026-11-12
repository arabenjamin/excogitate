//! The judges' known-answer test. Before a composer's decision may change an
//! agent, the model it ran on must judge three cases whose answers are known:
//! one candidate that fixed the run, one that broke it, and one that changed
//! nothing. Any wrong answer, or any citation that doesn't hold, fails it.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::render::Rendered;
use crate::roles::{Brief, Decision, Effect, Judges};

const INSTRUCTIONS: &str = "You answer questions.";
const BETTER: &str = "You answer questions. Work the answer out before you reply.";
const TASK: &str = "What is six times seven?";

/// One known-answer case and how the judge did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    /// The case.
    #[schemars(length(max = 32))]
    pub case: String,
    /// True if the judge got it right.
    pub passed: bool,
    /// What it got wrong, if anything.
    #[schemars(length(max = 2048))]
    pub detail: Option<String>,
}

/// A judge model's known-answer test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    /// The model that judged. Empty if no case got an answer.
    #[schemars(length(max = 256))]
    pub model: String,
    /// When.
    pub at: DateTime<Utc>,
    /// True only if every case passed.
    pub passed: bool,
    /// Each case.
    #[schemars(length(max = 8))]
    pub cases: Vec<CaseResult>,
    /// What the test cost.
    pub cost_usd_micros: u64,
}

fn run(label: &str, instructions: &str, said: &str, ended: &str) -> Rendered {
    Rendered {
        label: label.to_owned(),
        outcome: Some(ended.to_owned()),
        calls: ended.to_owned(),
        steps: vec![
            "started, offered tools: ".to_owned(),
            format!("instructions: {instructions}"),
            format!("task: {TASK}"),
            said.to_owned(),
            format!("ended: {ended}"),
        ],
        behavior: vec![false, false, false, true, true],
        cut: false,
    }
}

fn answer(text: &str) -> String {
    format!("model said: {text} (asked for 0 tool calls)")
}

struct Case {
    name: &'static str,
    reference: Rendered,
    candidate: Rendered,
    effect: Effect,
    decision: Decision,
}

fn cases() -> Vec<Case> {
    let right = answer("Six times seven is 42.");
    vec![
        Case {
            name: "fixed",
            reference: run("R", INSTRUCTIONS, &answer("I don't know."), "answered"),
            candidate: run("A", BETTER, &right, "answered"),
            effect: Effect::Mitigated,
            decision: Decision::Adopt,
        },
        Case {
            name: "broke",
            reference: run("R", INSTRUCTIONS, &right, "answered"),
            candidate: run(
                "A",
                BETTER,
                "model call failed: the model's reply was empty",
                "model_failed",
            ),
            effect: Effect::Regressed,
            decision: Decision::Keep,
        },
        Case {
            name: "same",
            reference: run("R", INSTRUCTIONS, &right, "answered"),
            candidate: run("A", BETTER, &right, "answered"),
            effect: Effect::Unchanged,
            decision: Decision::Keep,
        },
    ]
}

impl Judges<'_> {
    /// Runs the known-answer test on `model`, the composer's: every case is
    /// pinned to it, so a fallback elsewhere can't stand in for it.
    pub async fn calibrate(&self, model: &str, budget_usd_micros: u64) -> Calibration {
        let brief = Brief {
            instructions: INSTRUCTIONS,
            tools: &[],
            skills: &[],
            guardrails: &[],
            task: TASK,
            why: None,
            lessons: &[],
        };
        let mut calibration = Calibration {
            model: String::new(),
            at: Utc::now(),
            passed: false,
            cases: Vec::new(),
            cost_usd_micros: 0,
        };
        for case in cases() {
            let left = budget_usd_micros.saturating_sub(calibration.cost_usd_micros);
            let runs = [&case.reference, &case.candidate];
            let judged = self
                .compose_on(&brief, &runs, &[("A", BETTER)], left, Some(model))
                .await;
            let detail = match judged {
                Err(error) => {
                    calibration.cost_usd_micros = calibration
                        .cost_usd_micros
                        .saturating_add(error.cost_usd_micros());
                    Some(error.to_string())
                }
                Ok(judged) => {
                    calibration.cost_usd_micros = calibration
                        .cost_usd_micros
                        .saturating_add(judged.cost_usd_micros);
                    if judged.model == model {
                        calibration.model.clone_from(&judged.model);
                        check(&case, &judged.value, &runs)
                    } else {
                        Some(format!("answered by {}, not {model}", judged.model))
                    }
                }
            };
            calibration.cases.push(CaseResult {
                case: case.name.to_owned(),
                passed: detail.is_none(),
                detail: detail.map(|text| schema::bounded(&text)),
            });
        }
        calibration.passed = calibration.cases.iter().all(|case| case.passed);
        calibration
    }
}

fn check(case: &Case, reply: &crate::roles::ComposerReply, runs: &[&Rendered]) -> Option<String> {
    let Some(verdict) = reply.verdicts.iter().find(|v| v.candidate == "A") else {
        return Some("no verdict on A".to_owned());
    };
    if verdict.effect != case.effect {
        return Some(format!(
            "said {:?}, the answer is {:?}",
            verdict.effect, case.effect
        ));
    }
    if let Err(why) = verdict.cite.check(runs) {
        return Some(format!("its evidence: {why}"));
    }
    if reply.decision != case.decision {
        return Some(format!(
            "decided {:?}, the answer is {:?}",
            reply.decision, case.decision
        ));
    }
    None
}
