//! Choosing what to improve, as a job that can be scheduled: look at each
//! agent's recent real runs, and queue one attempt for the agent whose runs
//! went wrong the same way most often.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Duration as Span, Utc};
use dispatch::read_trajectory;
use events::RunQuery;
use harness::{Attempt, Evidence, Marked, Pick, Recent, Reviewed, best, chat_evidence, evidence};
use protocol::Actor;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use store::{DefinitionKind, Job, JobId, JobStatus, PageRequest, Store};
use uuid::Uuid;
use work::{Enqueued, JobFailure, JobHandler, JobTransport, StoreTransport};

use super::HARNESS_KIND;
use crate::Runtime;

/// The kind of job that picks what to improve.
pub(crate) const PICK_KIND: &str = "harness_pick";
/// How many of an agent's runs are looked at.
const RECENT_RUNS: usize = 20;
/// How many routed chats are looked at.
const RECENT_CHATS: usize = 50;
/// How far back a run counts as recent.
const RECENT_DAYS: i64 = 7;
/// How long after an attempt or a change an agent is left alone.
const REST_HOURS: i64 = 24;
/// The most agents looked at.
const MAX_AGENTS: u32 = 200;
/// The most steps read from one run.
const MAX_STEPS: usize = 512;
/// The longest picking may take.
const TIME_LIMIT: Duration = Duration::from_secs(300);

/// What a pick job is given: nothing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PickArgs {}

/// What a pick job found.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Picked {
    /// The agent picked, if one was.
    pub pick: Option<Pick>,
    /// The attempt queued for it.
    #[schemars(length(max = 36))]
    pub job_id: Option<String>,
    /// Agents left alone, and why.
    #[schemars(length(max = 200), inner(length(max = 2048)))]
    pub skipped: Vec<String>,
    /// The profile or writer that routed chats point to. Its attempt probes
    /// it as the route runs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed: Option<Pick>,
    /// The attempt queued for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 36))]
    pub routed_job_id: Option<String>,
}

/// One attempt per agent per day, so a schedule that fires twice queues it
/// once.
fn attempt_job_id(agent: &str, day: chrono::NaiveDate) -> JobId {
    let seed = format!("harness|{agent}|{day}");
    JobId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
}

impl Runtime {
    /// Agents tried in the last day.
    async fn tried_lately(&self) -> Result<BTreeSet<String>, String> {
        let since = Utc::now() - Span::hours(REST_HOURS);
        let mut tried = BTreeSet::new();
        let mut offset = 0;
        loop {
            let page = PageRequest::new(MAX_AGENTS, offset).map_err(|error| error.to_string())?;
            let found = self
                .attempts(None, page)
                .await
                .map_err(|error| error.to_string())?;
            for (_, data) in &found.items {
                if let Ok(attempt) = serde_json::from_value::<Attempt>(data.clone())
                    && attempt.started_at >= since
                {
                    tried.insert(attempt.request.agent);
                }
            }
            if !found.has_more {
                return Ok(tried);
            }
            offset = offset.saturating_add(MAX_AGENTS);
        }
    }

    /// Why `agent` should be left alone now, if it should.
    async fn resting(
        &self,
        agent: &str,
        tried: &BTreeSet<String>,
    ) -> Result<Option<String>, String> {
        if let Some(hold) = self
            .hold_on(agent, harness::Target::Agent)
            .await
            .map_err(super::ChangeError::message)?
        {
            return Ok(Some(format!("{agent} is held: {}", hold.reason)));
        }
        if tried.contains(agent) {
            return Ok(Some(format!(
                "{agent} was tried in the last {REST_HOURS} hours"
            )));
        }
        let since = Utc::now() - Span::hours(REST_HOURS);
        let changes = self
            .changes_to(agent, harness::Target::Agent)
            .await
            .map_err(super::ChangeError::message)?;
        if changes.iter().any(|(_, change)| change.at >= since) {
            return Ok(Some(format!(
                "{agent} was changed in the last {REST_HOURS} hours"
            )));
        }
        Ok(None)
    }

    pub(super) async fn recent_runs(&self, agent: &str) -> Result<Recent, String> {
        let since = Utc::now() - Span::days(RECENT_DAYS);
        let listed = self
            .sink
            .list_runs(&RunQuery {
                agent: Some(format!("agent:{agent}")),
                harness: None,
                offset: 0,
                limit: RECENT_RUNS,
                requested_by: None,
                // Every run counts as evidence; a private one's words are
                // never taken from it (see `task_of` in harness::pick).
                visible: events::Visibility::All,
            })
            .await
            .map_err(|error| error.to_string())?;
        let mut runs = Vec::new();
        for summary in listed.runs.iter().filter(|run| run.started_at >= since) {
            if let Some(trajectory) = read_trajectory(self.sink.as_ref(), summary.run, MAX_STEPS)
                .await
                .map_err(|error| error.to_string())?
            {
                runs.push(trajectory);
            }
        }
        Ok(Recent {
            agent: agent.to_owned(),
            runs,
        })
    }

    /// The recent routed chats, newest first, and the writer they're
    /// charged to: the route's writer now.
    pub(super) async fn recent_chats(&self) -> Result<(Vec<dispatch::Trajectory>, String), String> {
        let line = match self.config.route() {
            Some(route) => self.line_of(route).await,
            None => self.default_line().await,
        }
        .map_err(|failure| failure.to_string())?;
        let since = Utc::now() - Span::days(RECENT_DAYS);
        let listed = self
            .sink
            .list_runs(&RunQuery {
                agent: Some("chat".to_owned()),
                harness: None,
                offset: 0,
                limit: RECENT_CHATS,
                requested_by: None,
                // Every run counts as evidence; a private one's words are
                // never taken from it (see `task_of` in harness::pick).
                visible: events::Visibility::All,
            })
            .await
            .map_err(|error| error.to_string())?;
        let mut chats = Vec::new();
        for summary in listed.runs.iter().filter(|run| run.started_at >= since) {
            if let Some(trajectory) = read_trajectory(self.sink.as_ref(), summary.run, MAX_STEPS)
                .await
                .map_err(|error| error.to_string())?
            {
                chats.push(trajectory);
            }
        }
        Ok((chats, line.writer.0))
    }

    /// The profile or writer routed chats point to, from agents not
    /// resting. It's noted in `skipped` too, since nothing is queued for it.
    async fn routed_pick(
        &self,
        names: &BTreeSet<String>,
        tried: &BTreeSet<String>,
        marked: &Marked,
        reviewed: &Reviewed,
        skipped: &mut Vec<String>,
    ) -> Result<Option<Pick>, String> {
        let routed = match self.recent_chats().await {
            Ok((chats, writer)) => {
                let mut found = Vec::new();
                for chatted in chat_evidence(&chats, &writer, marked, reviewed) {
                    // A writer made from system_prompt is no agent's file.
                    if names.contains(&chatted.agent)
                        && self.resting(&chatted.agent, tried).await?.is_none()
                    {
                        found.push(chatted);
                    }
                }
                skipped.extend(harness::private_only(&found));
                best(&found)
            }
            Err(why) => {
                skipped.push(format!("routed chats weren't read: {why}"));
                None
            }
        };
        Ok(routed)
    }

    /// Other recent tasks of the attempt's agent, newest first, to replay a
    /// supported edit on: from the chats it took part in for a routed
    /// attempt, from its own runs otherwise. Never the attempt's own task.
    pub(super) async fn replay_tasks(&self, attempt: &Attempt) -> Vec<String> {
        let most = usize::try_from(self.config.harness.replay_tasks).unwrap_or(0);
        if most == 0 {
            return Vec::new();
        }
        let agent = &attempt.request.agent;
        let runs: Vec<dispatch::Trajectory> = if attempt.request.routed {
            match self.recent_chats().await {
                Ok((chats, writer)) => chats
                    .into_iter()
                    .filter(|chat| harness::chat_failures(chat, &writer).1.contains(agent))
                    .collect(),
                Err(_) => Vec::new(),
            }
        } else {
            self.recent_runs(agent)
                .await
                .map(|recent| recent.runs)
                .unwrap_or_default()
        };
        let mut tasks: Vec<String> = Vec::new();
        // A private run's words are never replayed.
        for task in runs
            .iter()
            .filter(|run| !run.private)
            .filter_map(harness::task)
        {
            if task != attempt.request.task && !tasks.contains(&task) {
                tasks.push(task);
            }
            if tasks.len() >= most {
                break;
            }
        }
        tasks
    }

    /// Picks an agent to improve and queues an attempt for it.
    pub(crate) async fn pick_and_queue(&self) -> Result<Picked, String> {
        self.harness_records()
            .await
            .map_err(|error| error.to_string())?;
        let mut agents = Vec::new();
        let mut offset = 0;
        loop {
            let page = PageRequest::new(MAX_AGENTS, offset).map_err(|error| error.to_string())?;
            let found = self
                .workflows
                .store
                .list_definitions(DefinitionKind::Agent, page)
                .await
                .map_err(|error| error.to_string())?;
            agents.extend(found.items);
            if !found.has_more {
                break;
            }
            offset = offset.saturating_add(MAX_AGENTS);
        }
        let tried = self.tried_lately().await?;
        let marked: Marked = self
            .marked_bad(Utc::now() - Span::days(RECENT_DAYS))
            .await
            .map_err(|error| error.to_string())?;
        let reviewed: Reviewed = self
            .reviewed_since(Utc::now() - Span::days(RECENT_DAYS))
            .await
            .map_err(|error| error.to_string())?;
        let mut skipped = Vec::new();
        let mut names = BTreeSet::new();
        let mut own: Vec<Evidence> = Vec::new();
        for definition in agents {
            let name = definition.key.name;
            names.insert(name.clone());
            match self.resting(&name, &tried).await? {
                Some(why) => skipped.push(why),
                None => own.push(evidence(&self.recent_runs(&name).await?, &marked)),
            }
        }
        let routed = self
            .routed_pick(&names, &tried, &marked, &reviewed, &mut skipped)
            .await?;
        let routed_job_id = match &routed {
            Some(chatted) => Some(self.queue(chatted, &mut skipped).await?),
            None => None,
        };
        skipped.extend(harness::private_only(&own));
        let Some(picked) = best(&own) else {
            return Ok(Picked {
                pick: None,
                job_id: None,
                skipped,
                routed,
                routed_job_id,
            });
        };
        let job_id = Some(self.queue(&picked, &mut skipped).await?);
        Ok(Picked {
            pick: Some(picked),
            job_id,
            skipped,
            routed,
            routed_job_id,
        })
    }

    /// Queues `picked`'s attempt: one per agent per day, so an agent picked
    /// twice in a day, or from its runs and its chats, is attempted once.
    async fn queue(&self, picked: &Pick, skipped: &mut Vec<String>) -> Result<String, String> {
        let id = attempt_job_id(&picked.request.agent, Utc::now().date_naive());
        let job = Job {
            id,
            kind: HARNESS_KIND.to_owned(),
            payload: serde_json::to_value(&picked.request).map_err(|error| error.to_string())?,
            priority: 0,
            status: JobStatus::Queued,
            requested_by: Actor::new("harness"),
            run_at: Utc::now(),
            attempts: 0,
            max_attempts: 1,
            lease: None,
            last_error: None,
            result: None,
            finished_at: None,
        };
        let transport = StoreTransport(Arc::clone(&self.workflows.store) as Arc<dyn Store>);
        if transport
            .enqueue(job)
            .await
            .map_err(|error| error.to_string())?
            == Enqueued::AlreadyThere
        {
            skipped.push(format!(
                "{} already has today's attempt",
                picked.request.agent
            ));
        }
        Ok(id.to_string())
    }
}

/// Picks what to improve, as a job.
#[derive(Debug)]
pub(crate) struct PickJob(pub(crate) Runtime);

#[async_trait]
impl JobHandler for PickJob {
    fn kind(&self) -> &'static str {
        PICK_KIND
    }

    fn payload_schema(&self) -> Value {
        schema::schema_of::<PickArgs>()
    }

    fn time_limit(&self) -> Duration {
        TIME_LIMIT
    }

    async fn run(&self, job: &Job) -> Result<Value, JobFailure> {
        let _: PickArgs =
            serde_json::from_value(job.payload.clone()).map_err(|error| error.to_string())?;
        let picked = self.0.pick_and_queue().await?;
        Ok(json!(picked))
    }
}
