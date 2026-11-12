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
use harness::{Attempt, Pick, Recent, pick};
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
            .hold_on(agent)
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
            .changes_to(agent)
            .await
            .map_err(super::ChangeError::message)?;
        if changes.iter().any(|(_, change)| change.at >= since) {
            return Ok(Some(format!(
                "{agent} was changed in the last {REST_HOURS} hours"
            )));
        }
        Ok(None)
    }

    async fn recent_runs(&self, agent: &str) -> Result<Recent, String> {
        let since = Utc::now() - Span::days(RECENT_DAYS);
        let listed = self
            .sink
            .list_runs(&RunQuery {
                agent: Some(format!("agent:{agent}")),
                harness: None,
                offset: 0,
                limit: RECENT_RUNS,
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
        let mut skipped = Vec::new();
        let mut recent = Vec::new();
        for definition in agents {
            let name = definition.key.name;
            match self.resting(&name, &tried).await? {
                Some(why) => skipped.push(why),
                None => recent.push(self.recent_runs(&name).await?),
            }
        }
        let Some(picked) = pick(&recent) else {
            return Ok(Picked {
                pick: None,
                job_id: None,
                skipped,
            });
        };
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
        Ok(Picked {
            pick: Some(picked),
            job_id: Some(id.to_string()),
            skipped,
        })
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
