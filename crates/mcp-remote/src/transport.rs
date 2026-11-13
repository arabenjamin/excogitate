//! The client side of MCP's Streamable HTTP transport: each message is a
//! POST, answered with JSON or an event stream. A session id the server
//! hands out at `initialize` goes back on every later message; when the
//! server forgets it (a 404), a new session is started once. Only the
//! fields used are read; the rest of what a server sends is ignored.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;

/// The most of one answer read, in bytes.
pub const MAX_READ_BYTES: usize = 8 * 1024 * 1024;
/// The most pages of a tool listing read.
const MAX_LIST_PAGES: usize = 20;
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const REDACTED: &str = "[redacted]";

/// A call to a server failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    /// It couldn't be reached.
    #[error("couldn't reach it: {0}")]
    Unreachable(String),
    /// It took too long.
    #[error("it took longer than {0} seconds")]
    Timeout(u64),
    /// It answered with an HTTP error.
    #[error("it answered {status}: {body}")]
    Status {
        /// The status.
        status: u16,
        /// The start of what it said.
        body: String,
    },
    /// It answered, but not with a message that fits the protocol.
    #[error("its answer doesn't fit the protocol: {0}")]
    Protocol(String),
    /// It answered with a JSON-RPC error.
    #[error("it refused: {0}")]
    Remote(String),
}

/// The key, and where it goes.
#[derive(Clone)]
pub struct Key {
    name: HeaderName,
    value: HeaderValue,
    secret: String,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({}: {REDACTED})", self.name)
    }
}

impl Key {
    /// `prefix` and `secret` in the header `name`.
    ///
    /// # Errors
    ///
    /// When the header name or value isn't valid.
    pub fn new(name: &str, prefix: &str, secret: String) -> Result<Self, String> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("{name} isn't a header name"))?;
        let mut value = HeaderValue::from_str(&format!("{prefix}{secret}"))
            .map_err(|_| "the key can't go in a header".to_owned())?;
        value.set_sensitive(true);
        Ok(Self {
            name,
            value,
            secret,
        })
    }
}

/// What `initialize` agreed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    id: Option<String>,
    version: String,
}

/// One server's connection.
#[derive(Debug)]
pub struct Connection {
    client: reqwest::Client,
    url: Url,
    key: Option<Key>,
    timeout: Duration,
    session: Mutex<Option<Session>>,
    next_id: AtomicU64,
}

/// What one POST brought back.
struct Answer {
    status: u16,
    session: Option<String>,
    messages: Vec<Value>,
    body: String,
}

/// The messages in an event stream: each event's `data` lines, joined.
#[must_use]
pub fn events(stream: &str) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    let mut flush = |data: &mut Vec<&str>| {
        if !data.is_empty() {
            if let Ok(message) = serde_json::from_str::<Value>(&data.join("\n")) {
                messages.push(message);
            }
            data.clear();
        }
    };
    for line in stream.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            flush(&mut data);
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    flush(&mut data);
    messages
}

fn messages_of(content_type: &str, body: &str) -> Vec<Value> {
    if content_type.starts_with("text/event-stream") {
        return events(body);
    }
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Array(batch)) => batch,
        Ok(message) => vec![message],
        Err(_) => Vec::new(),
    }
}

impl Connection {
    /// A connection to `url` through `client`, with `key`, each call
    /// limited to `timeout`. Nothing is sent until the first call.
    #[must_use]
    pub fn new(client: reqwest::Client, url: Url, key: Option<Key>, timeout: Duration) -> Self {
        Self {
            client,
            url,
            key,
            timeout,
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// `text` with the key taken out, should a server echo it.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        match &self.key {
            Some(key) if !key.secret.is_empty() => text.replace(&key.secret, REDACTED),
            _ => text.to_owned(),
        }
    }

    async fn post(&self, message: &Value, session: Option<&Session>) -> Result<Answer, McpError> {
        let mut request = self
            .client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(message.to_string());
        if let Some(key) = &self.key {
            request = request.header(key.name.clone(), key.value.clone());
        }
        if let Some(session) = session {
            request = request.header(VERSION_HEADER, session.version.as_str());
            if let Some(id) = &session.id {
                request = request.header(SESSION_HEADER, id.as_str());
            }
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| McpError::Unreachable(self.scrub(&describe(&error))))?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let session = header(SESSION_HEADER);
        let content_type = header(CONTENT_TYPE.as_str()).unwrap_or_default();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| McpError::Unreachable(self.scrub(&describe(&error))))?
        {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_READ_BYTES {
                return Err(McpError::Protocol(format!(
                    "its answer is over {MAX_READ_BYTES} bytes"
                )));
            }
        }
        let body = String::from_utf8_lossy(&bytes).into_owned();
        Ok(Answer {
            status,
            session,
            messages: messages_of(&content_type, &body),
            body,
        })
    }

    fn status_error(&self, answer: &Answer) -> McpError {
        McpError::Status {
            status: answer.status,
            body: self.scrub(&answer.body.chars().take(300).collect::<String>()),
        }
    }

    /// Sends a request and takes the response with its id.
    async fn exchange(
        &self,
        method: &str,
        params: &Value,
        session: Option<&Session>,
    ) -> Result<(Answer, Value), McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let answer = self.post(&message, session).await?;
        if !(200..300).contains(&answer.status) {
            return Err(self.status_error(&answer));
        }
        let response = answer
            .messages
            .iter()
            .find(|message| message.get("id").and_then(Value::as_u64) == Some(id))
            .cloned()
            .ok_or_else(|| McpError::Protocol(format!("no response to its {method}")))?;
        if let Some(error) = response.get("error") {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(McpError::Remote(self.scrub(text)));
        }
        let result = response
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Protocol(format!("its {method} response has no result")))?;
        Ok((answer, result))
    }

    async fn initialize(&self) -> Result<Session, McpError> {
        let params = json!({
            "protocolVersion": plugin_api::MCP_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "mcp-remote", "version": env!("CARGO_PKG_VERSION")},
        });
        let (answer, result) = self.exchange("initialize", &params, None).await?;
        let session = Session {
            id: answer.session,
            version: result
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(plugin_api::MCP_VERSION)
                .chars()
                .take(32)
                .collect(),
        };
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let answer = self.post(&initialized, Some(&session)).await?;
        if !(200..300).contains(&answer.status) {
            return Err(self.status_error(&answer));
        }
        Ok(session)
    }

    async fn session(&self) -> Result<Session, McpError> {
        let mut held = self.session.lock().await;
        if let Some(session) = held.as_ref() {
            return Ok(session.clone());
        }
        let session = self.initialize().await?;
        *held = Some(session.clone());
        Ok(session)
    }

    async fn forget(&self, session: &Session) {
        let mut held = self.session.lock().await;
        if held.as_ref() == Some(session) {
            *held = None;
        }
    }

    async fn call_once(&self, method: &str, params: &Value) -> Result<Value, McpError> {
        let session = self.session().await?;
        match self.exchange(method, params, Some(&session)).await {
            // The server forgot the session: one more try, on a new one.
            Err(McpError::Status { status: 404, .. }) if session.id.is_some() => {
                self.forget(&session).await;
                let session = self.session().await?;
                self.exchange(method, params, Some(&session))
                    .await
                    .map(|(_, result)| result)
            }
            Err(error) => {
                if matches!(error, McpError::Unreachable(_)) {
                    self.forget(&session).await;
                }
                Err(error)
            }
            Ok((_, result)) => Ok(result),
        }
    }

    /// Calls `method` with `params` and returns its result, within the
    /// connection's timeout or `limit`, whichever is shorter.
    ///
    /// # Errors
    ///
    /// [`McpError`] if it fails, naming why.
    pub async fn call(
        &self,
        method: &str,
        params: &Value,
        limit: Option<Duration>,
    ) -> Result<Value, McpError> {
        let timeout = limit.map_or(self.timeout, |limit| limit.min(self.timeout));
        tokio::time::timeout(timeout, self.call_once(method, params))
            .await
            .map_err(|_| McpError::Timeout(timeout.as_secs()))?
    }

    /// Every tool the server lists.
    ///
    /// # Errors
    ///
    /// [`McpError`] if listing fails.
    pub async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let result = self.call("tools/list", &params, None).await?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol("its tools/list has no tools".to_owned()))?;
            tools.extend(page.iter().cloned());
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err(McpError::Protocol(format!(
            "its tool list runs past {MAX_LIST_PAGES} pages"
        )))
    }
}

fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_stream_yields_its_messages() {
        let stream = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\r\n\r\n\
                      : a comment\n\
                      data: {\"jsonrpc\":\"2.0\",\n\
                      data: \"id\":3,\"result\":{}}\n\n\
                      data: not json\n\n";
        let messages = events(stream);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["id"], 3);
    }

    #[test]
    fn json_and_batches_are_read() {
        assert_eq!(messages_of("application/json", r#"{"id":1}"#).len(), 1);
        assert_eq!(
            messages_of("application/json; charset=utf-8", r#"[{"id":1},{"id":2}]"#).len(),
            2
        );
        assert_eq!(
            messages_of("application/json", "<html>"),
            Vec::<Value>::new()
        );
    }

    #[test]
    fn a_key_is_never_printed() {
        let key =
            Key::new("x-api-key", "", "s3cret-value".to_owned()).unwrap_or_else(|e| panic!("{e}"));
        assert!(!format!("{key:?}").contains("s3cret"));
    }
}
