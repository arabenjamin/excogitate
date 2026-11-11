//! Harness² over REST: start an attempt, and read what attempts found.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use harness::{Attempt, Request};
use protocol::Actor;
use schemars::JsonSchema;
use serde::Serialize;
use store::{Job, JobId, JobStatus, PageRequest, RecordId};
use uuid::Uuid;

use super::rest::{DEFAULT_LIMIT, MAX_LIMIT, Params, RunStarted, number, only};
use super::{ApiError, App, json_body};
use crate::harness::HARNESS_KIND;

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
    let known = app
        .brain
        .workflows
        .agent(&request.agent)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    if known.is_none() {
        return Err(ApiError::not_found(&format!(
            "there is no agent {}",
            request.agent
        )));
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
    let limit = number(&params, "limit", DEFAULT_LIMIT)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(ApiError::invalid(&format!(
            "limit must be 1 to {MAX_LIMIT}"
        )));
    }
    let page = PageRequest::new(limit, number(&params, "offset", 0)?)
        .map_err(|error| ApiError::invalid(&error.to_string()))?;
    let found = app
        .brain
        .attempts(status, page)
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
        .brain
        .attempt(id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(|| ApiError::not_found("there's no attempt with that id"))?;
    view(id, data).map(Json)
}

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/api/v1/harness/attempts", post(start).get(list))
        .route("/api/v1/harness/attempts/{id}", get(get_one))
}
