//! The HTTP server: MCP, the `OpenAI`-compatible front door and the REST API,
//! all behind the owner key. Everything is checked against its schema before
//! it's used.

mod harness;
mod mcp;
mod openai;
mod rest;
mod runs;

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{CONTENT_TYPE, ORIGIN, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use plugin_api::McpHandler;
use schema::{Refusal, Validator, bounded};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use work::{JobTransport, StoreTransport};

use crate::Runtime;

pub use harness::{
    AttemptList, AttemptView, ChangeList, ChangeView, HoldList, HoldRequest, LessonList,
};
pub use openai::OpenAiRequest;
pub use rest::{
    DefinitionList, DefinitionView, JobList, JobStatusView, JobView, RunRequest, RunStarted,
};
pub use runs::{RunList, RunView};

/// The shortest owner key accepted.
const MIN_KEY_CHARS: usize = 32;
/// The largest request read, the same as the longest MCP message over stdio.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Whether an owner key is good enough to serve with.
///
/// # Errors
///
/// What's wrong with it.
pub(crate) fn check_owner_key(key: &str) -> Result<(), String> {
    if key.chars().count() < MIN_KEY_CHARS {
        return Err(format!(
            "set this to a secret of at least {MIN_KEY_CHARS} characters; \
             the server doesn't run without one"
        ));
    }
    Ok(())
}

/// The HTTP server couldn't be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The owner key can't be used.
    #[error("the owner key: {0}")]
    OwnerKey(String),
    /// A schema or a tool's schema couldn't be used, which is a bug.
    #[error("couldn't set up a schema: {0}")]
    Schema(String),
}

/// One thing wrong with a request.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProblemView {
    /// Where in the request, as a JSON pointer.
    #[schemars(length(max = 1024))]
    pub path: String,
    /// What's wrong there.
    #[schemars(length(max = 1024))]
    pub message: String,
}

/// Why the API refused or failed a request.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ApiError {
    #[serde(skip)]
    #[schemars(skip)]
    status: StatusCode,
    /// What sort of problem it is.
    #[schemars(length(max = 32))]
    pub kind: String,
    /// What went wrong.
    #[schemars(length(max = 2048))]
    pub message: String,
    /// Each thing wrong with the request, if it didn't meet its schema.
    #[schemars(length(max = 16))]
    pub problems: Vec<ProblemView>,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, kind: &str, message: &str) -> Self {
        Self {
            status,
            kind: kind.to_owned(),
            message: bounded(message),
            problems: Vec::new(),
        }
    }

    pub(super) fn invalid(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub(super) fn not_found(message: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub(super) fn internal(error: &impl std::fmt::Display) -> Self {
        eprintln!("request failed: {}", bounded(&error.to_string()));
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the server failed; its log says why",
        )
    }

    pub(super) fn refused(refusal: &Refusal) -> Self {
        let mut error = Self::invalid("the request doesn't meet its schema");
        error.problems = refusal
            .problems
            .iter()
            .map(|problem| ProblemView {
                path: bounded(&problem.path),
                message: bounded(&problem.message),
            })
            .collect();
        error
    }

    fn busy() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "too many requests are running; try again shortly",
        )
    }

    pub(super) const fn status(&self) -> StatusCode {
        self.status
    }

    pub(super) fn message(&self) -> &str {
        &self.message
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let mut response = (status, Json(&self)).into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("5"));
        }
        response
    }
}

/// What every request can reach.
#[derive(Clone)]
pub(super) struct App {
    runtime: Runtime,
    /// The one model the front door lists: the product's name.
    model_id: Arc<str>,
    mcp: Arc<McpHandler>,
    transport: Arc<dyn JobTransport>,
    permits: Arc<Semaphore>,
    key: [u8; 32],
    origins: Arc<BTreeSet<String>>,
    run_request: Arc<Validator>,
    openai_request: Arc<Validator>,
    improve_request: Arc<Validator>,
    hold_request: Arc<Validator>,
}

impl App {
    /// A place to run a chat or tool call, or a 503 if there are none.
    fn permit(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| ApiError::busy())
    }

    fn holds_the_key(&self, headers: &HeaderMap) -> bool {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, key)| key);
        presented.is_some_and(|key| bool::from(digest(key).ct_eq(&self.key)))
    }

    fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        headers.get(ORIGIN).is_none_or(|origin| {
            origin
                .to_str()
                .is_ok_and(|origin| self.origins.contains(origin))
        })
    }
}

fn digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// Runs `work` where a client hanging up can't stop it halfway, so a run
/// always finishes and is recorded. Its budget is what limits it.
async fn detached<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    work: impl Future<Output = T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::spawn(async move {
        let _held = permit;
        work.await
    })
    .await
    .map_err(|error| ApiError::internal(&error))
}

/// The body as JSON. Requiring the content type also stops a browser form
/// from reaching the API.
fn json_body(headers: &HeaderMap, body: &Bytes) -> Result<Value, ApiError> {
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
        });
    if !is_json {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request",
            "the content type must be application/json",
        ));
    }
    serde_json::from_slice(body).map_err(|_| ApiError::invalid("the body isn't valid JSON"))
}

/// Refuses anything without the key, and any browser origin not listed.
async fn guard(State(app): State<App>, request: Request, next: Next) -> Response {
    if !app.origin_allowed(request.headers()) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "this origin isn't allowed",
        )
        .into_response();
    }
    if !app.holds_the_key(request.headers()) {
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "send the owner key as a bearer token",
        )
        .into_response();
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    next.run(request).await
}

async fn health() -> Json<Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn nowhere() -> ApiError {
    ApiError::not_found("there's nothing at this path")
}

fn validator<T: JsonSchema>() -> Result<Arc<Validator>, HttpError> {
    Validator::for_type::<T>()
        .map(Arc::new)
        .map_err(|error| HttpError::Schema(error.to_string()))
}

impl Runtime {
    /// The HTTP server, ready to be served. Only `/health` works without the
    /// key.
    ///
    /// # Errors
    ///
    /// [`HttpError`] if the key is too short or a schema can't be used.
    pub fn http_router(
        &self,
        product: &str,
        version: &str,
        owner_key: &str,
    ) -> Result<Router, HttpError> {
        check_owner_key(owner_key).map_err(HttpError::OwnerKey)?;
        let mcp = McpHandler::new(product, version, self.mcp_tools())
            .map_err(|error| HttpError::Schema(error.to_string()))?;
        let app = App {
            runtime: self.clone(),
            model_id: Arc::from(product),
            mcp: Arc::new(mcp),
            transport: Arc::new(StoreTransport(Arc::clone(&self.workflows.store))),
            permits: Arc::new(Semaphore::new(
                self.config.server.max_concurrent_requests as usize,
            )),
            key: digest(owner_key),
            origins: Arc::new(self.config.server.allowed_origins.iter().cloned().collect()),
            run_request: validator::<RunRequest>()?,
            openai_request: validator::<OpenAiRequest>()?,
            improve_request: validator::<::harness::Request>()?,
            hold_request: validator::<harness::HoldRequest>()?,
        };
        let guarded = Router::new()
            .merge(mcp::routes())
            .merge(openai::routes())
            .merge(rest::routes())
            .merge(runs::routes())
            .merge(harness::routes())
            .fallback(nowhere)
            .layer(middleware::from_fn_with_state(app.clone(), guard));
        Ok(Router::new()
            .route("/health", get(health))
            .merge(guarded)
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(app))
    }
}
