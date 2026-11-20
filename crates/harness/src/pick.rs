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

/// The failure a run the owner marked bad counts as.
pub const MARKED_BAD: &str = "the owner marked it bad";

/// One run that went wrong for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// How, in the words runs that went wrong the same way share.
    pub failure: String,
    /// What the run was asked.
    pub task: String,
    /// The run's calls line.
    pub calls: String,
    /// The owner's note, when the owner marked the run bad.
    pub note: Option<String>,
}

/// What recent runs say about one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// The agent.
    pub agent: String,
    /// The runs that went wrong, newest first, one finding per run and way.
    pub findings: Vec<Finding>,
    /// How many runs were looked at.
    pub of: usize,
    /// True when the runs were routed chats, where the agent was a profile or
    /// the writer.
    pub from_chats: bool,
}

/// The owner's mark on a run, as the picker reads it: bad, with any note.
pub type Marked = std::collections::BTreeMap<String, Option<String>>;
/// A chat's checked review that found a fault, as the picker reads it: the
/// agent it's charged to, and the failure.
pub type Reviewed = std::collections::BTreeMap<String, (String, String)>;

fn finding(run: &Trajectory, failure: String, note: Option<String>) -> Option<Finding> {
    Some(Finding {
        failure,
        task: task(run)?,
        calls: run.calls.clone(),
        note,
    })
}

/// An agent's own runs as evidence: each run's failure, and each run the
/// owner marked bad.
#[must_use]
pub fn evidence(recent: &Recent, marked: &Marked) -> Evidence {
    let mut findings = Vec::new();
    for run in &recent.runs {
        if let Some(note) = marked.get(&run.run) {
            findings.extend(finding(run, MARKED_BAD.to_owned(), note.clone()));
        }
        if let Some(why) = failure(run) {
            findings.extend(finding(run, why, None));
        }
    }
    Evidence {
        agent: recent.agent.clone(),
        findings,
        of: recent.runs.len(),
        from_chats: false,
    }
}

/// What went wrong in one routed chat, charged to whoever it belongs to: a
/// call, its arguments or a search to the profile it was made under, and a
/// chat that ended without an answer to `writer`. Each agent and failure
/// once. The second list is every agent the chat involved.
#[must_use]
pub fn chat_failures(chat: &Trajectory, writer: &str) -> (Vec<(String, String)>, Vec<String>) {
    let mut charged: Vec<(String, String)> = Vec::new();
    let mut involved = vec![writer.to_owned()];
    let mut profile: Option<String> = None;
    let mut charge = |agent: &str, why: String| {
        let entry = (agent.to_owned(), why);
        if !charged.contains(&entry) {
            charged.push(entry);
        }
    };
    for timed in &chat.steps {
        match &timed.step {
            Step::Choice {
                stage,
                picked,
                of,
                profile: under,
            } => {
                if stage == "profile" {
                    // A reply that named no profile looks nothing up.
                    profile = of.contains(picked).then(|| picked.clone());
                } else if under.is_some() {
                    profile.clone_from(under);
                }
                if let Some(name) = &profile
                    && !involved.contains(name)
                {
                    involved.push(name.clone());
                }
            }
            Step::Tool {
                tool,
                status,
                refusal,
                by_harness: false,
                ..
            } => {
                let why = match status {
                    CallStatus::Failed | CallStatus::TimedOut => {
                        Some(format!("{tool} {}", word(status)))
                    }
                    CallStatus::Refused => Some(format!(
                        "{tool} refused ({})",
                        refusal.map(word).unwrap_or_default()
                    )),
                    CallStatus::Ran | CallStatus::Stubbed => None,
                };
                if let (Some(why), Some(name)) = (why, &profile) {
                    charge(name, why);
                }
            }
            Step::Arguments { tool, dropped, .. } => {
                if let Some(name) = &profile
                    && dropped.trim() != "{}"
                {
                    charge(name, format!("{tool} was given values the question didn't"));
                }
            }
            Step::Rule { rule, detail } => {
                let why = match rule.as_str() {
                    "no_repeat" => Some(format!(
                        "{} would have run twice the same way",
                        detail.split_whitespace().next().unwrap_or("a tool")
                    )),
                    "not_found" => Some(
                        "its memory searches found nothing that answers the question".to_owned(),
                    ),
                    _ => None,
                };
                if let (Some(why), Some(name)) = (why, &profile) {
                    charge(name, why);
                }
            }
            Step::Ended {
                outcome,
                budget_hit,
                ..
            } if *outcome != Outcome::Answered => charge(
                writer,
                match budget_hit {
                    Some(limit) => {
                        format!("ended {}, hit its {} limit", word(outcome), word(limit))
                    }
                    None => format!("ended {}", word(outcome)),
                },
            ),
            _ => {}
        }
    }
    (charged, involved)
}

/// The profile a routed chat was working under at step `index`, if any.
#[must_use]
pub fn profile_at(chat: &Trajectory, index: usize) -> Option<String> {
    let mut profile: Option<String> = None;
    for timed in chat.steps.iter().take(index.saturating_add(1)) {
        if let Step::Choice {
            stage,
            picked,
            of,
            profile: under,
        } = &timed.step
        {
            if stage == "profile" {
                profile = of.contains(picked).then(|| picked.clone());
            } else if under.is_some() {
                profile.clone_from(under);
            }
        }
    }
    profile
}

/// Routed chats, newest first, as evidence for each profile and the writer.
/// A chat the owner marked bad is charged to the writer, since it's the
/// answer the owner saw; a chat's checked review, to whom it names.
#[must_use]
pub fn chat_evidence(
    chats: &[Trajectory],
    writer: &str,
    marked: &Marked,
    reviewed: &Reviewed,
) -> Vec<Evidence> {
    let mut all: Vec<Evidence> = Vec::new();
    let of = |agent: &str, all: &mut Vec<Evidence>| -> usize {
        if let Some(at) = all.iter().position(|evidence| evidence.agent == agent) {
            return at;
        }
        all.push(Evidence {
            agent: agent.to_owned(),
            findings: Vec::new(),
            of: 0,
            from_chats: true,
        });
        all.len() - 1
    };
    for chat in chats {
        let (charged, involved) = chat_failures(chat, writer);
        for agent in &involved {
            let at = of(agent, &mut all);
            if let Some(evidence) = all.get_mut(at) {
                evidence.of += 1;
            }
        }
        let mut found: Vec<(String, Finding)> = charged
            .into_iter()
            .filter_map(|(agent, why)| finding(chat, why, None).map(|found| (agent, found)))
            .collect();
        if let Some(note) = marked.get(&chat.run) {
            found.extend(
                finding(chat, MARKED_BAD.to_owned(), note.clone())
                    .map(|found| (writer.to_owned(), found)),
            );
        }
        if let Some((agent, why)) = reviewed.get(&chat.run) {
            found.extend(finding(chat, why.clone(), None).map(|found| (agent.clone(), found)));
        }
        for (agent, found) in found {
            let at = of(&agent, &mut all);
            if let Some(evidence) = all.get_mut(at) {
                evidence.findings.push(found);
            }
        }
    }
    all
}

/// The agent whose runs went wrong the same way most often, at least
/// [`MIN_REPEATS`] times, or once when the owner marked it bad, with the
/// newest of those runs as the task. Ties go to the agent listed first.
#[must_use]
pub fn best(evidence: &[Evidence]) -> Option<Pick> {
    let mut best: Option<Pick> = None;
    for agent in evidence {
        for newest in &agent.findings {
            let why = &newest.failure;
            let repeats = agent
                .findings
                .iter()
                .filter(|other| &other.failure == why)
                .count();
            let needed = if why == MARKED_BAD { 1 } else { MIN_REPEATS };
            if repeats < needed || best.as_ref().is_some_and(|b| b.repeats as usize >= repeats) {
                continue;
            }
            let runs = if agent.from_chats {
                "chats it took part in"
            } else {
                "runs"
            };
            let of = agent.of;
            let owner = newest
                .note
                .as_ref()
                .map(|note| format!(" The owner said: {note}."))
                .unwrap_or_default();
            let said = format!(
                "{repeats} of its last {of} {runs} went wrong the same way: {why}.{owner} \
                 The newest went: {}",
                newest.calls
            );
            best = Some(Pick {
                request: Request {
                    agent: agent.agent.clone(),
                    task: newest.task.clone(),
                    inputs: None,
                    target: crate::Target::Agent,
                    routed: agent.from_chats,
                    why: Some(said),
                },
                failure: why.clone(),
                repeats: u32::try_from(repeats).unwrap_or(u32::MAX),
                of: u32::try_from(of).unwrap_or(u32::MAX),
            });
        }
    }
    best
}

/// [`best`] over agents' own runs, with nothing marked by the owner.
#[must_use]
pub fn pick(agents: &[Recent]) -> Option<Pick> {
    let none = Marked::new();
    let evidence: Vec<Evidence> = agents
        .iter()
        .map(|recent| evidence(recent, &none))
        .collect();
    best(&evidence)
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
                requested_by: None,
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
            requested_by: None,
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

    fn routed(question: &str, steps: Vec<Step>, outcome: Outcome) -> Trajectory {
        let mut chat = run(question, None, outcome);
        chat.agent = Some("chat".to_owned());
        let ended = chat.steps.pop().unwrap();
        chat.steps.extend(steps.into_iter().map(timed));
        chat.steps.push(ended);
        chat
    }

    fn chose(stage: &str, picked: &str, profile: Option<&str>) -> Step {
        Step::Choice {
            stage: stage.to_owned(),
            picked: picked.to_owned(),
            of: vec!["todos".to_owned(), "notes".to_owned(), picked.to_owned()],
            profile: profile.map(str::to_owned),
        }
    }

    fn tool(name: &str, status: CallStatus) -> Step {
        Step::Tool {
            index: 0,
            tool: name.to_owned(),
            status,
            refusal: None,
            effect: None,
            by_harness: false,
            arguments: "{}".to_owned(),
            reply: None,
            duration_ms: 0,
        }
    }

    #[test]
    fn a_chat_charges_each_failure_to_the_profile_it_happened_under() {
        let chat = routed(
            "what's open, and what did I note?",
            vec![
                chose("profile", "todos", None),
                chose("tool", "todos_list", Some("todos")),
                tool("todos_list", CallStatus::Failed),
                chose("profile", "notes", None),
                Step::Arguments {
                    tool: "memory".to_owned(),
                    kept: "{}".to_owned(),
                    dropped: r#"{"tags":["x"]}"#.to_owned(),
                    refilled: false,
                },
                Step::Rule {
                    rule: "not_found".to_owned(),
                    detail: "a second search found nothing".to_owned(),
                },
            ],
            Outcome::ModelFailed,
        );
        let (charged, involved) = chat_failures(&chat, "writer");
        assert_eq!(
            charged,
            vec![
                ("todos".to_owned(), "todos_list failed".to_owned()),
                (
                    "notes".to_owned(),
                    "memory was given values the question didn't".to_owned()
                ),
                (
                    "notes".to_owned(),
                    "its memory searches found nothing that answers the question".to_owned()
                ),
                ("writer".to_owned(), "ended model_failed".to_owned()),
            ]
        );
        assert_eq!(involved, ["writer", "todos", "notes"]);
    }

    #[test]
    fn a_choice_that_named_no_profile_charges_nothing() {
        let chat = routed(
            "hi",
            vec![
                Step::Choice {
                    stage: "profile".to_owned(),
                    picked: "invalid".to_owned(),
                    of: vec!["todos".to_owned()],
                    profile: None,
                },
                tool("memory", CallStatus::Failed),
            ],
            Outcome::Answered,
        );
        assert_eq!(chat_failures(&chat, "writer").0, vec![]);
    }

    #[test]
    fn chats_that_fail_the_same_way_twice_pick_the_profile() {
        let failing = |question: &str| {
            routed(
                question,
                vec![
                    chose("profile", "todos", None),
                    tool("todos_list", CallStatus::Failed),
                ],
                Outcome::Answered,
            )
        };
        let fine = routed(
            "fine",
            vec![chose("profile", "notes", None)],
            Outcome::Answered,
        );
        let chats = [failing("newest"), fine, failing("older")];
        let evidence = chat_evidence(&chats, "writer", &Marked::new(), &Reviewed::new());
        let picked = best(&evidence).unwrap();
        assert_eq!(picked.request.agent, "todos");
        assert_eq!(picked.request.task, "newest");
        assert_eq!((picked.repeats, picked.of), (2, 2));
        assert!(
            picked
                .request
                .why
                .unwrap()
                .starts_with("2 of its last 2 chats it took part in")
        );
        let writer = evidence.iter().find(|e| e.agent == "writer").unwrap();
        assert_eq!(writer.of, 3);
    }

    #[test]
    fn a_run_the_owner_marked_bad_counts_on_its_own_with_the_note() {
        let chat = routed(
            "what's open?",
            vec![chose("profile", "todos", None)],
            Outcome::Answered,
        );
        let mut marked = Marked::new();
        marked.insert(
            chat.run.clone(),
            Some("it left out the in-progress one".to_owned()),
        );
        let picked = best(&chat_evidence(&[chat], "writer", &marked, &Reviewed::new())).unwrap();
        assert_eq!(picked.request.agent, "writer");
        assert_eq!(picked.failure, MARKED_BAD);
        assert!(
            picked
                .request
                .why
                .unwrap()
                .contains("The owner said: it left out the in-progress one.")
        );
        let own = recent("helper", vec![run("q", None, Outcome::Answered)]);
        let mut marked = Marked::new();
        marked.insert("r".to_owned(), None);
        let picked = best(&[evidence(&own, &marked)]).unwrap();
        assert_eq!(
            (picked.request.agent.as_str(), picked.repeats),
            ("helper", 1)
        );
    }
}
