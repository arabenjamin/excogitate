//! Runs and their trajectories, as Harness² and a person read them.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, put};
use chrono::{DateTime, Utc};
use dispatch::{Trajectory, read_trajectory};
use events::{RunQuery, RunSummary};
use protocol::RunId;
use schemars::JsonSchema;
use serde::Serialize;

use super::rest::{DEFAULT_LIMIT, MAX_LIMIT, Params, number, only};
use schema::schema_of;

use super::spec::{Body, LIMIT, OFFSET, Param, Route};
use super::{ApiError, App, json_body};

/// The most steps of one trajectory served. A run's budget caps its calls at
/// 512 each, so a whole run fits.
const MAX_STEPS: usize = 2048;
/// The longest agent or harness name a query may give.
const MAX_FILTER_CHARS: usize = 128;

/// A run, as the API lists it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RunView {
    /// Its id, which `GET /api/v1/runs/{id}` takes.
    #[schemars(length(max = 36))]
    pub run: String,
    /// What ran: `chat`, `agent:<name>` or `tool:<name>`.
    #[schemars(length(max = 128))]
    pub agent: String,
    /// The harness it ran under. Runs with the same id had the same
    /// instructions, tools and limits.
    #[schemars(length(max = 64))]
    pub harness: String,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it ended. None while it runs, or if it was cut short.
    pub ended_at: Option<DateTime<Utc>>,
    /// How it ended.
    #[schemars(length(max = 32))]
    pub outcome: Option<String>,
}

impl From<RunSummary> for RunView {
    fn from(run: RunSummary) -> Self {
        Self {
            run: run.run.to_string(),
            agent: run.agent,
            harness: run.harness,
            started_at: run.started_at,
            ended_at: run.ended_at,
            outcome: run.outcome,
        }
    }
}

/// One page of runs, newest first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RunList {
    /// The runs.
    #[schemars(length(max = MAX_LIMIT))]
    pub items: Vec<RunView>,
    /// True if there are more past this page.
    pub has_more: bool,
}

fn filter(
    params: &std::collections::BTreeMap<String, String>,
    name: &str,
) -> Result<Option<String>, ApiError> {
    match params.get(name) {
        Some(value) if value.is_empty() || value.chars().count() > MAX_FILTER_CHARS => {
            Err(ApiError::invalid(&format!(
                "{name} must be 1 to {MAX_FILTER_CHARS} characters"
            )))
        }
        other => Ok(other.cloned()),
    }
}

async fn list_runs(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<RunList>, ApiError> {
    only(&params, &["agent", "harness", "limit", "offset"])?;
    let limit = number(&params, "limit", DEFAULT_LIMIT)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(ApiError::invalid(&format!(
            "limit must be 1 to {MAX_LIMIT}"
        )));
    }
    let query = RunQuery {
        agent: filter(&params, "agent")?,
        harness: filter(&params, "harness")?,
        offset: usize::try_from(number(&params, "offset", 0)?).unwrap_or(usize::MAX),
        limit: usize::try_from(limit).unwrap_or(usize::MAX),
    };
    let found = app
        .runtime
        .sink
        .list_runs(&query)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(Json(RunList {
        items: found.runs.into_iter().map(RunView::from).collect(),
        has_more: found.has_more,
    }))
}

async fn get_run(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Trajectory>, ApiError> {
    let run: RunId = id
        .parse()
        .map_err(|error: protocol::NotARunId| ApiError::invalid(&error.to_string()))?;
    read_trajectory(app.runtime.sink.as_ref(), run, MAX_STEPS)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("there's no run with that id"))
}

/// Keeps the owner's feedback on a run, replacing any it had. A run marked
/// bad is evidence for Harness²'s picker on its own.
async fn put_feedback(
    State(app): State<App>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<harness::Feedback>, ApiError> {
    let run: RunId = id
        .parse()
        .map_err(|error: protocol::NotARunId| ApiError::invalid(&error.to_string()))?;
    let raw = json_body(&headers, &body)?;
    let request: harness::FeedbackRequest = app
        .feedback_request
        .parse(raw)
        .map_err(|refusal| ApiError::refused(&refusal))?;
    let found = read_trajectory(app.runtime.sink.as_ref(), run, 1)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(|| ApiError::not_found("there's no run with that id"))?;
    let agent = found.agent.unwrap_or_default();
    Ok(Json(
        app.runtime
            .set_feedback(&run.to_string(), &agent, request)
            .await?,
    ))
}

const AGENT: Param = Param {
    name: "agent",
    about: "Only runs of this agent.",
    values: &[],
    number: false,
};
const HARNESS: Param = Param {
    name: "harness",
    about: "Only runs under this harness id.",
    values: &[],
    number: false,
};

pub(super) fn routes() -> Vec<Route> {
    vec![
        Route::get(
            "list_runs",
            "/api/v1/runs",
            "The runs, newest first",
            get(list_runs),
        )
        .query(&[AGENT, HARNESS, LIMIT, OFFSET])
        .reply(200, "A page of runs.", Body::Typed(schema_of::<RunList>)),
        Route::get(
            "get_run",
            "/api/v1/runs/{id}",
            "One run's trajectory",
            get(get_run),
        )
        .reply(200, "Its trajectory.", Body::Typed(schema_of::<Trajectory>)),
        Route::put(
            "put_feedback",
            "/api/v1/runs/{id}/feedback",
            "Mark a run good or bad, for Harness²",
            put(put_feedback),
        )
        .body(Body::Typed(schema_of::<harness::FeedbackRequest>))
        .reply(
            200,
            "The feedback, as kept.",
            Body::Typed(schema_of::<harness::Feedback>),
        ),
    ]
}
