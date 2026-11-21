//! The HTTP server: MCP, the `OpenAI`-compatible front door and the REST API,
//! all behind a key: the owner key, or a key made for a person, whose scopes
//! each route checks. Everything is checked against its schema before it's
//! used.

mod harness;
mod mcp;
mod openai;
mod people;
mod rest;
mod runs;
mod spec;
mod tools;

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN, RETRY_AFTER, WWW_AUTHENTICATE};
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
use store::{RecordId, StoreError};
use subtle::ConstantTimeEq;
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};
use work::{JobTransport, StoreTransport};

use crate::Runtime;
use crate::auth::{self, Caller, Scope};
use crate::metrics::prefix_for;

pub use harness::{
    AttemptList, AttemptView, ChangeList, ChangeView, HoldList, HoldRequest, LessonList,
};
pub use openai::OpenAiRequest;
pub use rest::{
    DefinitionList, DefinitionView, JobList, JobStatusView, JobView, RunRequest, RunStarted,
};
pub use runs::{RunList, RunView};
pub use tools::ToolFailureView;

/// The shortest owner key accepted.
const MIN_KEY_CHARS: usize = 32;
/// The largest request read, the same as the longest MCP message over stdio.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// How often a key's last use is written, at most, so a busy client doesn't
/// turn every read into a write.
const USE_EVERY: Duration = Duration::from_secs(60);

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

    pub(super) fn forbidden(message: &str) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    fn unauthorized(message: &str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
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
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
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
    /// The `OpenAPI` document, built at start with every installed tool.
    openapi: Arc<Value>,
    transport: Arc<dyn JobTransport>,
    permits: Arc<Semaphore>,
    key: [u8; 32],
    origins: Arc<BTreeSet<String>>,
    run_request: Arc<Validator>,
    openai_request: Arc<Validator>,
    improve_request: Arc<Validator>,
    hold_request: Arc<Validator>,
    feedback_request: Arc<Validator>,
    /// What an MCP message is held to: the schema the document gives.
    mcp_message: Arc<Validator>,
    person_request: Arc<Validator>,
    key_request: Arc<Validator>,
    /// Set once the store knows the record types people and keys are kept as.
    auth_ready: Arc<OnceCell<()>>,
    /// When each key's use was last written.
    uses: Arc<Mutex<HashMap<RecordId, Instant>>>,
}

impl App {
    /// A place to run a chat or tool call, or a 503 if there are none.
    fn permit(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        Arc::clone(&self.permits)
            .try_acquire_owned()
            .map_err(|_| ApiError::busy())
    }

    fn is_owner_key(&self, token: &str) -> bool {
        bool::from(digest(token).ct_eq(&self.key))
    }

    /// Makes sure the store knows how people and keys are kept.
    async fn auth_ready(&self) -> Result<(), StoreError> {
        self.auth_ready
            .get_or_try_init(|| async {
                self.runtime
                    .workflows
                    .store
                    .register_records(&auth::declarations())
                    .await
            })
            .await
            .copied()
    }

    /// Who a bearer token is, or why it's refused.
    async fn caller(&self, token: Option<&str>) -> Result<Caller, ApiError> {
        let Some(token) = token else {
            return Err(ApiError::unauthorized(
                "send the owner key or a person's key as a bearer token",
            ));
        };
        if self.is_owner_key(token) {
            return Ok(Caller::owner());
        }
        if !auth::is_key(token) {
            return Err(ApiError::unauthorized(auth::Refused::Unknown.message()));
        }
        self.auth_ready()
            .await
            .map_err(|error| ApiError::internal(&error))?;
        let store = self.runtime.workflows.store.as_ref();
        let caller = auth::authenticate(store, token, chrono::Utc::now())
            .await
            .map_err(|error| ApiError::internal(&error))?
            .map_err(|refused| ApiError::unauthorized(refused.message()))?;
        if let Some(key) = caller.key {
            self.note_use(key);
        }
        Ok(caller)
    }

    /// Writes that `key` was used, unless it was written less than a minute
    /// ago. It's written apart from the request, which doesn't wait on it.
    fn note_use(&self, key: RecordId) {
        let now = Instant::now();
        {
            let mut uses = self.uses.lock().unwrap_or_else(PoisonError::into_inner);
            if uses
                .get(&key)
                .is_some_and(|last| now.duration_since(*last) < USE_EVERY)
            {
                return;
            }
            uses.insert(key, now);
        }
        let store = Arc::clone(&self.runtime.workflows.store);
        tokio::spawn(async move {
            if let Err(error) = auth::note_use(store.as_ref(), key).await {
                eprintln!("couldn't note a key's use: {}", bounded(&error.to_string()));
            }
        });
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

/// The bearer token a request carries, if it has one.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, key)| key.trim())
}

/// Refuses anything without a key that works, and any browser origin not
/// listed. What it lets through carries its [`Caller`].
async fn guard(State(app): State<App>, mut request: Request, next: Next) -> Response {
    if !app.origin_allowed(request.headers()) {
        return ApiError::forbidden("this origin isn't allowed").into_response();
    }
    match app.caller(bearer(request.headers())).await {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

/// Refuses a request whose caller lacks what its route needs. A route that
/// declares nothing is refused, so a new one fails closed.
async fn require(State(need): State<Option<spec::Need>>, request: Request, next: Next) -> Response {
    let Some(caller) = request.extensions().get::<Caller>() else {
        return ApiError::internal(&"a guarded request had no caller").into_response();
    };
    let allowed = match need {
        None => Err("this route declares no scope, so no key may use it".to_owned()),
        Some(spec::Need::Scope(scope)) => caller.may(scope),
        Some(spec::Need::Owner) if !caller.is_owner() => {
            Err("only the owner key manages people and keys".to_owned())
        }
        Some(spec::Need::Owner | spec::Need::Key | spec::Need::PerTool) => Ok(()),
    };
    match allowed {
        Ok(()) => next.run(request).await,
        Err(message) => ApiError::forbidden(&message).into_response(),
    }
}

/// The scope a tool call needs: `chat` for chat, `read` for a call that
/// only reads, and `run` for one that changes something.
pub(super) fn scope_for_tool(name: &str, effect: plugin_api::Effect) -> Scope {
    match (name, effect) {
        ("chat", _) => Scope::Chat,
        (_, plugin_api::Effect::Read) => Scope::Read,
        _ => Scope::Run,
    }
}

/// What `/health` says.
fn health_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["status"],
        "properties": { "status": { "const": "ok" } },
        "additionalProperties": false
    })
}

/// The longest refusal text kept from the router's own replies.
const MAX_ROUTER_TEXT: usize = 2048;

/// Rewrites any error reply that isn't JSON, such as the router's own
/// refusal of a method or of a body that's too large, as an [`ApiError`],
/// so every error a client sees has the one documented shape.
async fn json_errors(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !(status.is_client_error() || status.is_server_error()) || is_json {
        return response;
    }
    let (parts, body) = response.into_parts();
    let text = axum::body::to_bytes(body, MAX_ROUTER_TEXT)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
        .unwrap_or_default();
    let kind = match status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "too_large",
        status if status.is_client_error() => "invalid_request",
        _ => "internal",
    };
    let message = if text.is_empty() {
        status.canonical_reason().unwrap_or("refused").to_owned()
    } else {
        text
    };
    let mut rewritten = ApiError::new(status, kind, &message).into_response();
    // Keep what the router said beside it, such as which methods it allows.
    for (name, value) in &parts.headers {
        if name != CONTENT_TYPE && name != axum::http::header::CONTENT_LENGTH {
            rewritten.headers_mut().insert(name.clone(), value.clone());
        }
    }
    rewritten
}

async fn health() -> Json<Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// The metrics, with the jobs and spend gauges read now.
async fn metrics_page(State(app): State<App>) -> Response {
    let metrics = &app.runtime.metrics;
    metrics
        .read_jobs(app.runtime.workflows.store.as_ref())
        .await;
    metrics.set_spend(&app.runtime.spend_today().await);
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
        )],
        metrics.render(&prefix_for(&app.model_id)),
    )
        .into_response()
}

async fn timing(State(app): State<App>, request: Request, next: Next) -> Response {
    crate::metrics::timed(Arc::clone(&app.runtime.metrics), request, next).await
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
    /// key, and `/metrics` too when `server.open_metrics` is set.
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
        let routes = all_routes();
        let openapi = spec::document(
            &routes,
            Some(&scoped_listing(&mcp)),
            spec::About {
                title: product,
                version,
                server: "/",
                license: None,
                model: Some(product),
            },
        )
        .map_err(HttpError::Schema)?;
        let app = App {
            runtime: self.clone(),
            model_id: Arc::from(product),
            mcp: Arc::new(mcp),
            openapi: Arc::new(openapi),
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
            feedback_request: validator::<::harness::FeedbackRequest>()?,
            mcp_message: Arc::new(
                Validator::from_schema(&mcp::message_schema())
                    .map_err(|error| HttpError::Schema(error.to_string()))?,
            ),
            person_request: validator::<auth::PersonRequest>()?,
            key_request: validator::<auth::KeyRequest>()?,
            auth_ready: Arc::new(OnceCell::new()),
            uses: Arc::new(Mutex::new(HashMap::new())),
        };
        let (mut open, mut guarded) = (Router::new(), Router::new());
        for route in routes {
            let open_route = match route.access {
                spec::Access::Open => true,
                spec::Access::Metrics => self.config.server.open_metrics,
                spec::Access::Keyed => false,
            };
            if open_route {
                open = open.route(route.path, route.handler);
            } else {
                let checked = route
                    .handler
                    .route_layer(middleware::from_fn_with_state(route.need, require));
                guarded = guarded.route(route.path, checked);
            }
        }
        let guarded = guarded
            .fallback(nowhere)
            .layer(middleware::from_fn_with_state(app.clone(), guard));
        Ok(Router::new()
            .merge(open)
            .merge(guarded)
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            // Inside the limit, so its refusal is rewritten too.
            .layer(middleware::from_fn(json_errors))
            .layer(middleware::from_fn_with_state(app.clone(), timing))
            .with_state(app))
    }
}

/// Every route the server has, each with how the document describes it.
fn all_routes() -> Vec<spec::Route> {
    let mut routes = vec![
        spec::Route::get("health", "/health", "Whether the server is up", get(health))
            .access(spec::Access::Open)
            .reply(200, "It's up.", spec::Body::Inline(health_schema)),
        spec::Route::get(
            "metrics",
            "/metrics",
            "Metrics, in the OpenMetrics text format",
            get(metrics_page),
        )
        .access(spec::Access::Metrics)
        .reply(
            200,
            "The metrics, in the OpenMetrics text format.",
            spec::Body::Text("application/openmetrics-text"),
        ),
    ];
    routes.extend(mcp::routes());
    routes.extend(openai::routes());
    routes.extend(rest::routes());
    routes.extend(runs::routes());
    routes.extend(harness::routes());
    routes.extend(tools::routes());
    routes.extend(people::routes());
    for route in &mut routes {
        route.need = NEEDS
            .iter()
            .find(|(id, _)| *id == route.id)
            .map(|(_, need)| *need);
    }
    routes
}

/// What each keyed route needs of the caller's key, by route id: the one
/// place to read who may do what. A route left out is refused to every key.
const NEEDS: &[(&str, spec::Need)] = {
    use Scope::{Admin, Chat, Read, Run};
    use spec::Need::{Key, Owner, PerTool, Scope as Has};
    &[
        ("metrics", Has(Read)),
        ("openapi", Has(Read)),
        ("list_models", Has(Read)),
        ("list_jobs", Has(Read)),
        ("get_job", Has(Read)),
        ("list_definitions", Has(Read)),
        ("get_definition", Has(Read)),
        ("list_runs", Has(Read)),
        ("get_run", Has(Read)),
        ("list_attempts", Has(Read)),
        ("get_attempt", Has(Read)),
        ("list_changes", Has(Read)),
        ("get_change", Has(Read)),
        ("list_lessons", Has(Read)),
        ("list_holds", Has(Read)),
        ("chat_completions", Has(Chat)),
        ("put_feedback", Has(Chat)),
        ("start_run", Has(Run)),
        ("start_attempt", Has(Admin)),
        ("revert_change", Has(Admin)),
        ("hold", Has(Admin)),
        ("release", Has(Admin)),
        ("mcp", PerTool),
        ("call_tool", PerTool),
        ("me", Key),
        ("list_my_keys", Key),
        ("revoke_my_key", Key),
        ("put_person", Owner),
        ("list_people", Owner),
        ("make_key", Owner),
        ("list_keys", Owner),
        ("revoke_key", Owner),
    ]
};

/// The tools MCP lists, each with the scope a call to it needs, as
/// declared, for the document.
fn scoped_listing(mcp: &McpHandler) -> Value {
    let mut listing = mcp.listing();
    for tool in listing
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let name = tool.get("name").and_then(Value::as_str).map(str::to_owned);
        if let (Some(name), Some(object)) = (name, tool.as_object_mut())
            && let Some(effect) = mcp.declared_effect(&name)
        {
            object.insert(
                "x-scope".to_owned(),
                Value::from(scope_for_tool(&name, effect).name()),
            );
        }
    }
    listing
}

/// The `OpenAPI` document for the core's routes, with the tool route as one
/// generic path: what's checked into the repository.
///
/// # Errors
///
/// Why, if two different schemas would take the same name.
pub fn openapi_document() -> Result<Value, String> {
    spec::document(
        &all_routes(),
        None,
        spec::About {
            title: "HTTP API",
            version: "1",
            server: "http://127.0.0.1:8080",
            license: Some(env!("CARGO_PKG_LICENSE")),
            model: None,
        },
    )
}
