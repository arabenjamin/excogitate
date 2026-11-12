//! Harness² over REST: start an attempt, and read what attempts found.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use harness::{Attempt, Change, Hold, Lesson, LessonStatus, Request};
use protocol::Actor;
use schemars::JsonSchema;
use serde::Serialize;
use store::{Job, JobId, JobStatus, PageRequest, RecordId};
use uuid::Uuid;

use super::rest::{DEFAULT_LIMIT, MAX_LIMIT, Params, RunStarted, number, only};
use super::{ApiError, App, json_body};
use crate::harness::{ChangeError, HARNESS_KIND};

/// Who a request came from, until there are users.
const OWNER: &str = "owner";

/// A kept attempt, as the API shows it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AttemptView {
    /// Its id.
    #[schemars(length(max = 36))]
    pub id: String,
    /// What it found.
    pub attempt: Attempt,
}

/// One page of attempts, oldest first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AttemptList {
    /// The attempts.
    #[schemars(length(max = MAX_LIMIT))]
    pub items: Vec<AttemptView>,
    /// True if there are more past this page.
    pub has_more: bool,
}

fn view(id: RecordId, data: serde_json::Value) -> Result<AttemptView, ApiError> {
    Ok(AttemptView {
        id: id.to_string(),
        attempt: serde_json::from_value(data).map_err(|error| ApiError::internal(&error))?,
    })
}

async fn start(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let raw = json_body(&headers, &body)?;
    let request: Request = app
        .improve_request
        .parse(raw)
        .map_err(|refusal| ApiError::refused(&refusal))?;
    let engine = &app.runtime.workflows;
    let known = match request.target {
        harness::Target::Agent => engine
            .agent(&request.agent)
            .await
            .map(|found| found.is_some()),
        harness::Target::Workflow => engine
            .workflow(&request.agent)
            .await
            .map(|found| found.is_some()),
    }
    .map_err(|error| ApiError::internal(&error))?;
    if !known {
        return Err(ApiError::not_found(&format!(
            "there is no {} {}",
            request.target.word(),
            request.agent
        )));
    }
    if request.target == harness::Target::Workflow
        && !serde_json::from_str::<serde_json::Value>(&request.task)
            .is_ok_and(|inputs| inputs.is_object())
    {
        return Err(ApiError::invalid(
            "a workflow's task is the inputs to run it on, as a JSON object",
        ));
    }
    let id = JobId::generate();
    let job = Job {
        id,
        kind: HARNESS_KIND.to_owned(),
        payload: serde_json::to_value(&request).map_err(|error| ApiError::internal(&error))?,
        priority: 0,
        status: JobStatus::Queued,
        requested_by: Actor::new(OWNER),
        run_at: Utc::now(),
        attempts: 0,
        max_attempts: 1,
        lease: None,
        last_error: None,
        result: None,
        finished_at: None,
    };
    app.transport
        .enqueue(job)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let started = RunStarted {
        job_id: id.to_string(),
        already_existed: false,
    };
    Ok((StatusCode::ACCEPTED, Json(started)).into_response())
}

async fn list(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<AttemptList>, ApiError> {
    only(&params, &["status", "limit", "offset"])?;
    let status = params.get("status").map(String::as_str);
    if let Some(status) = status
        && !matches!(status, "kept" | "supported" | "failed")
    {
        return Err(ApiError::invalid(
            "status must be kept, supported or failed",
        ));
    }
    let found = app
        .runtime
        .attempts(status, page_of(&params)?)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let items = found
        .items
        .into_iter()
        .map(|(id, data)| view(id, data))
        .collect::<Result<_, _>>()?;
    Ok(Json(AttemptList {
        items,
        has_more: found.has_more,
    }))
}

async fn get_one(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<AttemptView>, ApiError> {
    let id = Uuid::parse_str(&id)
        .map(RecordId::from_uuid)
        .map_err(|_| ApiError::invalid("an attempt id is a UUID"))?;
    let data = app
        .runtime
        .attempt(id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(|| ApiError::not_found("there's no attempt with that id"))?;
    view(id, data).map(Json)
}

/// A change the system made to an agent, as the API shows it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ChangeView {
    /// Its id, which reverting it takes.
    #[schemars(length(max = 36))]
    pub id: String,
    /// The change.
    pub change: Change,
}

/// One page of changes, oldest first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ChangeList {
    /// The changes.
    #[schemars(length(max = MAX_LIMIT))]
    pub items: Vec<ChangeView>,
    /// True if there are more past this page.
    pub has_more: bool,
}

/// One page of the lesson library, oldest first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LessonList {
    /// The lessons.
    #[schemars(length(max = MAX_LIMIT))]
    pub items: Vec<Lesson>,
    /// True if there are more past this page.
    pub has_more: bool,
}

/// The agents the system leaves alone.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct HoldList {
    /// The holds.
    #[schemars(length(max = MAX_LIMIT))]
    pub items: Vec<Hold>,
    /// True if there are more past this page.
    pub has_more: bool,
}

/// Holds an agent, or a workflow.
#[derive(Debug, Clone, serde::Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HoldRequest {
    /// Why, for whoever reads the hold later.
    #[schemars(length(min = 1, max = 2048))]
    pub reason: String,
    /// What's held: an agent unless it says `workflow`.
    #[serde(default, skip_serializing_if = "harness::Target::is_agent")]
    pub target: harness::Target,
}

/// The `target` query parameter: an agent unless it says `workflow`.
fn target_of(
    params: &std::collections::BTreeMap<String, String>,
) -> Result<harness::Target, ApiError> {
    match params.get("target").map(String::as_str) {
        None | Some("agent") => Ok(harness::Target::Agent),
        Some("workflow") => Ok(harness::Target::Workflow),
        Some(_) => Err(ApiError::invalid("target is agent or workflow")),
    }
}

impl From<ChangeError> for ApiError {
    fn from(error: ChangeError) -> Self {
        match error {
            ChangeError::NotFound(why) => Self::not_found(&why),
            ChangeError::Conflict(why) => Self::new(StatusCode::CONFLICT, "conflict", &why),
            ChangeError::Internal(why) => Self::internal(&why),
        }
    }
}

fn page_of(params: &std::collections::BTreeMap<String, String>) -> Result<PageRequest, ApiError> {
    let limit = number(params, "limit", DEFAULT_LIMIT)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(ApiError::invalid(&format!(
            "limit must be 1 to {MAX_LIMIT}"
        )));
    }
    PageRequest::new(limit, number(params, "offset", 0)?)
        .map_err(|error| ApiError::invalid(&error.to_string()))
}

fn change_id(id: &str) -> Result<RecordId, ApiError> {
    Uuid::parse_str(id)
        .map(RecordId::from_uuid)
        .map_err(|_| ApiError::invalid("a change id is a UUID"))
}

fn agent_name(name: &str) -> Result<&str, ApiError> {
    let fits = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if fits {
        Ok(name)
    } else {
        Err(ApiError::invalid(
            "an agent's name is 1 to 64 of a-z, 0-9, _ and -",
        ))
    }
}

async fn list_changes(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<ChangeList>, ApiError> {
    only(&params, &["agent", "status", "limit", "offset"])?;
    let agent = params
        .get("agent")
        .map(|name| agent_name(name))
        .transpose()?;
    let status = params.get("status").map(String::as_str);
    if let Some(status) = status
        && !matches!(
            status,
            "pending" | "abandoned" | "live" | "reverted" | "replaced"
        )
    {
        return Err(ApiError::invalid(
            "status must be pending, abandoned, live, reverted or replaced",
        ));
    }
    let found = app
        .runtime
        .changes(agent, status, page_of(&params)?)
        .await?;
    Ok(Json(ChangeList {
        items: found
            .items
            .into_iter()
            .map(|(id, change)| ChangeView {
                id: id.to_string(),
                change,
            })
            .collect(),
        has_more: found.has_more,
    }))
}

async fn get_change(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<ChangeView>, ApiError> {
    let id = change_id(&id)?;
    let change = app.runtime.change(id).await?;
    Ok(Json(ChangeView {
        id: id.to_string(),
        change,
    }))
}

async fn revert(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<ChangeView>, ApiError> {
    let id = change_id(&id)?;
    let change = app.runtime.revert(id).await?;
    Ok(Json(ChangeView {
        id: id.to_string(),
        change,
    }))
}

async fn list_lessons(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<LessonList>, ApiError> {
    only(&params, &["agent", "status", "limit", "offset"])?;
    let agent = params
        .get("agent")
        .map(|name| agent_name(name))
        .transpose()?;
    let status = match params.get("status").map(String::as_str) {
        None => None,
        Some("active") => Some(LessonStatus::Active),
        Some("retired") => Some(LessonStatus::Retired),
        Some(_) => return Err(ApiError::invalid("status must be active or retired")),
    };
    let found = app
        .runtime
        .lessons(agent, status, page_of(&params)?)
        .await?;
    Ok(Json(LessonList {
        items: found.items,
        has_more: found.has_more,
    }))
}

async fn list_holds(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<HoldList>, ApiError> {
    only(&params, &["limit", "offset"])?;
    let found = app.runtime.holds(page_of(&params)?).await?;
    Ok(Json(HoldList {
        items: found.items,
        has_more: found.has_more,
    }))
}

async fn hold(
    State(app): State<App>,
    Path(agent): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Hold>, ApiError> {
    let agent = agent_name(&agent)?;
    let raw = json_body(&headers, &body)?;
    let request: HoldRequest = app
        .hold_request
        .parse(raw)
        .map_err(|refusal| ApiError::refused(&refusal))?;
    let held = app
        .runtime
        .set_hold(agent, request.target, Some(request.reason))
        .await?;
    held.map(Json)
        .ok_or_else(|| ApiError::internal(&"the hold wasn't kept"))
}

async fn release(
    State(app): State<App>,
    Path(agent): Path<String>,
    Query(params): Params,
) -> Result<StatusCode, ApiError> {
    only(&params, &["target"])?;
    let agent = agent_name(&agent)?;
    app.runtime
        .set_hold(agent, target_of(&params)?, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/api/v1/harness/attempts", post(start).get(list))
        .route("/api/v1/harness/attempts/{id}", get(get_one))
        .route("/api/v1/harness/changes", get(list_changes))
        .route("/api/v1/harness/changes/{id}", get(get_change))
        .route("/api/v1/harness/changes/{id}/revert", post(revert))
        .route("/api/v1/harness/lessons", get(list_lessons))
        .route("/api/v1/harness/holds", get(list_holds))
        .route("/api/v1/harness/holds/{agent}", put(hold).delete(release))
}
