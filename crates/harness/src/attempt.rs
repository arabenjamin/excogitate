//! One improvement attempt: probe the agent as it is, have the editor write
//! candidates, probe them, have the composer compare the runs, and check what
//! it cites. The attempt records everything, including what the code refused.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use dispatch::{Guardrail, Trajectory, read_trajectory};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workflow::{Agent, Engine, ProbeRun, Skill, Workflow};

use crate::cite::Cite;
use crate::flow::{
    StepEdit, Target, apply_edits, check_edited_reads, check_workflow_edit, merge_edits,
};
use crate::guard::check_harness;
use crate::lesson::lessons_for;
use crate::render::Rendered;
use crate::roles::{Brief, ComposerReply, Decision, Draft, Effect, Judges, Verdict, listed};

/// How many steps of a probe are read back for the judges.
pub(crate) const MAX_STEPS: usize = 512;
/// The most problems an attempt keeps.
const MAX_PROBLEMS: usize = 32;

/// What to improve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The agent, or the workflow when `target` is `workflow`.
    #[schemars(length(min = 1, max = 64))]
    pub agent: String,
    /// A task to run it on, such as one a real run got wrong. For a
    /// workflow, the inputs to run it on, as a JSON object.
    #[schemars(length(min = 1, max = 8000))]
    pub task: String,
    /// What to improve: an agent unless it says otherwise.
    #[serde(default, skip_serializing_if = "Target::is_agent")]
    pub target: Target,

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
    /// The run. For a workflow, its first step's.
    #[schemars(length(max = 36))]
    pub run: String,
    /// For a workflow, each step's run, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64), inner(length(max = 36)))]
    pub runs: Vec<String>,
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
    /// Its skills, all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub skills: Vec<Skill>,
    /// Its guardrails, all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub guardrails: Vec<Guardrail>,
    /// For a workflow, its step edits. Its instructions are empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub steps: Vec<StepEdit>,
    /// What changed, as the editor put it.
    #[schemars(length(max = 1000))]
    pub change: String,
    /// The editor's evidence that held.
    #[schemars(length(max = 8))]
    pub cites: Vec<Cite>,
    /// The editor's citations that didn't hold, and why. They count for
    /// nothing.
    #[serde(default)]
    #[schemars(length(max = 8), inner(length(max = 2048)))]
    pub dropped: Vec<String>,
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
    /// The model the composer ran on. Only a model that passed its known-answer
    /// test may have its decisions adopted.
    #[schemars(length(max = 256))]
    pub composer_model: Option<String>,
    /// What became of a supported edit, once the system looked at adopting it.
    pub adoption: Option<Adoption>,
    /// The changes whose lessons the editor was shown.
    #[serde(default)]
    #[schemars(length(max = 3), inner(length(max = 36)))]
    pub lessons_shown: Vec<String>,
}

/// Whether a supported edit was adopted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Adoption {
    /// True if the agent's file was changed.
    pub adopted: bool,
    /// The change, if there is one.
    #[schemars(length(max = 36))]
    pub change_id: Option<String>,
    /// Why, in a sentence.
    #[schemars(length(max = 2048))]
    pub reason: String,
}

impl Candidate {
    fn drafted(label: &str, kind: Kind, draft: Draft, agent: &Agent) -> Self {
        Self {
            system_prompt: draft.system_prompt,
            skills: draft.skills.unwrap_or_else(|| agent.skills.clone()),
            guardrails: draft.guardrails.unwrap_or_else(|| agent.guardrails.clone()),
            ..Self::empty(label, kind, draft.change, draft.cites)
        }
    }

    /// A candidate with nothing in it yet.
    pub(crate) fn empty(label: &str, kind: Kind, change: String, cites: Vec<Cite>) -> Self {
        Self {
            label: label.to_owned(),
            kind,
            system_prompt: String::new(),
            skills: Vec::new(),
            guardrails: Vec::new(),
            steps: Vec::new(),
            change,
            cites,
            dropped: Vec::new(),
            refused: None,
            probe: None,
        }
    }

    /// The agent as this candidate would have it.
    #[must_use]
    pub fn agent(&self, base: &Agent) -> Agent {
        Agent {
            system_prompt: self.system_prompt.clone(),
            skills: self.skills.clone(),
            guardrails: self.guardrails.clone(),
            ..base.clone()
        }
    }

    /// The workflow as this candidate would have it.
    ///
    /// # Errors
    ///
    /// Why one of its edits can't be made, as [`apply_edits`] says.
    pub fn workflow(&self, base: &Workflow, reads: &BTreeSet<String>) -> Result<Workflow, String> {
        apply_edits(base, &self.steps, reads)
    }

    /// Whether it's the agent or workflow as it is.
    fn changes_nothing(&self, base: &Base) -> bool {
        match base {
            Base::Agent(agent) => {
                self.system_prompt.trim() == agent.system_prompt.trim()
                    && self.skills == agent.skills
                    && self.guardrails == agent.guardrails
            }
            Base::Workflow {
                workflow, reads, ..
            } => self.steps.is_empty() || self.workflow(workflow, reads).as_ref() == Ok(workflow),
        }
    }

    /// The candidate as a judge reads it.
    fn shown(&self) -> String {
        if !self.steps.is_empty() {
            return format!("STEP EDITS:\n{}", listed(&self.steps));
        }
        format!(
            "INSTRUCTIONS:\n{}\nSKILLS:\n{}\nGUARDRAILS:\n{}",
            self.system_prompt,
            listed(&self.skills),
            listed(&self.guardrails)
        )
    }
}

/// `first`'s items, then those of `second` with a name not taken.
fn union<T: Clone>(first: &[T], second: &[T], name: impl Fn(&T) -> &str) -> Vec<T> {
    let mut all = first.to_vec();
    for item in second {
        if !all.iter().any(|kept| name(kept) == name(item)) {
            all.push(item.clone());
        }
    }
    all
}

/// What an attempt improves, as it was when the attempt began.
#[derive(Debug, Clone)]
pub(crate) enum Base {
    /// An agent.
    Agent(Agent),
    /// A workflow, the inputs it's tried on, and the tools that only read.
    Workflow {
        workflow: Workflow,
        inputs: Value,
        reads: BTreeSet<String>,
    },
}

impl Attempt {
    /// An attempt stopped before it could finish, such as by its time
    /// limit, so that it's kept all the same.
    #[must_use]
    pub fn stopped(request: Request, started_at: DateTime<Utc>, why: &str) -> Self {
        let mut attempt = Self::begun(request);
        attempt.started_at = started_at;
        attempt.problem(why);
        attempt.end(Status::Failed)
    }

    pub(crate) fn begun(request: Request) -> Self {
        Self {
            request,
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
            composer_model: None,
            adoption: None,
            lessons_shown: Vec::new(),
        }
    }

    /// The supported candidate, if there is one.
    #[must_use]
    pub fn supported(&self) -> Option<&Candidate> {
        let chosen = self.chosen.as_ref()?;
        (self.status == Status::Supported)
            .then(|| self.candidates.iter().find(|c| c.label == *chosen))
            .flatten()
    }

    pub(crate) fn problem(&mut self, text: impl Into<String>) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(schema::bounded(&text.into()));
        }
    }

    pub(crate) fn spend(&mut self, cost: u64) {
        self.cost_usd_micros = self.cost_usd_micros.saturating_add(cost);
    }

    pub(crate) fn end(mut self, status: Status) -> Self {
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
    u8::from(matches!(outcome, Some("answered" | "completed")))
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

    pub(crate) fn judge_money(&self, attempt: &Attempt) -> u64 {
        self.settings
            .judge_usd_micros
            .saturating_sub(attempt.cost_usd_micros)
    }

    /// Runs one attempt. It always returns one, and a failure says why.
    pub async fn improve(&self, request: Request) -> Attempt {
        match request.target {
            Target::Agent => self.improve_agent(request).await,
            Target::Workflow => self.improve_workflow(request).await,
        }
    }

    async fn improve_agent(&self, request: Request) -> Attempt {
        let mut attempt = Attempt::begun(request.clone());
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
        let lessons = self.lessons(&mut attempt).await;
        let brief = Brief {
            target: Target::Agent,
            instructions: &agent.system_prompt,
            tools: &agent.tools,
            skills: &agent.skills,
            guardrails: &agent.guardrails,
            task: &request.task,
            why: request.why.as_deref(),
            lessons: &lessons,
        };
        let money = self.judge_money(&attempt);
        let edited = match self.judges.edit(&brief, &reference, money).await {
            Ok(edited) => edited,
            Err(error) => {
                attempt.spend(error.cost_usd_micros());
                attempt.problem(format!("the editor: {error}"));
                return attempt.end(Status::Failed);
            }
        };
        attempt.spend(edited.cost_usd_micros);
        let base = Base::Agent(agent.clone());
        let candidates = [
            Candidate::drafted("A", Kind::Invariant, edited.value.invariant, &agent),
            Candidate::drafted("B", Kind::Domain, edited.value.domain, &agent),
        ];
        let runs = self
            .try_all(&mut attempt, &base, candidates, reference)
            .await;
        if runs.len() == 1 {
            attempt.problem("no candidate could be tried");
            return attempt.end(Status::Kept);
        }
        self.decide(attempt, &base, &brief, runs).await
    }

    /// The lessons that fit the attempt, as text, with which were shown
    /// kept on it. Looked up for this task, never carried over.
    pub(crate) async fn lessons(&self, attempt: &mut Attempt) -> Vec<String> {
        let request = &attempt.request;
        let found = lessons_for(
            self.engine.store.as_ref(),
            request.target,
            &request.agent,
            &request.task,
            request.why.as_deref(),
        )
        .await;
        attempt.lessons_shown = found.iter().map(|(change, _)| change.clone()).collect();
        found.into_iter().map(|(_, text)| text).collect()
    }

    /// Checks and probes each candidate, keeping them on the attempt.
    /// Returns the runs to compare, the reference first.
    pub(crate) async fn try_all(
        &self,
        attempt: &mut Attempt,
        base: &Base,
        candidates: [Candidate; 2],
        reference: Rendered,
    ) -> Vec<Rendered> {
        let mut runs = vec![reference.clone()];
        let task = attempt.request.task.clone();
        for mut candidate in candidates {
            candidate.refused = self.refusal(&mut candidate, base, &task, &reference).await;
            if candidate.refused.is_none()
                && let Some((probed, rendered)) =
                    self.try_candidate(base, &candidate, attempt).await
            {
                candidate.probe = Some(probed);
                runs.push(rendered);
            }
            attempt.candidates.push(candidate);
        }
        runs
    }

    /// Probes a candidate that passed its checks.
    async fn try_candidate(
        &self,
        base: &Base,
        candidate: &Candidate,
        attempt: &mut Attempt,
    ) -> Option<(Probed, Rendered)> {
        let Request {
            agent: name, task, ..
        } = attempt.request.clone();
        match base {
            Base::Agent(agent) => {
                let edit = candidate.agent(agent);
                let (probed, trajectory) = self.probe(&name, &edit, &task, attempt).await?;
                let rendered = Rendered::new(&candidate.label, &trajectory);
                Some((summary(&probed, &rendered), rendered))
            }
            Base::Workflow {
                workflow,
                inputs,
                reads,
            } => {
                let edited = match candidate.workflow(workflow, reads) {
                    Ok(edited) => edited,
                    Err(why) => {
                        attempt.problem(why);
                        return None;
                    }
                };
                self.probe_flow(&name, &edited, inputs, &candidate.label, attempt)
                    .await
            }
        }
    }

    /// Why a candidate can't be run, if it can't: it breaks a rule on what
    /// an edit may do, or wouldn't pass the checks a definition does.
    async fn unusable(&self, base: &Base, candidate: &Candidate, task: &str) -> Option<String> {
        match base {
            Base::Agent(agent) => {
                let edit = candidate.agent(agent);
                check_harness(&edit, agent, task)
                    .and_then(|()| self.engine.check_candidate(&edit))
                    .err()
            }
            Base::Workflow {
                workflow, reads, ..
            } => {
                let edited = match candidate.workflow(workflow, reads) {
                    Ok(edited) => edited,
                    Err(why) => return Some(why),
                };
                if let Err(why) = check_workflow_edit(&edited, workflow, task)
                    .and_then(|()| check_edited_reads(&edited, workflow, &self.engine.registry))
                {
                    return Some(why);
                }
                self.engine.check_workflow_candidate(&edited).await.err()
            }
        }
    }

    /// Why a candidate can't be tried, if it can't. Citations that don't hold
    /// are dropped, and recorded as dropped.
    async fn refusal(
        &self,
        candidate: &mut Candidate,
        base: &Base,
        task: &str,
        reference: &Rendered,
    ) -> Option<String> {
        if candidate.changes_nothing(base) {
            return Some("it changes nothing".to_owned());
        }
        if let Some(why) = self.unusable(base, candidate, task).await {
            return Some(why);
        }
        // Every edit traces to a moment in the reference run.
        if candidate.cites.is_empty() {
            return Some("it cites no step of run R".to_owned());
        }
        let mut held = Vec::new();
        for cite in std::mem::take(&mut candidate.cites) {
            match cite.check(&[reference]) {
                Ok(()) => held.push(cite),
                Err(why) => candidate.dropped.push(schema::bounded(&why)),
            }
        }
        candidate.cites = held;
        candidate.cites.is_empty().then(|| {
            format!(
                "none of its citations of run R hold: {}",
                candidate.dropped.join("; ")
            )
        })
    }

    pub(crate) async fn decide(
        &self,
        mut attempt: Attempt,
        base: &Base,
        brief: &Brief<'_>,
        runs: Vec<Rendered>,
    ) -> Attempt {
        let tried: Vec<(String, String)> = attempt
            .candidates
            .iter()
            .filter(|c| c.probe.is_some())
            .map(|c| (c.label.clone(), c.shown()))
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
            Decision::Merge => self.merge(attempt, base, brief, &runs, reply).await,
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
                attempt.spend(error.cost_usd_micros());
                attempt.problem(format!("the composer: {error}"));
                return None;
            }
        };
        attempt.spend(reply.cost_usd_micros);
        attempt.composer_model = Some(reply.model.clone());
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
        base: &Base,
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
        let mut candidate = Candidate::empty("M", Kind::Merged, reply.reason, Vec::new());
        {
            let pair: Vec<&Candidate> = attempt
                .candidates
                .iter()
                .filter(|c| c.probe.is_some())
                .collect();
            match (base, pair.as_slice()) {
                // The merge keeps both candidates' skills and guardrails.
                (Base::Agent(_), [first, second]) => {
                    candidate.system_prompt = reply.merged.unwrap_or_default();
                    candidate.skills = union(&first.skills, &second.skills, |skill| &skill.name);
                    candidate.guardrails =
                        union(&first.guardrails, &second.guardrails, |rule| &rule.name);
                }
                // A workflow's merge is made in code, not by the composer.
                (Base::Workflow { .. }, [first, second]) => {
                    candidate.steps = merge_edits(&first.steps, &second.steps);
                }
                _ => {}
            }
        }
        let task = attempt.request.task.clone();
        candidate.refused = self.unusable(base, &candidate, &task).await;
        if candidate.refused.is_some() {
            attempt.candidates.push(candidate);
            return attempt.end(Status::Kept);
        }
        let Some((probed, rendered)) = self.try_candidate(base, &candidate, &mut attempt).await
        else {
            attempt.candidates.push(candidate);
            return attempt.end(Status::Failed);
        };
        candidate.probe = Some(probed);
        let shown = candidate.shown();
        attempt.candidates.push(candidate);
        // The merge is judged on its own run, against the reference.
        let pair: Vec<Rendered> = runs.iter().take(1).cloned().chain([rendered]).collect();
        let tried = [("M".to_owned(), shown)];
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
        runs: Vec::new(),
        harness: probed.harness.clone(),
        outcome: rendered.outcome.clone(),
        calls: rendered.calls.clone(),
    }
}
