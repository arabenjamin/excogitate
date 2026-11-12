//! MCP over HTTP: one JSON-RPC message per POST, answered with JSON. It keeps
//! no session, so there's nothing to resume and no event stream to open.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use plugin_api::MCP_VERSION;
use plugin_api::rpc::{INVALID_REQUEST, PARSE_ERROR, failure};
use serde_json::Value;

use super::{ApiError, App, detached, json_body};

fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    (status, axum::Json(failure(&Value::Null, code, message))).into_response()
}

fn well_formed(message: &Value) -> bool {
    message.is_object() && message.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
}

async fn post_message(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(version) = headers.get("mcp-protocol-version")
        && version.to_str().ok() != Some(MCP_VERSION)
    {
        let detail = format!("this server speaks MCP {MCP_VERSION}");
        return rpc_error(StatusCode::BAD_REQUEST, INVALID_REQUEST, &detail);
    }
    let message = match json_body(&headers, &body) {
        Ok(message) => message,
        Err(error) if error.status() == StatusCode::BAD_REQUEST => {
            return rpc_error(StatusCode::BAD_REQUEST, PARSE_ERROR, error.message());
        }
        Err(error) => return error.into_response(),
    };
    if !well_formed(&message) {
        let detail = "a request is one JSON-RPC 2.0 object";
        return rpc_error(StatusCode::BAD_REQUEST, INVALID_REQUEST, detail);
    }
    // A notification, or the client's reply to something: nothing to answer.
    if message.get("id").is_none() || message.get("method").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let reply = if message.get("method").and_then(Value::as_str) == Some("tools/call") {
        let permit = match app.permit() {
            Ok(permit) => permit,
            Err(error) => return error.into_response(),
        };
        let handler = Arc::clone(&app.mcp);
        match detached(permit, async move { handler.handle(&message).await }).await {
            Ok(reply) => reply,
            Err(error) => return error.into_response(),
        }
    } else {
        app.mcp.handle(&message).await
    };
    match reply {
        Some(reply) => axum::Json(reply).into_response(),
        None => ApiError::internal(&"a request got no reply").into_response(),
    }
}

pub(super) fn routes() -> Router<App> {
    Router::new().route("/mcp", post(post_message))
}
