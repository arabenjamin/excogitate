//! The `job_history` tool: what the queue ran, the latest first, so "what
//! ran today?" and "why did the scan fail?" have an answer.
//!
//! It lists what each job was, not what it was given or found: a job's
//! inputs and result can hold private data, and the run that asks may be
//! public. A public run sees no error text either, as a plugin's own look
//! at its jobs doesn't.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::{DataClass, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::{Job, JobStatus, PageRequest, Store};
use uuid::Uuid;

use crate::clamp::clamped;

const DESCRIPTION: &str = "What the job queue ran, the latest first: each job's kind and \
workflow, its status (queued, running, succeeded or dead), who asked for it, when it ran and \
finished, its attempts, and why it last failed. `status` keeps one status; `limit` is how many \
(10 unless given, at most 50). Not a job's inputs or what it found.";

const DEFAULT_LIMIT: u32 = 10;
const MAX_LIMIT: u32 = 50;
const MAX_NAME: usize = 128;
const MAX_ERROR: usize = 1000;

/// A job's status, as the tool names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Status {
    Queued,
    Running,
    Succeeded,
    Dead,
}

impl From<JobStatus> for Status {
    fn from(status: JobStatus) -> Self {
        match status {
            JobStatus::Queued => Self::Queued,
            JobStatus::Claimed => Self::Running,
            JobStatus::Succeeded => Self::Succeeded,
            JobStatus::Dead => Self::Dead,
        }
    }
}

impl From<Status> for JobStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Queued => Self::Queued,
            Status::Running => Self::Claimed,
            Status::Succeeded => Self::Succeeded,
            Status::Dead => Self::Dead,
        }
    }
}

/// What the model asks for.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobHistoryArgs {
    /// Only jobs with this status.
    status: Option<Status>,
    /// How many, the latest first. The default is 10.
    #[schemars(range(min = 1, max = MAX_LIMIT))]
    limit: Option<u32>,
}

/// One job.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct JobRow {
    /// The job.
    id: Uuid,
    /// What kind of job: `workflow`, or another a worker runs.
    #[schemars(length(max = MAX_NAME))]
    kind: String,
    /// For a workflow run, which workflow.
    #[schemars(length(max = MAX_NAME))]
    workflow: Option<String>,
    /// Where it stands.
    status: Status,
    /// Who asked for it: the owner, a schedule, a run.
    #[schemars(length(max = MAX_NAME))]
    requested_by: String,
    /// When it ran, or is due to.
    run_at: DateTime<Utc>,
    /// When it finished, if it has.
    finished_at: Option<DateTime<Utc>>,
    /// Tries so far.
    attempts: u32,
    /// Tries it may have.
    max_attempts: u32,
    /// Why it last failed. Never shown to a public run.
    #[schemars(length(max = MAX_ERROR))]
    last_error: Option<String>,
}

/// What the tool found.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct JobHistoryResult {
    /// The jobs, the latest `run_at` first.
    #[schemars(length(max = MAX_LIMIT))]
    jobs: Vec<JobRow>,
    /// True when there are more than are listed.
    has_more: bool,
}

fn row(job: Job, private: bool) -> JobRow {
    let workflow = job
        .payload
        .get("workflow")
        .and_then(Value::as_str)
        .map(|name| clamped(name.to_owned(), MAX_NAME));
    JobRow {
        id: job.id.as_uuid(),
        kind: clamped(job.kind, MAX_NAME),
        workflow,
        status: job.status.into(),
        requested_by: clamped(job.requested_by.as_str().to_owned(), MAX_NAME),
        run_at: job.run_at,
        finished_at: job.finished_at,
        attempts: job.attempts,
        max_attempts: job.max_attempts,
        last_error: job
            .last_error
            .filter(|_| private)
            .map(|error| clamped(error, MAX_ERROR)),
    }
}

/// Lists what the job queue ran.
#[derive(Debug, Clone)]
pub struct JobHistoryTool {
    store: Arc<dyn Store>,
}

impl JobHistoryTool {
    /// The tool, over this store's queue.
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    /// The tool's input and output schemas, as published.
    #[must_use]
    pub fn schemas() -> (Value, Value) {
        (
            schema::schema_of::<JobHistoryArgs>(),
            schema::schema_of::<JobHistoryResult>(),
        )
    }
}

#[async_trait]
impl Tool for JobHistoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::for_types::<JobHistoryArgs, JobHistoryResult>(
            "job_history",
            DESCRIPTION,
            Effect::Read,
        )
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let args: JobHistoryArgs = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        let page = PageRequest::new(args.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT), 0)
            .map_err(|error| ToolFailure::new(error.to_string()))?;
        let listed = self
            .store
            .list_jobs(args.status.map(JobStatus::from), page)
            .await
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        let private = context.data_class == DataClass::Private;
        let found = JobHistoryResult {
            jobs: listed
                .items
                .into_iter()
                .map(|job| row(job, private))
                .collect(),
            has_more: listed.has_more,
        };
        let value = serde_json::to_value(found)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        // An error's text came from a tool or a page: data, never orders.
        Ok(ToolOutput::new(value, Trust::Untrusted))
    }
}
