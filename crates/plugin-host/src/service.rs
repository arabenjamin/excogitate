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
use plugin_api::wire::{
    CAPABILITY_METHOD, CONTEXT_META, CapabilityCall, WireContext, WireHttpResponse, WireModelReply,
    error_code, page_of, query_of, wire_page, wire_record,
};
use plugin_api::{
    Capabilities, CapabilityError, HttpRequest, MCP_VERSION, PluginManifest, read_line,
};
use protocol::{DataClass, RunId, Trust};
use schema::bounded;
use serde_json::{Value, json};
use store::RecordId;
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
    /// empty, so the runtime's secrets don't reach it.
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

/// What a process may ask of the host: the capabilities it was granted,
/// for the tool calls the host is serving on its connection right now.
#[derive(Debug, Default)]
struct HostCalls {
    capabilities: Capabilities,
    /// Each run with a call in flight, with how many.
    serving: Mutex<HashMap<RunId, (ToolContext, usize)>>,
}

impl HostCalls {
    fn serving(&self) -> std::sync::MutexGuard<'_, HashMap<RunId, (ToolContext, usize)>> {
        self.serving.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin(&self, context: ToolContext) {
        self.serving().entry(context.run).or_insert((context, 0)).1 += 1;
    }

    fn end(&self, run: RunId) {
        let mut serving = self.serving();
        if let Some((_, count)) = serving.get_mut(&run) {
            *count -= 1;
            if *count == 0 {
                serving.remove(&run);
            }
        }
    }

    /// The host's own record of the run, if it's serving a call for it.
    fn context(&self, run: RunId) -> Option<ToolContext> {
        self.serving().get(&run).map(|(context, _)| *context)
    }

    async fn answer(
        &self,
        call: CapabilityCall,
        context: ToolContext,
    ) -> Result<Value, CapabilityError> {
        let json = |value: Result<Value, serde_json::Error>| {
            value.map_err(|error| CapabilityError::Failed(error.to_string()))
        };
        match call {
            CapabilityCall::RecordsPut {
                record_type,
                id,
                data,
            } => {
                let records = self.capabilities.records()?;
                let id = records
                    .put(&record_type, id.map(RecordId::from_uuid), data, context)
                    .await?;
                Ok(Value::String(id.to_string()))
            }
            CapabilityCall::RecordsGet { record_type, id } => {
                let found = self
                    .capabilities
                    .records()?
                    .get(&record_type, RecordId::from_uuid(id))
                    .await?;
                json(serde_json::to_value(found.map(wire_record)))
            }
            CapabilityCall::RecordsQuery {
                record_type,
                filters,
                limit,
                offset,
            } => {
                let query = query_of(record_type, filters, limit, offset)?;
                let page = self.capabilities.records()?.query(query).await?;
                json(serde_json::to_value(wire_page(page)))
            }
            CapabilityCall::RecordsDelete { record_type, id } => {
                self.capabilities
                    .records()?
                    .delete(&record_type, RecordId::from_uuid(id))
                    .await?;
                Ok(Value::Null)
            }
            CapabilityCall::RecordsLink { relation, from, to } => {
                self.capabilities
                    .records()?
                    .link(
                        &relation,
                        RecordId::from_uuid(from),
                        RecordId::from_uuid(to),
                    )
                    .await?;
                Ok(Value::Null)
            }
            CapabilityCall::RecordsNeighbors {
                relation,
                id,
                direction,
                limit,
                offset,
            } => {
                let page = self
                    .capabilities
                    .records()?
                    .neighbors(
                        &relation,
                        RecordId::from_uuid(id),
                        direction.into(),
                        page_of(limit, offset)?,
                    )
                    .await?;
                json(serde_json::to_value(wire_page(page)))
            }
            CapabilityCall::Http {
                method,
                url,
                headers,
                body,
            } => {
                let request = HttpRequest {
                    method,
                    url,
                    headers,
                    body,
                };
                let response = self.capabilities.http()?.request(request, context).await?;
                json(serde_json::to_value(WireHttpResponse::from(response)))
            }
            CapabilityCall::Model { prompt } => {
                let reply = self
                    .capabilities
                    .model()?
                    .complete(&prompt, context)
                    .await?;
                json(serde_json::to_value(WireModelReply::from(reply)))
            }
        }
    }

    /// The reply to a request the process sent.
    async fn reply_to(&self, message: &Value) -> Value {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let refuse = |code: i64, text: &str| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": bounded(text) } });
        if message.get("method").and_then(Value::as_str) != Some(CAPABILITY_METHOD) {
            return refuse(-32601, "the host answers only capabilities/call");
        }
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let run = params
            .get("run")
            .and_then(Value::as_str)
            .and_then(|run| run.parse::<RunId>().ok());
        let call = params
            .get("call")
            .cloned()
            .and_then(|call| serde_json::from_value::<CapabilityCall>(call).ok());
        let (Some(run), Some(call)) = (run, call) else {
            return refuse(
                -32602,
                "a capability call needs a run and a call it understands",
            );
        };
        // The host's record of the run, never the process's: it can't claim
        // a bigger budget, act outside a call, or reach another run.
        let Some(context) = self.context(run) else {
            return refuse(
                plugin_api::wire::REFUSED_CODE,
                "no call of this plugin is running for that run",
            );
        };
        match self.answer(call, context).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => refuse(error_code(&error), &error.to_string()),
        }
    }
}

fn pending_lock(
    pending: &Pending,
) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Value>>> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stops the tasks and, through `kill_on_drop`, the process.
#[derive(Debug)]
struct Connection {
    host: Arc<HostCalls>,
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

async fn read_replies(
    stdout: ChildStdout,
    pending: Pending,
    host: Arc<HostCalls>,
    outgoing: mpsc::Sender<String>,
) {
    let mut reader = BufReader::new(stdout);
    // Ends at the end of the stream, a read error, or an oversized line.
    while let Ok(Some(line)) = read_line(&mut reader).await {
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        // A request from the process: a capability call, answered on its
        // own task so a slow one doesn't hold up the replies behind it.
        if message.get("method").is_some() {
            if message.get("id").is_some() {
                let (host, outgoing) = (Arc::clone(&host), outgoing.clone());
                tokio::spawn(async move {
                    let reply = host.reply_to(&message).await;
                    let _ = outgoing.send(reply.to_string()).await;
                });
            }
            continue;
        }
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
    fn open(spec: &ServiceSpec, capabilities: Capabilities) -> Result<Self, ServiceError> {
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
        let host = Arc::new(HostCalls {
            capabilities,
            serving: Mutex::default(),
        });
        Ok(Self {
            reader: tokio::spawn(read_replies(
                stdout,
                Arc::clone(&pending),
                Arc::clone(&host),
                sender.clone(),
            )),
            host,
            outgoing: sender,
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

/// Ends a call's window for capability requests, even if it's abandoned.
struct EndCall<'a> {
    host: &'a HostCalls,
    run: RunId,
}

impl Drop for EndCall<'_> {
    fn drop(&mut self) {
        self.host.end(self.run);
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
    capabilities: Capabilities,
) -> Result<Vec<Arc<dyn Tool>>, ServiceError> {
    let connection = Arc::new(Connection::open(spec, capabilities)?);
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

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let sent = WireContext {
            run: context.run.as_uuid(),
            public: context.data_class == DataClass::Public,
            budget_usd_micros: context.budget_usd_micros,
        };
        let params = json!({
            "name": self.definition.name,
            "arguments": arguments,
            "_meta": { CONTEXT_META: sent },
        });
        // While the call is out, the process may ask for capabilities on its
        // behalf, and only then.
        self.connection.host.begin(context);
        let serving = EndCall {
            host: &self.connection.host,
            run: context.run,
        };
        let result = self.connection.request("tools/call", params).await;
        drop(serving);
        let result = result.map_err(|error| ToolFailure(error.to_string()))?;
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
    async fn the_runtimes_environment_does_not_reach_a_service() {
        let connection =
            Connection::open(&shell(SHOW_ENV, &[], 10), Capabilities::default()).unwrap();
        let result = connection.request("x", json!({})).await.unwrap();
        assert_eq!(result["seen"], "");
    }

    #[tokio::test]
    async fn a_named_variable_does_reach_a_service() {
        let connection = Connection::open(
            &shell(SHOW_ENV, &["CARGO_MANIFEST_DIR"], 10),
            Capabilities::default(),
        )
        .unwrap();
        let result = connection.request("x", json!({})).await.unwrap();
        assert_ne!(result["seen"], "");
    }

    #[tokio::test]
    async fn replies_written_together_all_arrive() {
        let script = r#"read a; read b; printf '{"id":1,"result":{"n":1}}\n{"id":2,"result":{"n":2}}\n'; sleep 5"#;
        let connection =
            Connection::open(&shell(script, &[], 10), Capabilities::default()).unwrap();
        let (one, two) = tokio::join!(
            connection.request("x", json!({})),
            connection.request("y", json!({}))
        );
        assert_eq!(one.unwrap()["n"], 1);
        assert_eq!(two.unwrap()["n"], 2);
    }

    #[tokio::test]
    async fn a_silent_service_times_out_and_forgets_the_call() {
        let connection =
            Connection::open(&shell("sleep 30", &[], 1), Capabilities::default()).unwrap();
        let error = connection.request("x", json!({})).await.unwrap_err();
        assert_eq!(error, ServiceError::Timeout);
        assert!(pending_lock(&connection.pending).is_empty());
    }

    #[tokio::test]
    async fn a_service_dying_mid_call_is_reported() {
        let connection = Connection::open(
            &shell("read line; exit 3", &[], 10),
            Capabilities::default(),
        )
        .unwrap();
        let error = connection.request("x", json!({})).await.unwrap_err();
        assert_eq!(error, ServiceError::Stopped);
    }

    #[tokio::test]
    async fn a_protocol_error_is_passed_on() {
        let script = r#"read a; printf '{"id":1,"error":{"message":"no"}}\n'; sleep 5"#;
        let connection =
            Connection::open(&shell(script, &[], 10), Capabilities::default()).unwrap();
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

    #[derive(Debug, Default)]
    struct SeenModel(Mutex<Vec<ToolContext>>);

    #[async_trait]
    impl plugin_api::ModelCapability for SeenModel {
        async fn complete(
            &self,
            _prompt: &str,
            context: ToolContext,
        ) -> Result<plugin_api::ModelReply, CapabilityError> {
            self.0.lock().unwrap().push(context);
            Ok(plugin_api::ModelReply {
                text: "ok".to_owned(),
                model: "m".to_owned(),
                cost_usd_micros: 5,
            })
        }
    }

    fn host_with_model() -> (HostCalls, Arc<SeenModel>) {
        let seen = Arc::new(SeenModel::default());
        let capabilities = Capabilities {
            model: Some(Arc::clone(&seen) as Arc<dyn plugin_api::ModelCapability>),
            ..Capabilities::default()
        };
        let host = HostCalls {
            capabilities,
            serving: Mutex::default(),
        };
        (host, seen)
    }

    fn capability_request(run: RunId, call: &Value) -> Value {
        json!({
            "jsonrpc": "2.0", "id": 9, "method": CAPABILITY_METHOD,
            "params": { "run": run.to_string(), "call": call }
        })
    }

    fn context(run: RunId) -> ToolContext {
        ToolContext {
            run,
            data_class: DataClass::Public,
            budget_usd_micros: 77,
        }
    }

    #[tokio::test]
    async fn a_capability_call_for_a_run_not_being_served_is_refused() {
        let (host, seen) = host_with_model();
        let reply = host
            .reply_to(&capability_request(
                RunId::generate(),
                &json!({ "op": "model", "prompt": "hi" }),
            ))
            .await;
        assert_eq!(
            reply["error"]["code"],
            plugin_api::wire::REFUSED_CODE,
            "{reply}"
        );
        assert!(seen.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_host_uses_its_own_record_of_the_run_and_only_while_it_runs() {
        let (host, seen) = host_with_model();
        let run = RunId::generate();
        host.begin(context(run));
        let reply = host
            .reply_to(&capability_request(
                run,
                &json!({ "op": "model", "prompt": "hi" }),
            ))
            .await;
        assert_eq!(reply["result"]["text"], "ok", "{reply}");
        assert_eq!(seen.0.lock().unwrap().as_slice(), [context(run)]);
        host.end(run);
        let after = host
            .reply_to(&capability_request(
                run,
                &json!({ "op": "model", "prompt": "hi" }),
            ))
            .await;
        assert_eq!(
            after["error"]["code"],
            plugin_api::wire::REFUSED_CODE,
            "{after}"
        );
    }

    #[tokio::test]
    async fn a_capability_that_wasnt_granted_is_refused_as_such() {
        let (host, _) = host_with_model();
        let run = RunId::generate();
        host.begin(context(run));
        let call = json!({ "op": "http", "method": "GET", "url": "http://x/", "headers": [], "body": null });
        let reply = host.reply_to(&capability_request(run, &call)).await;
        assert_eq!(
            reply["error"]["code"],
            plugin_api::wire::NOT_GRANTED_CODE,
            "{reply}"
        );
    }

    #[tokio::test]
    async fn a_request_that_isnt_a_capability_call_is_refused() {
        let (host, _) = host_with_model();
        let reply = host
            .reply_to(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {} }))
            .await;
        assert_eq!(reply["error"]["code"], -32601, "{reply}");
        let garbled = host
            .reply_to(&json!({ "jsonrpc": "2.0", "id": 1, "method": CAPABILITY_METHOD, "params": { "run": "x" } }))
            .await;
        assert_eq!(garbled["error"]["code"], -32602, "{garbled}");
    }
}
