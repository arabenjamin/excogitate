//! Runs and their trajectories, as Harness² and a person read them.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use dispatch::{Trajectory, read_trajectory};
use events::{RunQuery, RunSummary};
use protocol::RunId;
use schemars::JsonSchema;
use serde::Serialize;

use super::rest::{DEFAULT_LIMIT, MAX_LIMIT, Params, number, only};
use super::{ApiError, App};

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

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/api/v1/runs", get(list_runs))
        .route("/api/v1/runs/{id}", get(get_run))
}
