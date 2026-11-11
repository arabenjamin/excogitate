//! One improvement attempt: probe the agent as it is, have the editor write
//! candidates, probe them, have the composer compare the runs, and check what
//! it cites. The attempt records everything, including what the code refused.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use dispatch::{Trajectory, read_trajectory};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workflow::{Agent, Engine, ProbeRun};

use crate::cite::Cite;
use crate::guard::check_instructions;
use crate::render::Rendered;
use crate::roles::{Brief, ComposerReply, Decision, Effect, Judges, Verdict};

/// How many steps of a probe are read back for the judges.
const MAX_STEPS: usize = 512;
/// The most problems an attempt keeps.
const MAX_PROBLEMS: usize = 32;

/// What to improve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The agent.
    #[schemars(length(min = 1, max = 64))]
    pub agent: String,
    /// A task to run it on, such as one a real run got wrong.
    #[schemars(length(min = 1, max = 8000))]
    pub task: String,
    /// What went wrong, if someone said.
    #[schemars(length(max = 2000))]
    pub why: Option<String>,
}

/// How the loop runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The read tools a probe really runs.
    pub reads: BTreeSet<String>,
    /// What the judges may spend on one attempt, in millionths of a dollar.
    pub judge_usd_micros: u64,
}

/// What a candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// How the agent works.
    Invariant,
    /// What it knows.
    Domain,
    /// Two candidates combined.
    Merged,
}

/// A probe, in brief. Its trajectory has the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Probed {
    /// The run.
    #[schemars(length(max = 36))]
    pub run: String,
    /// The harness it ran under.
    #[schemars(length(max = 64))]
    pub harness: String,
    /// How it ended.
    #[schemars(length(max = 32))]
    pub outcome: Option<String>,
    /// Its calls line.
    #[schemars(length(max = 4200))]
    pub calls: String,
}

/// One candidate and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// Its run label.
    #[schemars(length(max = 4))]
    pub label: String,
    /// What it is.
    pub kind: Kind,
    /// Its instructions.
    #[schemars(length(max = 16_000))]
    pub system_prompt: String,
    /// What changed, as the editor put it.
    #[schemars(length(max = 1000))]
    pub change: String,
    /// The editor's evidence.
    #[schemars(length(max = 8))]
    pub cites: Vec<Cite>,
    /// Why it wasn't tried, if it wasn't.
    #[schemars(length(max = 2048))]
    pub refused: Option<String>,
    /// Its probe, if it was tried.
    pub probe: Option<Probed>,
}

/// A verdict and whether its evidence held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Checked {
    /// What the composer said.
    pub verdict: Verdict,
    /// Why the evidence didn't hold, if it didn't. Such a verdict counts for
    /// nothing.
    #[schemars(length(max = 2048))]
    pub refused: Option<String>,
    /// Set when the code overrode the verdict, such as a run that ended worse
    /// than the reference whatever the composer said.
    pub overridden: Option<Effect>,
}

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
// Inline, so the store can see it's a string it can filter on.
#[schemars(inline)]
pub enum Status {
    /// The evidence supports no change.
    Kept,
    /// The evidence supports a change.
    Supported,
    /// The loop couldn't finish.
    Failed,
}

/// One attempt to improve an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    /// What was asked.
    pub request: Request,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it ended.
    pub finished_at: DateTime<Utc>,
    /// The agent's harness when it started.
    #[schemars(length(max = 64))]
    pub base_harness: Option<String>,
    /// The agent as it is, on the task.
    pub reference: Option<Probed>,
    /// What the editor proposed.
    #[schemars(length(max = 3))]
    pub candidates: Vec<Candidate>,
    /// What the composer found, checked.
    #[schemars(length(max = 8))]
    pub verdicts: Vec<Checked>,
    /// What the composer decided.
    pub decision: Option<Decision>,
    /// Why, as the composer put it.
    #[schemars(length(max = 1000))]
    pub reason: Option<String>,
    /// How it ended.
    pub status: Status,
    /// The candidate the evidence supports, for `supported`.
    #[schemars(length(max = 4))]
    pub chosen: Option<String>,
    /// What the code refused or what went wrong, in order.
    #[schemars(length(max = 32), inner(length(max = 2048)))]
    pub problems: Vec<String>,
    /// What the attempt cost: its probes and its judges.
    pub cost_usd_micros: u64,
}

impl Attempt {
    /// The supported candidate, if there is one.
    #[must_use]
    pub fn supported(&self) -> Option<&Candidate> {
        let chosen = self.chosen.as_ref()?;
        (self.status == Status::Supported)
            .then(|| self.candidates.iter().find(|c| c.label == *chosen))
            .flatten()
    }

    fn problem(&mut self, text: impl Into<String>) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(schema::bounded(&text.into()));
        }
    }

    fn spend(&mut self, cost: u64) {
        self.cost_usd_micros = self.cost_usd_micros.saturating_add(cost);
    }

    fn end(mut self, status: Status) -> Self {
        self.status = status;
        self.finished_at = Utc::now();
        self
    }
}

/// What the loop runs with.
#[derive(Debug, Clone)]
pub struct Loop<'a> {
    /// Runs the probes.
    pub engine: &'a Engine,
    /// The editor and composer.
    pub judges: Judges<'a>,
    /// How it runs.
    pub settings: &'a Settings,
}

fn rank(outcome: Option<&str>) -> u8 {
    u8::from(outcome == Some("answered"))
}

impl Loop<'_> {
    async fn probe(
        &self,
        name: &str,
        agent: &Agent,
        task: &str,
        attempt: &mut Attempt,
    ) -> Option<(ProbeRun, Trajectory)> {
        let probed = match self
            .engine
            .probe_agent(name, agent, task, self.settings.reads.clone())
            .await
        {
            Ok(probed) => probed,
            Err(why) => {
                attempt.problem(format!("a probe couldn't run: {why}"));
                return None;
            }
        };
        attempt.spend(probed.receipt.totals.cost_usd_micros);
        let run = probed.run.parse().ok()?;
        if let Ok(Some(trajectory)) =
            read_trajectory(self.engine.sink.as_ref(), run, MAX_STEPS).await
        {
            return Some((probed, trajectory));
        }
        attempt.problem("a probe's trajectory couldn't be read");
        None
    }

    fn judge_money(&self, attempt: &Attempt) -> u64 {
        self.settings
            .judge_usd_micros
            .saturating_sub(attempt.cost_usd_micros)
    }

    /// Runs one attempt. It always returns one, and a failure says why.
    pub async fn improve(&self, request: Request) -> Attempt {
        let mut attempt = Attempt {
            request: request.clone(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            base_harness: None,
            reference: None,
            candidates: Vec::new(),
            verdicts: Vec::new(),
            decision: None,
            reason: None,
            status: Status::Failed,
            chosen: None,
            problems: Vec::new(),
            cost_usd_micros: 0,
        };
        let agent = match self.engine.agent(&request.agent).await {
            Ok(Some(agent)) => agent,
            Ok(None) => {
                attempt.problem(format!("there is no agent {}", request.agent));
                return attempt.end(Status::Failed);
            }
            Err(error) => {
                attempt.problem(error.to_string());
                return attempt.end(Status::Failed);
            }
        };
        attempt.base_harness = Some(self.engine.agent_harness(&agent));
        let Some((probed, trajectory)) = self
            .probe(&request.agent, &agent, &request.task, &mut attempt)
            .await
        else {
            return attempt.end(Status::Failed);
        };
        let reference = Rendered::new("R", &trajectory);
        attempt.reference = Some(summary(&probed, &reference));
        let brief = Brief {
            instructions: &agent.system_prompt,
            tools: &agent.tools,
            task: &request.task,
            why: request.why.as_deref(),
            lessons: &[],
        };
        let money = self.judge_money(&attempt);
        let edited = match self.judges.edit(&brief, &reference, money).await {
            Ok(edited) => edited,
            Err(error) => {
                attempt.problem(format!("the editor: {error}"));
                return attempt.end(Status::Failed);
            }
        };
        attempt.spend(edited.cost_usd_micros);

        let mut runs = vec![reference.clone()];
        for (label, kind, draft) in [
            ("A", Kind::Invariant, edited.value.invariant),
            ("B", Kind::Domain, edited.value.domain),
        ] {
            let mut candidate = Candidate {
                label: label.to_owned(),
                kind,
                system_prompt: draft.system_prompt,
                change: draft.change,
                cites: draft.cites,
                refused: None,
                probe: None,
            };
            candidate.refused = Self::refusal(&candidate, &agent, &request.task, &reference);
            if candidate.refused.is_none() {
                let edit = Agent {
                    system_prompt: candidate.system_prompt.clone(),
                    ..agent.clone()
                };
                if let Some((probed, trajectory)) = self
                    .probe(&request.agent, &edit, &request.task, &mut attempt)
                    .await
                {
                    let rendered = Rendered::new(label, &trajectory);
                    candidate.probe = Some(summary(&probed, &rendered));
                    runs.push(rendered);
                }
            }
            attempt.candidates.push(candidate);
        }
        if runs.len() == 1 {
            attempt.problem("no candidate could be tried");
            return attempt.end(Status::Kept);
        }
        self.decide(attempt, &agent, &brief, runs).await
    }

    /// Why a candidate can't be tried, if it can't.
    fn refusal(
        candidate: &Candidate,
        agent: &Agent,
        task: &str,
        reference: &Rendered,
    ) -> Option<String> {
        if candidate.system_prompt.trim() == agent.system_prompt.trim() {
            return Some("it changes nothing".to_owned());
        }
        if let Err(why) = check_instructions(&candidate.system_prompt, &agent.system_prompt, task) {
            return Some(why);
        }
        // Every edit traces to a moment in the reference run.
        if candidate.cites.is_empty() {
            return Some("it cites no step of run R".to_owned());
        }
        candidate
            .cites
            .iter()
            .find_map(|cite| cite.check(&[reference]).err())
            .map(|why| format!("its evidence: {why}"))
    }

    async fn decide(
        &self,
        mut attempt: Attempt,
        agent: &Agent,
        brief: &Brief<'_>,
        runs: Vec<Rendered>,
    ) -> Attempt {
        let tried: Vec<(String, String)> = attempt
            .candidates
            .iter()
            .filter(|c| c.probe.is_some())
            .map(|c| (c.label.clone(), c.system_prompt.clone()))
            .collect();
        let Some(reply) = self.compose(&mut attempt, brief, &runs, &tried).await else {
            return attempt.end(Status::Failed);
        };
        attempt.decision = Some(reply.decision);
        attempt.reason = Some(reply.reason.clone());
        match reply.decision {
            Decision::Keep => attempt.end(Status::Kept),
            Decision::Adopt => {
                let chosen = reply.choose.clone().unwrap_or_default();
                if mitigated(&attempt, &chosen) {
                    attempt.chosen = Some(chosen);
                    attempt.end(Status::Supported)
                } else {
                    attempt.problem(format!(
                        "the composer chose {chosen}, but no checked evidence shows it mitigated anything without regressing"
                    ));
                    attempt.end(Status::Kept)
                }
            }
            Decision::Merge => self.merge(attempt, agent, brief, &runs, reply).await,
        }
    }

    async fn compose(
        &self,
        attempt: &mut Attempt,
        brief: &Brief<'_>,
        runs: &[Rendered],
        tried: &[(String, String)],
    ) -> Option<ComposerReply> {
        let shown: Vec<&Rendered> = runs.iter().collect();
        let candidates: Vec<(&str, &str)> = tried
            .iter()
            .map(|(l, p)| (l.as_str(), p.as_str()))
            .collect();
        let money = self.judge_money(attempt);
        let reply = match self.judges.compose(brief, &shown, &candidates, money).await {
            Ok(reply) => reply,
            Err(error) => {
                attempt.problem(format!("the composer: {error}"));
                return None;
            }
        };
        attempt.spend(reply.cost_usd_micros);
        let reference = runs.first().map(|run| rank(run.outcome.as_deref()));
        for verdict in &reply.value.verdicts {
            let refused = if tried.iter().any(|(label, _)| *label == verdict.candidate) {
                verdict.cite.check(&shown).err()
            } else {
                Some(format!("there is no candidate {}", verdict.candidate))
            };
            let worse = runs
                .iter()
                .find(|run| run.label == verdict.candidate)
                .is_some_and(|run| Some(rank(run.outcome.as_deref())) < reference);
            let overridden =
                (worse && verdict.effect != Effect::Regressed).then_some(Effect::Regressed);
            attempt.verdicts.push(Checked {
                verdict: verdict.clone(),
                refused,
                overridden,
            });
        }
        Some(reply.value)
    }

    async fn merge(
        &self,
        mut attempt: Attempt,
        agent: &Agent,
        brief: &Brief<'_>,
        runs: &[Rendered],
        reply: ComposerReply,
    ) -> Attempt {
        let tried: Vec<String> = attempt
            .candidates
            .iter()
            .filter(|c| c.probe.is_some())
            .map(|c| c.label.clone())
            .collect();
        if tried.len() < 2 || !tried.iter().all(|label| mitigated(&attempt, label)) {
            attempt.problem("a merge needs two candidates that each mitigated something");
            return attempt.end(Status::Kept);
        }
        let merged = reply.merged.unwrap_or_default();
        let mut candidate = Candidate {
            label: "M".to_owned(),
            kind: Kind::Merged,
            system_prompt: merged,
            change: reply.reason,
            cites: Vec::new(),
            refused: None,
            probe: None,
        };
        candidate.refused = check_instructions(
            &candidate.system_prompt,
            &agent.system_prompt,
            &attempt.request.task,
        )
        .err();
        if candidate.refused.is_some() {
            attempt.candidates.push(candidate);
            return attempt.end(Status::Kept);
        }
        let edit = Agent {
            system_prompt: candidate.system_prompt.clone(),
            ..agent.clone()
        };
        let Request {
            agent: name, task, ..
        } = attempt.request.clone();
        let Some((probed, trajectory)) = self.probe(&name, &edit, &task, &mut attempt).await else {
            attempt.candidates.push(candidate);
            return attempt.end(Status::Failed);
        };
        let rendered = Rendered::new("M", &trajectory);
        candidate.probe = Some(summary(&probed, &rendered));
        let prompt = candidate.system_prompt.clone();
        attempt.candidates.push(candidate);
        // The merge is judged on its own run, against the reference.
        let pair: Vec<Rendered> = runs.iter().take(1).cloned().chain([rendered]).collect();
        let tried = [("M".to_owned(), prompt)];
        let Some(second) = self.compose(&mut attempt, brief, &pair, &tried).await else {
            return attempt.end(Status::Failed);
        };
        if second.decision == Decision::Adopt
            && second.choose.as_deref() == Some("M")
            && mitigated(&attempt, "M")
        {
            attempt.chosen = Some("M".to_owned());
            attempt.end(Status::Supported)
        } else {
            attempt.problem("the merge, run on its own, didn't hold up");
            attempt.end(Status::Kept)
        }
    }
}

/// True when `label`'s checked verdicts show it mitigated and none show it
/// regressed.
fn mitigated(attempt: &Attempt, label: &str) -> bool {
    let counted: Vec<Effect> = attempt
        .verdicts
        .iter()
        .filter(|checked| checked.verdict.candidate == label)
        .filter(|checked| checked.refused.is_none() || checked.overridden.is_some())
        .map(|checked| checked.overridden.unwrap_or(checked.verdict.effect))
        .collect();
    counted.contains(&Effect::Mitigated) && !counted.contains(&Effect::Regressed)
}

fn summary(probed: &ProbeRun, rendered: &Rendered) -> Probed {
    Probed {
        run: probed.run.clone(),
        harness: probed.harness.clone(),
        outcome: rendered.outcome.clone(),
        calls: rendered.calls.clone(),
    }
}
