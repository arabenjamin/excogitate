//! Harness² over REST: start an attempt, and read what attempts found.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json};
use chrono::Utc;
use harness::{Attempt, Change, Hold, Lesson, LessonStatus, Request};
use schemars::JsonSchema;
use serde::Serialize;
use store::{Job, JobId, JobStatus, PageRequest, RecordId};
use uuid::Uuid;

use super::rest::{DEFAULT_LIMIT, MAX_LIMIT, Params, RunStarted, number, only};
use schema::schema_of;

use super::spec::{Body, ID, Kind, LIMIT, OFFSET, Param, Route};
use super::{ApiError, App, json_body};
use crate::auth::Caller;
use crate::harness::{ChangeError, HARNESS_KIND};

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
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let raw = json_body(&headers, &body)?;
    let request: Request = match app.improve_request.parse(raw.clone()) {
        Ok(request) => request,
        Err(refusal) => {
            // The schema states the rules `check` does; when one of them is
            // what's broken, its sentence says it better than a pointer.
            let mut refused = ApiError::refused(&refusal);
            if let Ok(request) = serde_json::from_value::<Request>(raw)
                && let Err(why) = request.check()
            {
                refused.message = schema::bounded(&why);
            }
            return Err(refused);
        }
    };
    request.check().map_err(|why| ApiError::invalid(&why))?;
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

    let id = JobId::generate();
    let job = Job {
        id,
        kind: HARNESS_KIND.to_owned(),
        payload: serde_json::to_value(&request).map_err(|error| ApiError::internal(&error))?,
        priority: 0,
        status: JobStatus::Queued,
        requested_by: caller.actor.clone(),
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

/// Holds an agent, or a workflow with `?target=workflow`, as a release
/// says it.
#[derive(Debug, Clone, serde::Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HoldRequest {
    /// Why, for whoever reads the hold later.
    #[schemars(length(min = 1, max = 2048))]
    pub reason: String,
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
    Query(params): Params,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Hold>, ApiError> {
    only(&params, &["target"])?;
    let target = target_of(&params)?;
    let agent = agent_name(&agent)?;
    let raw = json_body(&headers, &body)?;
    let request: HoldRequest = app
        .hold_request
        .parse(raw)
        .map_err(|refusal| ApiError::refused(&refusal))?;
    let held = app
        .runtime
        .set_hold(agent, target, Some(request.reason))
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

const AGENT: Param = Param {
    name: "agent",
    about: "Only this agent's.",
    kind: Kind::Name,
};
const AGENT_PATH: Param = Param {
    name: "agent",
    about: "The agent, or the workflow when target is workflow.",
    kind: Kind::Name,
};
const TARGET: Param = Param {
    name: "target",
    about: "Whether the name is an agent's or a workflow's. agent if left out.",
    kind: Kind::OneOf(&["agent", "workflow"]),
};

pub(super) fn routes() -> Vec<Route> {
    let mut routes = vec![
        Route::post(
            "start_attempt",
            "/api/v1/harness/attempts",
            "Start a Harness² attempt, as a job",
            post(start),
        )
        .body(Body::Typed(schema_of::<Request>))
        .reply(
            202,
            "The job was queued.",
            Body::Typed(schema_of::<RunStarted>),
        ),
        Route::get(
            "list_attempts",
            "/api/v1/harness/attempts",
            "The attempts, newest first",
            get(list),
        )
        .query(&[
            Param {
                name: "status",
                about: "Only attempts that ended this way.",
                kind: Kind::OneOf(&["kept", "supported", "failed"]),
            },
            LIMIT,
            OFFSET,
        ])
        .reply(
            200,
            "A page of attempts.",
            Body::Typed(schema_of::<AttemptList>),
        ),
        Route::get(
            "get_attempt",
            "/api/v1/harness/attempts/{id}",
            "One attempt",
            get(get_one),
        )
        .path(&[ID])
        .reply(200, "The attempt.", Body::Typed(schema_of::<AttemptView>)),
    ];
    routes.extend(change_routes());
    routes
}

fn change_routes() -> Vec<Route> {
    vec![
        Route::get(
            "list_changes",
            "/api/v1/harness/changes",
            "The changes Harness² made or proposed",
            get(list_changes),
        )
        .query(&[
            AGENT,
            Param {
                name: "status",
                about: "Only changes in this state.",
                kind: Kind::OneOf(&["pending", "abandoned", "live", "reverted", "replaced"]),
            },
            LIMIT,
            OFFSET,
        ])
        .reply(
            200,
            "A page of changes.",
            Body::Typed(schema_of::<ChangeList>),
        ),
        Route::get(
            "get_change",
            "/api/v1/harness/changes/{id}",
            "One change",
            get(get_change),
        )
        .path(&[ID])
        .reply(200, "The change.", Body::Typed(schema_of::<ChangeView>)),
        Route::post(
            "revert_change",
            "/api/v1/harness/changes/{id}/revert",
            "Revert a change",
            post(revert),
        )
        .path(&[ID])
        .reply(
            200,
            "The change, reverted.",
            Body::Typed(schema_of::<ChangeView>),
        ),
        Route::get(
            "list_lessons",
            "/api/v1/harness/lessons",
            "The lessons Harness² keeps",
            get(list_lessons),
        )
        .query(&[
            AGENT,
            Param {
                name: "status",
                about: "Only lessons in this state.",
                kind: Kind::OneOf(&["active", "retired"]),
            },
            LIMIT,
            OFFSET,
        ])
        .reply(
            200,
            "A page of lessons.",
            Body::Typed(schema_of::<LessonList>),
        ),
        Route::get(
            "list_holds",
            "/api/v1/harness/holds",
            "The agents and workflows Harness² leaves alone",
            get(list_holds),
        )
        .query(&[LIMIT, OFFSET])
        .reply(200, "A page of holds.", Body::Typed(schema_of::<HoldList>)),
        Route::put(
            "hold",
            "/api/v1/harness/holds/{agent}",
            "Have Harness² leave an agent or workflow alone",
            put(hold),
        )
        .path(&[AGENT_PATH])
        .query(&[TARGET])
        .body(Body::Typed(schema_of::<HoldRequest>))
        .reply(200, "The hold, as kept.", Body::Typed(schema_of::<Hold>)),
        Route::delete(
            "release",
            "/api/v1/harness/holds/{agent}",
            "Release a hold",
            delete(release),
        )
        .path(&[AGENT_PATH])
        .query(&[TARGET])
        .reply(204, "Released.", Body::Empty),
    ]
}
