//! Every tool MCP offers, over REST too: `POST /api/v1/tools/{name}` makes
//! the same call `tools/call` makes, with the same checks, budget and
//! receipt. And the document that describes them all.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::header::HOST;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use plugin_api::{Called, DataClass, RunId, ToolContext};
use schema::bounded;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::spec::{Body, Route};
use super::{ApiError, App, detached, json_body};

/// The generic tool route's id. A served document lists each tool at a
/// path of its own instead.
pub(super) const CALL_TOOL: &str = "call_tool";

/// A tool call that ran and failed. It still ran, so it isn't an error
/// reply.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolFailureView {
    /// Always true: tells a failure from a tool's output.
    pub is_error: bool,
    /// Why it failed.
    #[schemars(length(max = 2048))]
    pub message: String,
}

async fn call_tool(
    State(app): State<App>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let arguments = json_body(&headers, &body)?;
    let permit = app.permit()?;
    let handler = Arc::clone(&app.mcp);
    // A caller from outside, as over MCP: private, and the call's own run.
    let context = ToolContext::new(RunId::generate(), DataClass::Private, 0);
    let called = detached(permit, async move {
        handler.call(&name, arguments, context).await
    })
    .await?;
    match called {
        Called::Ran(value) => Ok(Json(value).into_response()),
        Called::Failed(message) => Ok(Json(ToolFailureView {
            is_error: true,
            message: bounded(&message),
        })
        .into_response()),
        Called::Refused(refusal) => Err(ApiError::refused(&refusal)),
        Called::NoSuchTool => Err(ApiError::not_found(
            "no tool has that name; GET /api/v1/openapi.json lists them",
        )),
        _ => Err(ApiError::internal(&"a tool call came to something unknown")),
    }
}

/// The document, with its server set to where it was asked for.
async fn openapi(State(app): State<App>, headers: HeaderMap) -> Json<Value> {
    let mut document = (*app.openapi).clone();
    if let Some(host) = headers.get(HOST).and_then(|host| host.to_str().ok()) {
        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|scheme| scheme.to_str().ok())
            .filter(|scheme| matches!(*scheme, "http" | "https"))
            .unwrap_or("http");
        if let Some(object) = document.as_object_mut() {
            object.insert(
                "servers".to_owned(),
                serde_json::json!([{ "url": format!("{scheme}://{host}") }]),
            );
        }
    }
    Json(document)
}

pub(super) fn routes() -> Vec<Route> {
    vec![
        Route::post(
            CALL_TOOL,
            "/api/v1/tools/{name}",
            "Call a tool MCP offers, with its arguments as the body",
            post(call_tool),
        )
        .body(Body::Described(
            "The tool's arguments, meeting its input schema.",
        ))
        .reply(
            200,
            "What the tool returned, or a ToolFailureView if it ran and failed.",
            Body::Described("The tool's output."),
        ),
        Route::get(
            "openapi",
            "/api/v1/openapi.json",
            "This document, with every installed tool at a path of its own",
            get(openapi),
        )
        .reply(
            200,
            "The OpenAPI 3.1 document.",
            Body::Described("OpenAPI 3.1."),
        ),
    ]
}
