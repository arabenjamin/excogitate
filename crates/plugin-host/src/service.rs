//! A plugin in its own process: the client side of MCP over stdio.
//!
//! One task owns the child's stdout and routes replies by id, and another
//! owns its stdin. A call that's abandoned, such as by a run's time limit,
//! leaves neither stream mid-message.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use dispatch::{Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use plugin_api::{MCP_VERSION, PluginManifest, read_line};
use protocol::Trust;
use schema::bounded;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// How many messages can wait to be written.
const OUTGOING_QUEUE: usize = 64;

/// How to start a plugin's process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// The program.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The environment variables it may see. Its environment is otherwise
    /// empty, so the brain's secrets don't reach it.
    pub env_allow: Vec<String>,
    /// How long one call may take.
    pub timeout_secs: u32,
}

/// A service failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ServiceError {
    /// The process couldn't be started.
    #[error("couldn't start: {0}")]
    Spawn(String),
    /// It stopped, or its connection ended.
    #[error("the service stopped")]
    Stopped,
    /// It took too long.
    #[error("the service timed out")]
    Timeout,
    /// It answered with a protocol error.
    #[error("the service refused: {0}")]
    Remote(String),
    /// What it said doesn't fit the protocol or its manifest.
    #[error("the service misbehaved: {0}")]
    Protocol(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

fn pending_lock(
    pending: &Pending,
) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Value>>> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stops the tasks and, through `kill_on_drop`, the process.
#[derive(Debug)]
struct Connection {
    outgoing: mpsc::Sender<String>,
    pending: Pending,
    next_id: AtomicU64,
    limit: Duration,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    _child: Child,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

async fn read_replies(stdout: ChildStdout, pending: Pending) {
    let mut reader = BufReader::new(stdout);
    // Ends at the end of the stream, a read error, or an oversized line.
    while let Ok(Some(line)) = read_line(&mut reader).await {
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let waiter = message
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| pending_lock(&pending).remove(&id));
        if let Some(waiter) = waiter {
            // The caller may have given up; that's fine.
            let _ = waiter.send(message);
        }
    }
    // Dropping the senders tells every waiting caller the service stopped.
    pending_lock(&pending).clear();
}

async fn write_messages(mut stdin: ChildStdin, mut outgoing: mpsc::Receiver<String>) {
    while let Some(mut message) = outgoing.recv().await {
        message.push('\n');
        if stdin.write_all(message.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

impl Connection {
    fn open(spec: &ServiceSpec) -> Result<Self, ServiceError> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for name in &spec.env_allow {
            if let Ok(value) = std::env::var(name) {
                command.env(name, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| ServiceError::Spawn(bounded(&error.to_string())))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ServiceError::Spawn("no pipes to the process".to_owned()));
        };
        let pending = Pending::default();
        let (sender, receiver) = mpsc::channel(OUTGOING_QUEUE);
        Ok(Self {
            outgoing: sender,
            reader: tokio::spawn(read_replies(stdout, Arc::clone(&pending))),
            writer: tokio::spawn(write_messages(stdin, receiver)),
            pending,
            next_id: AtomicU64::new(1),
            limit: Duration::from_secs(u64::from(spec.timeout_secs)),
            _child: child,
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ServiceError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        pending_lock(&self.pending).insert(id, sender);
        let forget = Forget {
            pending: &self.pending,
            id,
        };
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.outgoing
            .send(message.to_string())
            .await
            .map_err(|_| ServiceError::Stopped)?;
        let reply = timeout(self.limit, receiver)
            .await
            .map_err(|_| ServiceError::Timeout)?
            .map_err(|_| ServiceError::Stopped)?;
        drop(forget);
        if let Some(error) = reply.get("error") {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(ServiceError::Remote(bounded(text)));
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| ServiceError::Protocol("a reply with no result".to_owned()))
    }

    async fn notify(&self, method: &str) -> Result<(), ServiceError> {
        let message = json!({ "jsonrpc": "2.0", "method": method });
        self.outgoing
            .send(message.to_string())
            .await
            .map_err(|_| ServiceError::Stopped)
    }
}

/// Removes a waiter when its call is abandoned, so they can't pile up.
struct Forget<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        pending_lock(self.pending).remove(&self.id);
    }
}

/// Starts the process and checks it against the manifest. Returns one tool
/// per declared tool. The manifest is the authority: a server can't offer
/// more than it declares, or loosen a declared schema.
pub(crate) async fn start(
    manifest: &PluginManifest,
    spec: &ServiceSpec,
) -> Result<Vec<Arc<dyn Tool>>, ServiceError> {
    let connection = Arc::new(Connection::open(spec)?);
    let params = json!({
        "protocolVersion": MCP_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "host", "version": env!("CARGO_PKG_VERSION") },
    });
    connection.request("initialize", params).await?;
    connection.notify("notifications/initialized").await?;
    let listing = connection.request("tools/list", json!({})).await?;
    let listed = listing
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| ServiceError::Protocol("tools/list has no tools".to_owned()))?;
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for definition in manifest.definitions() {
        let found = listed
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(&definition.name))
            .ok_or_else(|| ServiceError::Protocol(format!("{} isn't served", definition.name)))?;
        if found.get("inputSchema") != Some(&definition.input_schema) {
            return Err(ServiceError::Protocol(format!(
                "{}'s input schema differs from its manifest",
                definition.name
            )));
        }
        tools.push(Arc::new(ServiceTool {
            definition,
            connection: Arc::clone(&connection),
        }));
    }
    if listed.len() != tools.len() {
        return Err(ServiceError::Protocol(
            "it serves tools its manifest doesn't declare".to_owned(),
        ));
    }
    Ok(tools)
}

#[derive(Debug)]
struct ServiceTool {
    definition: ToolDefinition,
    connection: Arc<Connection>,
}

fn result_value(result: &Value) -> Result<Value, ToolFailure> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(ToolFailure(bounded(text.unwrap_or("the tool failed"))));
    }
    if let Some(value) = result.get("structuredContent") {
        return Ok(value.clone());
    }
    text.and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| ToolFailure("the service returned no JSON result".to_owned()))
}

#[async_trait]
impl Tool for ServiceTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let params = json!({ "name": self.definition.name, "arguments": arguments });
        let result = self
            .connection
            .request("tools/call", params)
            .await
            .map_err(|error| ToolFailure(error.to_string()))?;
        Ok(ToolOutput {
            value: result_value(&result)?,
            // Another process's output is never trusted as instructions.
            trust: Trust::Untrusted,
            cost_usd_micros: 0,
        })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn shell(script: &str, env_allow: &[&str], timeout_secs: u32) -> ServiceSpec {
        ServiceSpec {
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            env_allow: env_allow.iter().map(|s| (*s).to_owned()).collect(),
            timeout_secs,
        }
    }

    /// Answers one request with what it can see of cargo's environment.
    const SHOW_ENV: &str =
        r#"read line; printf '{"id":1,"result":{"seen":"%s"}}\n' "$CARGO_MANIFEST_DIR""#;

    #[tokio::test]
    async fn the_brains_environment_does_not_reach_a_service() {
        let connection = Connection::open(&shell(SHOW_ENV, &[], 10)).unwrap();
        let result = connection.request("x", json!({})).await.unwrap();
        assert_eq!(result["seen"], "");
    }

    #[tokio::test]
    async fn a_named_variable_does_reach_a_service() {
        let connection = Connection::open(&shell(SHOW_ENV, &["CARGO_MANIFEST_DIR"], 10)).unwrap();
        let result = connection.request("x", json!({})).await.unwrap();
        assert_ne!(result["seen"], "");
    }

    #[tokio::test]
    async fn replies_written_together_all_arrive() {
        let script = r#"read a; read b; printf '{"id":1,"result":{"n":1}}\n{"id":2,"result":{"n":2}}\n'; sleep 5"#;
        let connection = Connection::open(&shell(script, &[], 10)).unwrap();
        let (one, two) = tokio::join!(
            connection.request("x", json!({})),
            connection.request("y", json!({}))
        );
        assert_eq!(one.unwrap()["n"], 1);
        assert_eq!(two.unwrap()["n"], 2);
    }

    #[tokio::test]
    async fn a_silent_service_times_out_and_forgets_the_call() {
        let connection = Connection::open(&shell("sleep 30", &[], 1)).unwrap();
        let error = connection.request("x", json!({})).await.unwrap_err();
        assert_eq!(error, ServiceError::Timeout);
        assert!(pending_lock(&connection.pending).is_empty());
    }

    #[tokio::test]
    async fn a_service_dying_mid_call_is_reported() {
        let connection = Connection::open(&shell("read line; exit 3", &[], 10)).unwrap();
        let error = connection.request("x", json!({})).await.unwrap_err();
        assert_eq!(error, ServiceError::Stopped);
    }

    #[tokio::test]
    async fn a_protocol_error_is_passed_on() {
        let script = r#"read a; printf '{"id":1,"error":{"message":"no"}}\n'; sleep 5"#;
        let connection = Connection::open(&shell(script, &[], 10)).unwrap();
        let error = connection.request("x", json!({})).await.unwrap_err();
        assert_eq!(error, ServiceError::Remote("no".to_owned()));
    }

    #[test]
    fn structured_content_is_preferred() {
        let result =
            json!({ "content": [{ "text": "{\"a\":1}" }], "structuredContent": { "b": 2 } });
        assert_eq!(result_value(&result).unwrap(), json!({ "b": 2 }));
    }

    #[test]
    fn text_is_parsed_when_it_is_all_there_is() {
        let result = json!({ "content": [{ "text": "{\"a\":1}" }] });
        assert_eq!(result_value(&result).unwrap(), json!({ "a": 1 }));
    }

    #[test]
    fn an_error_result_is_a_failure() {
        let result = json!({ "content": [{ "text": "broke" }], "isError": true });
        assert_eq!(result_value(&result), Err(ToolFailure("broke".to_owned())));
    }

    #[test]
    fn a_result_with_nothing_usable_is_a_failure() {
        assert!(result_value(&json!({ "content": [{ "text": "not json" }] })).is_err());
        assert!(result_value(&json!({})).is_err());
    }
}
