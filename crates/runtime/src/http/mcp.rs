//! MCP over HTTP: one JSON-RPC message per POST, answered with JSON. It keeps
//! no session, so there's nothing to resume and no event stream to open.
//! Both eras are served: a handshake's (2025-11-25, 2025-06-18) and the
//! stateless one (2026-07-28), whose requests also say in headers what
//! their body says.

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use plugin_api::rpc::{
    HEADER_MISMATCH, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
    UNSUPPORTED_VERSION, failure,
};
use plugin_api::{STATELESS_VERSION, SUPPORTED_VERSIONS, VERSION_META};
use serde_json::{Value, json};

use super::spec::{Body, Errors, Kind, Param, Route};
use super::{ApiError, App, detached, json_body, scope_for_tool};
use crate::auth::{Caller, Scope};
use plugin_api::Effect;

/// The JSON-RPC error a call gets when its key may not make it: a code in
/// the range JSON-RPC leaves to servers.
const FORBIDDEN: i64 = -32001;

fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    (status, axum::Json(failure(&Value::Null, code, message))).into_response()
}

/// A header's value, if it's there and readable.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The revision a stateless request names in its body, if it is one. An
/// `initialize` is a handshake's, whatever else it carries.
fn stateless_version(message: &Value) -> Option<&str> {
    if message.get("method").and_then(Value::as_str) == Some("initialize") {
        return None;
    }
    message
        .pointer("/params/_meta")?
        .get(VERSION_META)?
        .as_str()
}

/// Why a stateless request's headers don't fit its body, if they don't:
/// the revision, the method, and on a call the tool must all match.
fn header_mismatch(headers: &HeaderMap, message: &Value, version: &str) -> Option<String> {
    if header(headers, "mcp-protocol-version") != Some(version) {
        return Some(format!(
            "MCP-Protocol-Version must be {version}, as the body's `_meta` says"
        ));
    }
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    if message.get("id").is_some() && header(headers, "mcp-method") != Some(method) {
        return Some(format!("Mcp-Method must be {method}, as the body says"));
    }
    if method == "tools/call" {
        let name = message
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        if header(headers, "mcp-name") != Some(name) {
            return Some(format!("Mcp-Name must be {name}, the tool the body calls"));
        }
    }
    None
}

/// The status a stateless reply goes with: a request the server can't take
/// is a 400, an unknown method a 404. A handshake's errors are all 200, as
/// before.
fn stateless_status(reply: &Value) -> StatusCode {
    match reply.pointer("/error/code").and_then(Value::as_i64) {
        Some(METHOD_NOT_FOUND) => StatusCode::NOT_FOUND,
        Some(INVALID_PARAMS | UNSUPPORTED_VERSION | HEADER_MISMATCH) => StatusCode::BAD_REQUEST,
        _ => StatusCode::OK,
    }
}

/// Whether `caller` may make this `tools/call`, and if not, why. A tool
/// that doesn't exist is left for the handler to say so.
fn may_call(app: &App, caller: &Caller, params: Option<&Value>) -> Result<(), String> {
    let params = params.cloned().unwrap_or_else(|| json!({}));
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Ok(());
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match app.mcp.effect_of(name, &arguments) {
        Some(effect) => caller.may(scope_for_tool(name, effect)),
        None => Ok(()),
    }
}

/// Whether `caller` sees tool `name` listed: when it may make the call the
/// tool declares, or, for a tool declared as writing, when it may read,
/// since such a tool often reads too (memory searches, `http_request`
/// GETs). A call that would write is still refused, with the scope it needs.
fn listed_for(caller: &Caller, name: &str, declared: Effect) -> bool {
    caller.may(scope_for_tool(name, declared)).is_ok()
        || (declared == Effect::Write && name != "chat" && caller.may(Scope::Read).is_ok())
}

/// Leaves out of a `tools/list` reply each tool the caller can't call at
/// all, so a client doesn't offer one it can't use.
fn only_callable(app: &App, caller: &Caller, mut reply: Value) -> Value {
    if let Some(tools) = reply
        .pointer_mut("/result/tools")
        .and_then(Value::as_array_mut)
    {
        tools.retain(|tool| {
            let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
            app.mcp
                .declared_effect(name)
                .is_some_and(|effect| listed_for(caller, name, effect))
        });
    }
    reply
}

/// The request's headers checked against the document and its body: the
/// revision it names if it's stateless, or the JSON-RPC error to refuse it
/// with, as a 400.
fn checked_headers(
    headers: &HeaderMap,
    message: &Value,
    id: &Value,
) -> Result<Option<String>, Value> {
    // The headers that repeat a body hold to what the document says of
    // them, whichever revision the request is.
    for (name, longest) in [("mcp-method", METHOD_CHARS), ("mcp-name", TOOL_CHARS)] {
        if let Some(value) = headers.get(name) {
            let fits = value
                .to_str()
                .is_ok_and(|text| !text.is_empty() && text.chars().count() <= longest);
            if !fits {
                let detail = format!("{name} must be 1 to {longest} characters");
                return Err(failure(id, INVALID_REQUEST, &detail));
            }
        }
    }
    if let Some(version) = headers.get("mcp-protocol-version") {
        let version = version.to_str().unwrap_or("");
        if !SUPPORTED_VERSIONS.contains(&version) {
            let refused = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": UNSUPPORTED_VERSION,
                    "message": "Unsupported protocol version",
                    "data": {
                        "supported": SUPPORTED_VERSIONS,
                        "requested": version.chars().take(64).collect::<String>(),
                    },
                },
            });
            return Err(refused);
        }
    }
    let stateless = stateless_version(message).map(str::to_owned);
    if let Some(version) = &stateless {
        if let Some(detail) = header_mismatch(headers, message, version) {
            let refused = failure(id, HEADER_MISMATCH, &detail);
            return Err(refused);
        }
    } else if header(headers, "mcp-protocol-version") == Some(STATELESS_VERSION)
        && message.get("id").is_some()
    {
        let detail = format!(
            "a {STATELESS_VERSION} request names its revision and the client's capabilities in `params._meta`"
        );
        return Err(failure(id, INVALID_PARAMS, &detail));
    }
    Ok(stateless)
}

async fn post_message(
    State(app): State<App>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
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
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let stateless = match checked_headers(&headers, &message, &id) {
        Ok(stateless) => stateless,
        Err(refused) => return (StatusCode::BAD_REQUEST, axum::Json(refused)).into_response(),
    };
    // A notification, or the client's reply to something: nothing to answer.
    if message.get("id").is_none() || message.get("method").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let method = message.get("method").and_then(Value::as_str);
    if method == Some("tools/call")
        && let Err(detail) = may_call(&app, &caller, message.get("params"))
    {
        return axum::Json(failure(&id, FORBIDDEN, &detail)).into_response();
    }
    let listing = method == Some("tools/list");
    let reply = if method == Some("tools/call") {
        let permit = match app.permit() {
            Ok(permit) => permit,
            Err(error) => return error.into_response(),
        };
        let handler = Arc::clone(&app.mcp);
        let asking = crate::mcp::Asking {
            actor: caller.actor.to_string(),
            private_by_default: caller.private_by_default,
        };
        let call = crate::mcp::CALLER.scope(asking, async move { handler.handle(&message).await });
        match detached(permit, call).await {
            Ok(reply) => reply,
            Err(error) => return error.into_response(),
        }
    } else {
        app.mcp.handle(&message).await
    };
    let status = |reply: &Value| {
        if stateless.is_some() {
            stateless_status(reply)
        } else {
            StatusCode::OK
        }
    };
    match reply {
        Some(reply) if listing => {
            let reply = only_callable(&app, &caller, reply);
            (status(&reply), axum::Json(reply)).into_response()
        }
        Some(reply) => (status(&reply), axum::Json(reply)).into_response(),
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
                        "description": "initialize, ping, server/discover, tools/list or \
                                        tools/call; anything else is method-not-found." },
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
            "id": { "type": ["string", "number", "null"] },
            "error": error_object()
        }
    })
}

const PROTOCOL: Param = Param {
    name: "mcp-protocol-version",
    about: "The MCP revision. Required on a 2026-07-28 request, and then the same as \
            its `_meta` says; any revision not served is refused.",
    kind: Kind::OneOf(&SUPPORTED_VERSIONS),
};

/// The longest `mcp-method` header taken.
const METHOD_CHARS: usize = 64;
/// The longest `mcp-name` header taken.
const TOOL_CHARS: usize = 128;

const METHOD: Param = Param {
    name: "mcp-method",
    about: "The request's method. Required on a 2026-07-28 request.",
    kind: Kind::Text(64),
};

const TOOL: Param = Param {
    name: "mcp-name",
    about: "The tool a 2026-07-28 `tools/call` calls. Required on one.",
    kind: Kind::Text(128),
};

pub(super) fn routes() -> Vec<Route> {
    vec![
        Route::post(
            "mcp",
            "/mcp",
            "MCP over HTTP: one JSON-RPC 2.0 message per request, in revision 2026-07-28, \
             2025-11-25 or 2025-06-18",
            post(post_message),
        )
        .headers(&[PROTOCOL, METHOD, TOOL])
        .body(Body::Inline(message_schema))
        .reply(200, "The JSON-RPC reply.", Body::Inline(reply_schema))
        .reply(
            404,
            "A 2026-07-28 request for a method there's no such of.",
            Body::Inline(error_schema),
        )
        .reply(
            202,
            "A notification or a client's reply: nothing to answer.",
            Body::Empty,
        )
        .errors(Errors::JsonRpc),
    ]
}
