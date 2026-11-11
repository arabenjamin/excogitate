//! The REST API: start workflows, and see what's defined and what's run.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use protocol::Actor;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use store::{Definition, DefinitionKey, DefinitionKind, Job, JobId, JobStatus, PageRequest};
use uuid::Uuid;
use work::Enqueued;
use workflow::WorkflowError;

use super::{ApiError, App, json_body};
use crate::jobs::WORKFLOW_KIND;

pub(super) const DEFAULT_LIMIT: u32 = 50;
pub(super) const MAX_LIMIT: u32 = 200;
/// Who a request came from, until there are users.
const OWNER: &str = "owner";

/// Starts a workflow.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    /// Its inputs, which must fit what it declares.
    #[schemars(extend("x-extension" = "checked against the workflow's inputs schema"))]
    pub inputs: Value,
    /// Higher runs first.
    #[serde(default)]
    #[schemars(range(min = -1000, max = 1000))]
    pub priority: i32,
    /// Send the same key again and no second run starts, so a retry is safe.
    #[schemars(length(min = 1, max = 128))]
    pub idempotency_key: Option<String>,
}

/// A run that was asked for.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RunStarted {
    /// The job to look at for how it went.
    #[schemars(length(max = 36))]
    pub job_id: String,
    /// True if this key had already started it, and nothing new was queued.
    pub already_existed: bool,
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatusView {
    /// Waiting to run.
    Queued,
    /// Running.
    Claimed,
    /// It finished.
    Succeeded,
    /// It failed and has no attempts left.
    Dead,
}

impl From<JobStatus> for JobStatusView {
    fn from(status: JobStatus) -> Self {
        match status {
            JobStatus::Queued => Self::Queued,
            JobStatus::Claimed => Self::Claimed,
            JobStatus::Succeeded => Self::Succeeded,
            JobStatus::Dead => Self::Dead,
        }
    }
}

/// A job, as the API shows it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct JobView {
    /// Its id.
    #[schemars(length(max = 36))]
    pub id: String,
    /// What kind of work it is.
    #[schemars(length(max = 64))]
    pub kind: String,
    /// Its input.
    #[schemars(extend("x-extension" = "the job kind's own payload"))]
    pub payload: Value,
    /// Higher runs first.
    pub priority: i32,
    /// Where it stands.
    pub status: JobStatusView,
    /// Who asked for it.
    #[schemars(length(max = 256))]
    pub requested_by: String,
    /// The earliest it may run.
    pub run_at: DateTime<Utc>,
    /// How many times it has been claimed.
    pub attempts: u32,
    /// How many claims it may have.
    pub max_attempts: u32,
    /// Why the last attempt failed.
    #[schemars(length(max = 2048))]
    pub last_error: Option<String>,
    /// What it produced, or what a failed attempt left behind.
    #[schemars(extend("x-extension" = "the job kind's own result"))]
    pub result: Option<Value>,
}

impl From<Job> for JobView {
    fn from(job: Job) -> Self {
        Self {
            id: job.id.to_string(),
            kind: job.kind,
            payload: job.payload,
            priority: job.priority,
            status: job.status.into(),
            requested_by: job.requested_by.as_str().to_owned(),
            run_at: job.run_at,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            last_error: job.last_error,
            result: job.result,
        }
    }
}

/// One page of jobs, newest first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct JobList {
    /// The jobs.
    #[schemars(length(max = MAX_LIMIT))]
    pub items: Vec<JobView>,
    /// True if there are more past this page.
    pub has_more: bool,
}

/// A definition, as the API shows it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DefinitionView {
    /// Its name.
    #[schemars(length(max = 64))]
    pub name: String,
    /// Where it came from.
    #[schemars(length(max = 256))]
    pub source: String,
    /// When it was last written.
    pub updated_at: DateTime<Utc>,
    /// What it says.
    #[schemars(extend("x-extension" = "the definition, which met its own schema when loaded"))]
    pub body: Value,
}

impl From<Definition> for DefinitionView {
    fn from(definition: Definition) -> Self {
        Self {
            name: definition.key.name,
            source: definition.source,
            updated_at: definition.updated_at,
            body: definition.body,
        }
    }
}

/// One page of definitions, by name.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DefinitionList {
    /// The definitions.
    #[schemars(length(max = MAX_LIMIT))]
    pub items: Vec<DefinitionView>,
    /// True if there are more past this page.
    pub has_more: bool,
}

pub(super) type Params = Query<BTreeMap<String, String>>;

/// Refuses a query parameter nothing reads, so a typo can't go unnoticed.
pub(super) fn only(params: &BTreeMap<String, String>, known: &[&str]) -> Result<(), ApiError> {
    match params.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(ApiError::invalid(&format!(
            "unknown query parameter {key}; this takes {}",
            known.join(", ")
        ))),
        None => Ok(()),
    }
}

pub(super) fn number(
    params: &BTreeMap<String, String>,
    name: &str,
    default: u32,
) -> Result<u32, ApiError> {
    params.get(name).map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| ApiError::invalid(&format!("{name} must be a whole number")))
    })
}

fn page(params: &BTreeMap<String, String>) -> Result<PageRequest, ApiError> {
    let limit = number(params, "limit", DEFAULT_LIMIT)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(ApiError::invalid(&format!(
            "limit must be 1 to {MAX_LIMIT}"
        )));
    }
    PageRequest::new(limit, number(params, "offset", 0)?)
        .map_err(|error| ApiError::invalid(&error.to_string()))
}

fn status_named(name: &str) -> Result<JobStatus, ApiError> {
    match name {
        "queued" => Ok(JobStatus::Queued),
        "claimed" => Ok(JobStatus::Claimed),
        "succeeded" => Ok(JobStatus::Succeeded),
        "dead" => Ok(JobStatus::Dead),
        _ => Err(ApiError::invalid(
            "status must be queued, claimed, succeeded or dead",
        )),
    }
}

fn kind_named(name: &str) -> Result<DefinitionKind, ApiError> {
    match name {
        "agent" => Ok(DefinitionKind::Agent),
        "workflow" => Ok(DefinitionKind::Workflow),
        "schedule" => Ok(DefinitionKind::Schedule),
        _ => Err(ApiError::not_found(
            "there are agent, workflow and schedule definitions",
        )),
    }
}

async fn list_jobs(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<JobList>, ApiError> {
    only(&params, &["status", "limit", "offset"])?;
    let status = params
        .get("status")
        .map(|name| status_named(name))
        .transpose()?;
    let found = app
        .brain
        .workflows
        .store
        .list_jobs(status, page(&params)?)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(Json(JobList {
        items: found.items.into_iter().map(JobView::from).collect(),
        has_more: found.has_more,
    }))
}

async fn get_job(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<JobView>, ApiError> {
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::invalid("a job id is a UUID"))?;
    app.brain
        .workflows
        .store
        .get_job(JobId::from_uuid(id))
        .await
        .map_err(|error| ApiError::internal(&error))?
        .map(|job| Json(job.into()))
        .ok_or_else(|| ApiError::not_found("there's no job with that id"))
}

async fn list_definitions(
    State(app): State<App>,
    Path(kind): Path<String>,
    Query(params): Params,
) -> Result<Json<DefinitionList>, ApiError> {
    only(&params, &["limit", "offset"])?;
    let kind = kind_named(&kind)?;
    let found = app
        .brain
        .workflows
        .store
        .list_definitions(kind, page(&params)?)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(Json(DefinitionList {
        items: found.items.into_iter().map(DefinitionView::from).collect(),
        has_more: found.has_more,
    }))
}

async fn get_definition(
    State(app): State<App>,
    Path((kind, name)): Path<(String, String)>,
) -> Result<Json<DefinitionView>, ApiError> {
    let key = DefinitionKey {
        kind: kind_named(&kind)?,
        name,
    };
    app.brain
        .workflows
        .store
        .get_definition(&key)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .map(|definition| Json(definition.into()))
        .ok_or_else(|| ApiError::not_found(&format!("there's no {kind} by that name")))
}

fn job_id_for(workflow: &str, key: Option<&str>) -> JobId {
    key.map_or_else(JobId::generate, |key| {
        let seed = format!("api|{workflow}|{key}");
        JobId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
    })
}

async fn start_run(
    State(app): State<App>,
    Path(workflow): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let raw = json_body(&headers, &body)?;
    let request: RunRequest = app
        .run_request
        .parse(raw)
        .map_err(|refusal| ApiError::refused(&refusal))?;
    app.brain
        .workflows
        .check_run(&workflow, &request.inputs)
        .await
        .map_err(|error| match error {
            WorkflowError::Unknown(_) => ApiError::not_found(&error.to_string()),
            WorkflowError::Invalid(_) => ApiError::invalid(&error.to_string()),
            other => ApiError::internal(&other),
        })?;
    let id = job_id_for(&workflow, request.idempotency_key.as_deref());
    let job = Job {
        id,
        kind: WORKFLOW_KIND.to_owned(),
        payload: json!({ "workflow": workflow, "inputs": request.inputs }),
        priority: request.priority,
        status: JobStatus::Queued,
        requested_by: Actor::new(OWNER),
        run_at: Utc::now(),
        attempts: 0,
        // A retry would repeat whatever the earlier steps did.
        max_attempts: 1,
        lease: None,
        last_error: None,
        result: None,
    };
    let enqueued = app
        .transport
        .enqueue(job)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let already_existed = enqueued == Enqueued::AlreadyThere;
    let status = if already_existed {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    let started = RunStarted {
        job_id: id.to_string(),
        already_existed,
    };
    Ok((status, Json(started)).into_response())
}

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/api/v1/jobs", get(list_jobs))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/definitions/{kind}", get(list_definitions))
        .route("/api/v1/definitions/{kind}/{name}", get(get_definition))
        .route("/api/v1/workflows/{name}/runs", post(start_run))
}
