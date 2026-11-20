//! MCP over HTTP: one JSON-RPC message per POST, answered with JSON. It keeps
//! no session, so there's nothing to resume and no event stream to open.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use plugin_api::MCP_VERSION;
use plugin_api::rpc::{INVALID_REQUEST, PARSE_ERROR, failure};
use serde_json::{Value, json};

use super::spec::{Body, Errors, Kind, Param, Route};
use super::{ApiError, App, detached, json_body};

fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    (status, axum::Json(failure(&Value::Null, code, message))).into_response()
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
    // The schema the document gives is the one a message is held to.
    if app.mcp_message.check(&message).is_err() {
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

/// The JSON-RPC error object.
fn error_object() -> Value {
    json!({
        "type": "object",
        "required": ["code", "message"],
        "properties": {
            "code": { "type": "integer" },
            "message": { "type": "string" },
            "data": {}
        }
    })
}

/// What a client sends: one JSON-RPC 2.0 request, notification or reply.
pub(super) fn message_schema() -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc"],
        "properties": {
            "jsonrpc": { "const": "2.0" },
            "id": { "type": ["string", "number", "null"] },
            "method": { "type": "string",
                        "description": "initialize, ping, tools/list or tools/call; \
                                        anything else is method-not-found." },
            "params": { "type": ["object", "array"] },
            "result": {},
            "error": error_object()
        }
    })
}

/// The reply to a request: a result, or an error the call came to.
fn reply_schema() -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc", "id"],
        "properties": {
            "jsonrpc": { "const": "2.0" },
            "id": { "type": ["string", "number", "null"] },
            "result": { "type": "object" },
            "error": error_object()
        }
    })
}

/// The JSON-RPC error a malformed message gets.
pub(super) fn error_schema() -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "error"],
        "properties": {
            "jsonrpc": { "const": "2.0" },
            "id": { "type": "null" },
            "error": error_object()
        }
    })
}

const PROTOCOL: Param = Param {
    name: "mcp-protocol-version",
    about: "The protocol version; any other is refused.",
    kind: Kind::OneOf(&[MCP_VERSION]),
};

pub(super) fn routes() -> Vec<Route> {
    vec![
        Route::post(
            "mcp",
            "/mcp",
            "MCP over HTTP: one JSON-RPC 2.0 message per request, protocol 2025-06-18",
            post(post_message),
        )
        .headers(&[PROTOCOL])
        .body(Body::Inline(message_schema))
        .reply(200, "The JSON-RPC reply.", Body::Inline(reply_schema))
        .reply(
            202,
            "A notification or a client's reply: nothing to answer.",
            Body::Empty,
        )
        .errors(Errors::JsonRpc),
    ]
}
