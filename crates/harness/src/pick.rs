//! What to improve: an agent whose real runs went wrong the same way more
//! than once. One failure is noise; the same failure twice is a pattern worth
//! an attempt.

use dispatch::{CallStatus, Outcome, Speaker, Step, Trajectory};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::attempt::Request;

/// How many runs must fail the same way.
pub const MIN_REPEATS: usize = 2;
/// The longest task taken from a run.
const MAX_TASK_CHARS: usize = 8000;

fn word(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// How a run went wrong, in a few words, or `None` if it didn't. Runs that
/// went wrong the same way have the same words.
#[must_use]
pub fn failure(trajectory: &Trajectory) -> Option<String> {
    for timed in &trajectory.steps {
        if let Step::Tool {
            tool,
            status,
            refusal,
            by_harness: false,
            ..
        } = &timed.step
        {
            match status {
                CallStatus::Failed | CallStatus::TimedOut => {
                    return Some(format!("{tool} {}", word(status)));
                }
                CallStatus::Refused => {
                    let why = refusal.map(word).unwrap_or_default();
                    return Some(format!("{tool} refused ({why})"));
                }
                CallStatus::Ran | CallStatus::Stubbed => {}
            }
        }
    }
    trajectory.steps.iter().find_map(|timed| match &timed.step {
        Step::Ended {
            outcome,
            budget_hit,
            ..
        } if *outcome != Outcome::Answered => Some(match budget_hit {
            Some(limit) => format!("ended {}, hit its {} limit", word(outcome), word(limit)),
            None => format!("ended {}", word(outcome)),
        }),
        _ => None,
    })
}

/// What the run was asked: its first message from the user's side.
#[must_use]
pub fn task(trajectory: &Trajectory) -> Option<String> {
    trajectory.steps.iter().find_map(|timed| match &timed.step {
        Step::Message {
            role: Speaker::User,
            content,
            ..
        } => Some(content.chars().take(MAX_TASK_CHARS).collect()),
        _ => None,
    })
}

/// An agent's recent real runs, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recent {
    /// The agent.
    pub agent: String,
    /// Its runs.
    pub runs: Vec<Trajectory>,
}

/// The agent picked, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    /// What to attempt.
    pub request: Request,
    /// How the runs went wrong.
    #[schemars(length(max = 512))]
    pub failure: String,
    /// How many recent runs went wrong that way.
    pub repeats: u32,
    /// How many recent runs were looked at.
    pub of: u32,
}

/// The agent whose runs failed the same way most often, at least
/// [`MIN_REPEATS`] times, with the newest of those runs as the task. Ties go
/// to the agent listed first.
#[must_use]
pub fn pick(agents: &[Recent]) -> Option<Pick> {
    let mut best: Option<Pick> = None;
    for recent in agents {
        let failures: Vec<(String, &Trajectory)> = recent
            .runs
            .iter()
            .filter_map(|run| failure(run).map(|why| (why, run)))
            .collect();
        for (why, newest) in &failures {
            let repeats = failures.iter().filter(|(other, _)| other == why).count();
            if repeats < MIN_REPEATS || best.as_ref().is_some_and(|b| b.repeats as usize >= repeats)
            {
                continue;
            }
            let Some(task) = task(newest) else { continue };
            let of = recent.runs.len();
            best = Some(Pick {
                request: Request {
                    agent: recent.agent.clone(),
                    task,
                    target: crate::Target::Agent,
                    why: Some(format!(
                        "{repeats} of its last {of} runs went wrong the same way: {why}. \
                         The newest went: {}",
                        newest.calls
                    )),
                },
                failure: why.clone(),
                repeats: u32::try_from(repeats).unwrap_or(u32::MAX),
                of: u32::try_from(of).unwrap_or(u32::MAX),
            });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use dispatch::{Budget, TimedStep};

    fn timed(step: Step) -> TimedStep {
        TimedStep {
            at: DateTime::UNIX_EPOCH,
            step,
        }
    }

    fn run(question: &str, tool: Option<CallStatus>, outcome: Outcome) -> Trajectory {
        let mut steps = vec![
            timed(Step::Started {
                agent: "agent:helper".to_owned(),
                harness: "h".to_owned(),
                purpose: "workflow".to_owned(),
                offered: vec![],
                budget: Budget {
                    max_rounds: 1,
                    max_tool_calls: 1,
                    max_writes: 0,
                    max_total_tokens: 10,
                    max_usd_micros: 0,
                    max_request_bytes: 10,
                    max_duration_ms: 10,
                },
                skills: vec![],
            }),
            timed(Step::Message {
                role: Speaker::User,
                content: question.to_owned(),
                tool_calls: vec![],
            }),
        ];
        if let Some(status) = tool {
            steps.push(timed(Step::Tool {
                index: 0,
                tool: "memory".to_owned(),
                status,
                refusal: None,
                effect: None,
                by_harness: false,
                arguments: "{}".to_owned(),
                reply: None,
                duration_ms: 0,
            }));
        }
        steps.push(timed(Step::Ended {
            outcome,
            budget_hit: None,
            cost_usd_micros: 0,
        }));
        Trajectory {
            run: "r".to_owned(),
            agent: Some("agent:helper".to_owned()),
            harness: Some("h".to_owned()),
            calls: "calls".to_owned(),
            steps,
            unreadable: 0,
            complete: true,
        }
    }

    fn recent(agent: &str, runs: Vec<Trajectory>) -> Recent {
        Recent {
            agent: agent.to_owned(),
            runs,
        }
    }

    #[test]
    fn a_failure_is_named_by_what_went_wrong() {
        let ok = run("q", Some(CallStatus::Ran), Outcome::Answered);
        assert_eq!(failure(&ok), None);
        let failed = run("q", Some(CallStatus::Failed), Outcome::Answered);
        assert_eq!(failure(&failed).as_deref(), Some("memory failed"));
        let silent = run("q", None, Outcome::ModelFailed);
        assert_eq!(failure(&silent).as_deref(), Some("ended model_failed"));
    }

    #[test]
    fn one_failure_is_noise() {
        let agents = [recent(
            "helper",
            vec![
                run("a", None, Outcome::ModelFailed),
                run("b", None, Outcome::Answered),
            ],
        )];
        assert_eq!(pick(&agents), None);
    }

    #[test]
    fn the_same_failure_twice_is_picked_with_the_newest_task() {
        let agents = [
            recent(
                "quiet",
                vec![
                    run("x", None, Outcome::ModelFailed),
                    run("y", Some(CallStatus::Failed), Outcome::Answered),
                ],
            ),
            recent(
                "helper",
                vec![
                    run("newest", Some(CallStatus::Failed), Outcome::Answered),
                    run("ok", None, Outcome::Answered),
                    run("older", Some(CallStatus::Failed), Outcome::Answered),
                ],
            ),
        ];
        let picked = pick(&agents).unwrap();
        assert_eq!(picked.request.agent, "helper");
        assert_eq!(picked.request.task, "newest");
        assert_eq!(picked.failure, "memory failed");
        assert_eq!((picked.repeats, picked.of), (2, 3));
        assert!(
            picked
                .request
                .why
                .unwrap()
                .starts_with("2 of its last 3 runs")
        );
    }

    #[test]
    fn the_most_repeated_failure_wins() {
        let failing = |n: usize| -> Vec<Trajectory> {
            (0..n)
                .map(|_| run("q", None, Outcome::ModelFailed))
                .collect()
        };
        let agents = [recent("two", failing(2)), recent("three", failing(3))];
        assert_eq!(pick(&agents).unwrap().request.agent, "three");
    }
}
