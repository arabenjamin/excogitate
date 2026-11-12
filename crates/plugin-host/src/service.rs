//! A plugin in its own process: the client side of MCP over stdio.
//!
//! One task drives the connection through the shared JSON-RPC peer. A call
//! that's abandoned, such as by a run's time limit, leaves neither stream
//! mid-message, and the process is told to stop it.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use dispatch::{Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use plugin_api::rpc::{self, INVALID_PARAMS, METHOD_NOT_FOUND, Peer, RpcError, failure, success};
use plugin_api::wire::{
    CAPABILITY_METHOD, CONTEXT_META, CapabilityCall, CapabilityRequest, SOURCE_GET_METHOD,
    SOURCE_SEARCH_METHOD, WireContext, WireHttpResponse, WireModelReply, WireSourceFound,
    WireSourceGet, WireSourceGot, WireSourceSearch, error_code, error_message, page_of, query_of,
    wire_page, wire_record,
};
use plugin_api::{
    Capabilities, CapabilityError, HttpRequest, MCP_VERSION, PluginManifest, Source, SourceFound,
    SourceGot, SourceItem, SourceQuery,
};
use protocol::{DataClass, Trust};
use schema::{Validator, bounded};
use serde_json::{Value, json};
use store::RecordId;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// What one process may have the host do for it at once: capability calls
/// running together. More are refused, not queued.
const LIMITS: rpc::Limits = rpc::Limits {
    in_flight: 8,
    queue: 64,
};

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

/// What a capability request must meet.
static REQUEST_SCHEMA: LazyLock<Option<Validator>> =
    LazyLock::new(|| Validator::for_type::<CapabilityRequest>().ok());

/// One tool call the host has out with the process.
#[derive(Debug)]
struct OutCall {
    context: ToolContext,
    ended: watch::Receiver<bool>,
}

/// What a process may ask of the host: the capabilities it was granted,
/// for the tool calls the host is serving on its connection right now.
#[derive(Debug, Default)]
struct HostCalls {
    capabilities: Capabilities,
    /// Each tool call in flight, by the token it was sent with.
    serving: Mutex<HashMap<Uuid, OutCall>>,
}

impl HostCalls {
    fn serving(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, OutCall>> {
        self.serving.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens a call's window for capability calls. It closes when the guard
    /// is dropped, and whatever is still running for it is stopped.
    fn begin(&self, context: ToolContext) -> EndCall<'_> {
        let token = Uuid::new_v4();
        let (ending, ended) = watch::channel(false);
        self.serving().insert(token, OutCall { context, ended });
        EndCall {
            host: self,
            token,
            ending,
        }
    }

    /// The host's own record of the call, if it's still running.
    fn call(&self, token: Uuid) -> Option<(ToolContext, watch::Receiver<bool>)> {
        self.serving()
            .get(&token)
            .map(|call| (call.context, call.ended.clone()))
    }

    async fn http(
        &self,
        mut request: HttpRequest,
        headers: impl Iterator<Item = (String, String)> + Send,
        body: Option<String>,
        context: ToolContext,
    ) -> Result<Value, CapabilityError> {
        request.headers = headers.collect();
        request.body = body;
        let response = self.capabilities.http()?.request(request, context).await?;
        serde_json::to_value(WireHttpResponse::from(response))
            .map_err(|error| CapabilityError::Failed(error.to_string()))
    }

    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: store::Direction,
        page: store::PageRequest,
        context: ToolContext,
    ) -> Result<Value, CapabilityError> {
        let found = self
            .capabilities
            .records()?
            .neighbors(relation, id, direction, page, context)
            .await?;
        serde_json::to_value(wire_page(found))
            .map_err(|error| CapabilityError::Failed(error.to_string()))
    }

    async fn run_workflow(
        &self,
        run: plugin_api::RunWorkflow,
        context: ToolContext,
    ) -> Result<Value, CapabilityError> {
        let started = self.capabilities.jobs()?.run_workflow(run, context).await?;
        let wire = plugin_api::wire::WireJobStarted {
            job_id: started.job_id,
            already_existed: started.already_existed,
        };
        serde_json::to_value(wire).map_err(|error| CapabilityError::Failed(error.to_string()))
    }

    async fn job(&self, job_id: Uuid, context: ToolContext) -> Result<Value, CapabilityError> {
        let found = self.capabilities.jobs()?.job(job_id, context).await?;
        serde_json::to_value(found.map(plugin_api::wire::WireJobState::from))
            .map_err(|error| CapabilityError::Failed(error.to_string()))
    }

    async fn store_note(
        &self,
        mut note: plugin_api::NoteToStore,
        source_url: Option<String>,
        context: ToolContext,
    ) -> Result<Value, CapabilityError> {
        if let Some(url) = source_url {
            note = note.with_source_url(url);
        }
        let stored = self.capabilities.memory()?.store(note, context).await?;
        serde_json::to_value(plugin_api::wire::WireNoteStored::from(stored))
            .map_err(|error| CapabilityError::Failed(error.to_string()))
    }

    async fn put_many(
        &self,
        records: Vec<plugin_api::wire::WireRecordWrite>,
        context: ToolContext,
    ) -> Result<Value, CapabilityError> {
        let writes = records
            .into_iter()
            .map(|write| {
                let record = plugin_api::RecordWrite::new(write.record_type, write.data);
                match write.id {
                    Some(id) => record.with_id(RecordId::from_uuid(id)),
                    None => record,
                }
            })
            .collect();
        let ids = self
            .capabilities
            .records()?
            .put_many(writes, context)
            .await?;
        Ok(Value::Array(
            ids.into_iter()
                .map(|id| Value::String(id.to_string()))
                .collect(),
        ))
    }

    async fn link_many(
        &self,
        links: Vec<plugin_api::wire::WireLink>,
        context: ToolContext,
    ) -> Result<Value, CapabilityError> {
        let links = links
            .into_iter()
            .map(|link| {
                plugin_api::RecordLink::new(
                    link.relation,
                    RecordId::from_uuid(link.from),
                    RecordId::from_uuid(link.to),
                )
            })
            .collect();
        self.capabilities
            .records()?
            .link_many(links, context)
            .await?;
        Ok(Value::Null)
    }

    /// The records calls, which are most of them.
    async fn answer_records(
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
                    .get(&record_type, RecordId::from_uuid(id), context)
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
                let page = self.capabilities.records()?.query(query, context).await?;
                json(serde_json::to_value(wire_page(page)))
            }
            CapabilityCall::RecordsDelete { record_type, id } => {
                self.capabilities
                    .records()?
                    .delete(&record_type, RecordId::from_uuid(id), context)
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
                        context,
                    )
                    .await?;
                Ok(Value::Null)
            }
            CapabilityCall::RecordsPutMany { records } => self.put_many(records, context).await,
            CapabilityCall::RecordsLinkMany { links } => self.link_many(links, context).await,
            CapabilityCall::RecordsNeighbors {
                relation,
                id,
                direction,
                limit,
                offset,
            } => {
                let page = page_of(limit, offset)?;
                let id = RecordId::from_uuid(id);
                self.neighbors(&relation, id, direction.into(), page, context)
                    .await
            }
            // Every other call is answered before it gets here.
            _ => Err(CapabilityError::Refused("not a records call".to_owned())),
        }
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
            CapabilityCall::Http {
                method,
                url,
                headers,
                body,
            } => {
                let headers = headers
                    .into_iter()
                    .map(|header| (header.name, header.value));
                self.http(HttpRequest::new(method, url), headers, body, context)
                    .await
            }
            CapabilityCall::Model { prompt } => {
                let reply = self
                    .capabilities
                    .model()?
                    .complete(&prompt, context)
                    .await?;
                json(serde_json::to_value(WireModelReply::from(reply)))
            }
            CapabilityCall::JobsRunWorkflow {
                workflow,
                inputs,
                priority,
                idempotency_key,
            } => {
                let mut run =
                    plugin_api::RunWorkflow::new(workflow, inputs).with_priority(priority);
                run.idempotency_key = idempotency_key;
                self.run_workflow(run, context).await
            }
            CapabilityCall::JobsGet { job_id } => self.job(job_id, context).await,
            CapabilityCall::MemoryStore {
                title,
                text,
                tags,
                source_url,
            } => {
                let note = plugin_api::NoteToStore::new(title, text).with_tags(tags);
                self.store_note(note, source_url, context).await
            }
            records => self.answer_records(records, context).await,
        }
    }

    /// The reply to a request the process sent.
    async fn reply_to(&self, message: &Value) -> Value {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if message.get("method").and_then(Value::as_str) != Some(CAPABILITY_METHOD) {
            return failure(
                &id,
                METHOD_NOT_FOUND,
                "the host answers only capabilities/call",
            );
        }
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        // The published schema, bounds and all, before anything else.
        let checked = REQUEST_SCHEMA
            .as_ref()
            .ok_or_else(|| "the capability schema doesn't compile".to_owned())
            .and_then(|schema| {
                schema
                    .parse::<CapabilityRequest>(params)
                    .map_err(|refusal| refusal.to_string())
            });
        let CapabilityRequest { token, call } = match checked {
            Ok(request) => request,
            Err(problem) => return failure(&id, INVALID_PARAMS, &problem),
        };
        // The host's record of the call, never the process's: it can't claim
        // a bigger budget, act outside a call, or reach another one.
        let Some((context, mut ended)) = self.call(token) else {
            return failure(
                &id,
                plugin_api::wire::REFUSED_CODE,
                "no tool call with that token is running",
            );
        };
        let answered = tokio::select! {
            answered = self.answer(call, context) => answered,
            _ = ended.wait_for(|ended| *ended) => Err(CapabilityError::Refused(
                "the tool call it was for has ended".to_owned(),
            )),
        };
        match answered {
            Ok(result) => success(&id, &result),
            Err(error) => failure(&id, error_code(&error), &error_message(&error)),
        }
    }
}

#[async_trait]
impl rpc::Handler for HostCalls {
    async fn handle(&self, message: Value) -> Option<Value> {
        // The process's notifications need nothing from the host.
        message.get("id")?;
        Some(self.reply_to(&message).await)
    }
}

fn service_error(error: RpcError) -> ServiceError {
    match error {
        RpcError::Timeout => ServiceError::Timeout,
        RpcError::Remote { message, .. } => ServiceError::Remote(message),
        RpcError::Malformed(problem) => ServiceError::Protocol(problem),
        _ => ServiceError::Stopped,
    }
}

/// Stops the connection's task and, through `kill_on_drop`, the process.
#[derive(Debug)]
struct Connection {
    host: Arc<HostCalls>,
    peer: Arc<Peer>,
    limit: Duration,
    running: JoinHandle<()>,
    _child: Child,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.running.abort();
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
        let host = Arc::new(HostCalls {
            capabilities,
            serving: Mutex::default(),
        });
        let (peer, io) = Peer::new(LIMITS);
        let (driving, handler) = (Arc::clone(&peer), Arc::clone(&host));
        let running = tokio::spawn(async move {
            // Ends when the process closes its stdout or dies; every request
            // still waiting then hears the service stopped.
            let _ = driving.run(io, stdout, stdin, handler).await;
        });
        Ok(Self {
            host,
            peer,
            limit: Duration::from_secs(u64::from(spec.timeout_secs)),
            running,
            _child: child,
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ServiceError> {
        self.peer
            .request(method, params, Some(self.limit))
            .await
            .map_err(service_error)
    }

    async fn notify(&self, method: &str) -> Result<(), ServiceError> {
        self.peer
            .notify(method, Value::Null)
            .await
            .map_err(service_error)
    }
}

/// Ends a call's window for capability calls, even if it's abandoned, and
/// stops what's still running for it.
struct EndCall<'a> {
    host: &'a HostCalls,
    token: Uuid,
    ending: watch::Sender<bool>,
}

impl EndCall<'_> {
    const fn token(&self) -> Uuid {
        self.token
    }
}

impl Drop for EndCall<'_> {
    fn drop(&mut self) {
        self.host.serving().remove(&self.token);
        let _ = self.ending.send(true);
    }
}

/// What a started process offers: its tools and its sources.
pub(crate) type Offered = (Vec<Arc<dyn Tool>>, Vec<Arc<dyn Source>>);

/// Starts the process and checks it against the manifest. Returns one tool
/// per declared tool, and one source per declared source. The manifest is
/// the authority: a server can't offer more than it declares, or loosen a
/// declared schema.
pub(crate) async fn start(
    manifest: &PluginManifest,
    spec: &ServiceSpec,
    capabilities: Capabilities,
) -> Result<Offered, ServiceError> {
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
    let sources = manifest
        .sources
        .iter()
        .map(|declared| {
            Arc::new(ServiceSource {
                name: declared.name.clone(),
                connection: Arc::clone(&connection),
            }) as Arc<dyn Source>
        })
        .collect();
    Ok((tools, sources))
}

/// The context a call carries to a plugin's process, with the host's token
/// for it.
fn wire_context(context: ToolContext, call: Uuid) -> WireContext {
    WireContext {
        run: context.run.as_uuid(),
        public: context.data_class == DataClass::Public,
        budget_usd_micros: context.budget_usd_micros,
        call,
    }
}

/// A source in a plugin's process, reached over its connection.
#[derive(Debug)]
struct ServiceSource {
    name: String,
    connection: Arc<Connection>,
}

impl ServiceSource {
    /// Sends `params` with the call's context, as a tool call does, so the
    /// process may use its capabilities for it while it runs.
    async fn ask<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        mut params: Value,
        context: ToolContext,
    ) -> Result<T, ToolFailure> {
        let serving = self.connection.host.begin(context);
        let sent = wire_context(context, serving.token());
        if let Some(fields) = params.as_object_mut() {
            fields.insert("_meta".to_owned(), json!({ CONTEXT_META: sent }));
        }
        let reply = self.connection.request(method, params).await;
        drop(serving);
        let reply = reply.map_err(|error| ToolFailure::new(bounded(&error.to_string())))?;
        serde_json::from_value(reply)
            .map_err(|error| ToolFailure::new(bounded(&format!("the source's answer: {error}"))))
    }
}

#[async_trait]
impl Source for ServiceSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn search(
        &self,
        query: SourceQuery,
        context: ToolContext,
    ) -> Result<SourceFound, ToolFailure> {
        let asked = WireSourceSearch {
            source: self.name.clone(),
            text: query.text,
            vector: query.vector,
            limit: query.limit,
            meta: None,
        };
        let params = serde_json::to_value(asked).map_err(|error| error.to_string())?;
        let found: WireSourceFound = self.ask(SOURCE_SEARCH_METHOD, params, context).await?;
        Ok(
            SourceFound::new(found.hits.into_iter().map(SourceItem::from).collect())
                .with_cost(found.cost_usd_micros),
        )
    }

    async fn get(&self, id: &str, context: ToolContext) -> Result<SourceGot, ToolFailure> {
        let asked = WireSourceGet {
            source: self.name.clone(),
            id: id.to_owned(),
            meta: None,
        };
        let params = serde_json::to_value(asked).map_err(|error| error.to_string())?;
        let got: WireSourceGot = self.ask(SOURCE_GET_METHOD, params, context).await?;
        Ok(SourceGot::new(got.item.map(SourceItem::from)).with_cost(got.cost_usd_micros))
    }
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
        return Err(ToolFailure::new(bounded(text.unwrap_or("the tool failed"))));
    }
    if let Some(value) = result.get("structuredContent") {
        return Ok(value.clone());
    }
    text.and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| ToolFailure::new("the service returned no JSON result".to_owned()))
}

#[async_trait]
impl Tool for ServiceTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        // While the call is out, the process may ask for capabilities on its
        // behalf, with this call's token, and only then.
        let serving = self.connection.host.begin(context);
        let sent = wire_context(context, serving.token());
        let params = json!({
            "name": self.definition.name,
            "arguments": arguments,
            "_meta": { CONTEXT_META: sent },
        });
        let result = self.connection.request("tools/call", params).await;
        drop(serving);
        let result = result.map_err(|error| ToolFailure::new(error.to_string()))?;
        // Another process's output is never trusted as instructions.
        Ok(ToolOutput::new(result_value(&result)?, Trust::Untrusted))
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use protocol::RunId;

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
    async fn a_silent_service_times_out() {
        let connection =
            Connection::open(&shell("sleep 30", &[], 1), Capabilities::default()).unwrap();
        let error = connection.request("x", json!({})).await.unwrap_err();
        // The peer's own tests check that the call is forgotten.
        assert_eq!(error, ServiceError::Timeout);
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
        assert_eq!(
            result_value(&result),
            Err(ToolFailure::new("broke".to_owned()))
        );
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
            prompt: &str,
            context: ToolContext,
        ) -> Result<plugin_api::ModelReply, CapabilityError> {
            self.0.lock().unwrap().push(context);
            if prompt == "slow" {
                std::future::pending::<()>().await;
            }
            Ok(plugin_api::ModelReply::new("ok", "m", 5))
        }
    }

    fn host_with_model() -> (HostCalls, Arc<SeenModel>) {
        let seen = Arc::new(SeenModel::default());
        let capabilities = Capabilities::default()
            .with_model(Arc::clone(&seen) as Arc<dyn plugin_api::ModelCapability>);
        let host = HostCalls {
            capabilities,
            serving: Mutex::default(),
        };
        (host, seen)
    }

    fn capability_request(token: Uuid, call: &Value) -> Value {
        json!({
            "jsonrpc": "2.0", "id": 9, "method": CAPABILITY_METHOD,
            "params": { "token": token.to_string(), "call": call }
        })
    }

    fn context(run: RunId) -> ToolContext {
        ToolContext::new(run, DataClass::Public, 77)
    }

    const MODEL_CALL: &str = r#"{ "op": "model", "prompt": "hi" }"#;

    fn model_call() -> Value {
        serde_json::from_str(MODEL_CALL).unwrap()
    }

    #[tokio::test]
    async fn a_capability_call_without_a_live_token_is_refused() {
        let (host, seen) = host_with_model();
        let reply = host
            .reply_to(&capability_request(Uuid::new_v4(), &model_call()))
            .await;
        assert_eq!(
            reply["error"]["code"],
            plugin_api::wire::REFUSED_CODE,
            "{reply}"
        );
        assert!(seen.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_host_uses_its_own_record_of_the_call_and_only_while_it_runs() {
        let (host, seen) = host_with_model();
        let run = RunId::generate();
        let serving = host.begin(context(run));
        let token = serving.token();
        let reply = host
            .reply_to(&capability_request(token, &model_call()))
            .await;
        assert_eq!(reply["result"]["text"], "ok", "{reply}");
        assert_eq!(seen.0.lock().unwrap().as_slice(), [context(run)]);
        drop(serving);
        let after = host
            .reply_to(&capability_request(token, &model_call()))
            .await;
        assert_eq!(
            after["error"]["code"],
            plugin_api::wire::REFUSED_CODE,
            "{after}"
        );
    }

    #[tokio::test]
    async fn two_calls_for_one_run_each_keep_their_own_record() {
        let (host, seen) = host_with_model();
        let run = RunId::generate();
        let first = host.begin(ToolContext::new(run, DataClass::Private, 10));
        let second = host.begin(ToolContext::new(run, DataClass::Public, 20));
        for serving in [&first, &second] {
            let reply = host
                .reply_to(&capability_request(serving.token(), &model_call()))
                .await;
            assert_eq!(reply["result"]["text"], "ok", "{reply}");
        }
        let budgets: Vec<u64> = seen
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|context| context.budget_usd_micros)
            .collect();
        assert_eq!(budgets, [10, 20]);
    }

    #[tokio::test]
    async fn a_capability_call_still_running_when_its_tool_call_ends_is_stopped() {
        let (host, _) = host_with_model();
        let host = Arc::new(host);
        let serving = host.begin(context(RunId::generate()));
        let token = serving.token();
        // A model call that never finishes on its own.
        let slow = {
            let host = Arc::clone(&host);
            tokio::spawn(async move {
                host.reply_to(&capability_request(
                    token,
                    &json!({ "op": "model", "prompt": "slow" }),
                ))
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(serving);
        let reply = tokio::time::timeout(Duration::from_secs(5), slow)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("has ended"),
            "{reply}"
        );
    }

    #[tokio::test]
    async fn a_capability_that_wasnt_granted_is_refused_as_such() {
        let (host, _) = host_with_model();
        let serving = host.begin(context(RunId::generate()));
        let call = json!({ "op": "http", "method": "GET", "url": "http://x/", "headers": [], "body": null });
        let reply = host
            .reply_to(&capability_request(serving.token(), &call))
            .await;
        assert_eq!(
            reply["error"]["code"],
            plugin_api::wire::NOT_GRANTED_CODE,
            "{reply}"
        );
    }

    #[tokio::test]
    async fn a_capability_call_past_the_published_bounds_is_refused_unrun() {
        let (host, seen) = host_with_model();
        let serving = host.begin(context(RunId::generate()));
        let long = "x".repeat(plugin_api::wire::MAX_PROMPT + 1);
        let reply = host
            .reply_to(&capability_request(
                serving.token(),
                &json!({ "op": "model", "prompt": long }),
            ))
            .await;
        assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{reply}");
        assert!(seen.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_request_that_isnt_a_capability_call_is_refused() {
        let (host, _) = host_with_model();
        let reply = host
            .reply_to(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {} }))
            .await;
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND, "{reply}");
        let garbled = host
            .reply_to(&json!({ "jsonrpc": "2.0", "id": 1, "method": CAPABILITY_METHOD, "params": { "token": "x" } }))
            .await;
        assert_eq!(garbled["error"]["code"], INVALID_PARAMS, "{garbled}");
    }

    #[tokio::test]
    async fn a_batch_from_a_process_is_written_as_one() {
        use plugin_api::wire::{CapabilityCall, WireRecordWrite};
        let store = Arc::new(store_memory::MemoryStore::new(3));
        let decls: store::RecordDecls = serde_json::from_value(json!({
            "types": [{ "name": "demo.a", "filterable": [],
                        "schema": { "type": "object", "additionalProperties": false,
                                    "properties": { "a": { "type": "integer" } } } }],
            "relations": []
        }))
        .unwrap();
        store::RecordReader::register_records(store.as_ref(), &decls)
            .await
            .unwrap();
        let records =
            crate::ScopedRecords::new(Arc::clone(&store) as Arc<dyn store::Store>, "demo");
        let host = HostCalls {
            capabilities: Capabilities::default()
                .with_records(Arc::new(records) as Arc<dyn plugin_api::RecordsCapability>),
            serving: Mutex::default(),
        };
        let serving = host.begin(context(RunId::generate()));
        // What the process side sends for put_many.
        let call = serde_json::to_value(CapabilityCall::RecordsPutMany {
            records: (1..=2)
                .map(|n| WireRecordWrite {
                    record_type: "demo.a".to_owned(),
                    id: None,
                    data: json!({ "a": n }),
                })
                .collect(),
        })
        .unwrap();
        let reply = host
            .reply_to(&capability_request(serving.token(), &call))
            .await;
        let ids = reply["result"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply}"));
        assert_eq!(ids.len(), 2);
        let page = store::PageRequest::new(10, 0).unwrap();
        let stored = store::RecordReader::query_records(
            store.as_ref(),
            store::RecordQuery::new("demo.a".to_owned(), page),
        )
        .await
        .unwrap();
        assert_eq!(stored.items.len(), 2);
    }

    /// Keeps what it's asked to store and start, and answers as the
    /// runtime would.
    #[derive(Debug, Default)]
    struct Remembers(Mutex<Vec<String>>);

    #[async_trait]
    impl plugin_api::MemoryCapability for Remembers {
        async fn store(
            &self,
            note: plugin_api::NoteToStore,
            _: ToolContext,
        ) -> Result<plugin_api::NoteStored, CapabilityError> {
            self.0.lock().unwrap().push(note.text);
            Ok(plugin_api::NoteStored::new(Uuid::nil(), true, 1, 0).with_cost(3))
        }
    }

    #[async_trait]
    impl plugin_api::JobsCapability for Remembers {
        async fn run_workflow(
            &self,
            run: plugin_api::RunWorkflow,
            _: ToolContext,
        ) -> Result<plugin_api::JobStarted, CapabilityError> {
            self.0.lock().unwrap().push(run.workflow);
            Ok(plugin_api::JobStarted::new(Uuid::nil(), false))
        }

        async fn job(
            &self,
            job_id: Uuid,
            _: ToolContext,
        ) -> Result<Option<plugin_api::JobState>, CapabilityError> {
            Ok(Some(
                plugin_api::JobState::new(job_id, plugin_api::JobPhase::Succeeded)
                    .with_outcome(None, Some(json!({ "answer": 42 }))),
            ))
        }
    }

    #[tokio::test]
    async fn a_note_and_a_workflow_run_from_a_process_reach_their_capabilities() {
        use plugin_api::wire::{CapabilityCall, WireJobStarted, WireJobState, WireNoteStored};
        let both = Arc::new(Remembers::default());
        let host = HostCalls {
            capabilities: Capabilities::default()
                .with_memory(Arc::clone(&both) as Arc<dyn plugin_api::MemoryCapability>)
                .with_jobs(Arc::clone(&both) as Arc<dyn plugin_api::JobsCapability>),
            serving: Mutex::default(),
        };
        let serving = host.begin(context(RunId::generate()));
        let ask = |call: CapabilityCall| {
            capability_request(serving.token(), &serde_json::to_value(call).unwrap())
        };
        let stored = host
            .reply_to(&ask(CapabilityCall::MemoryStore {
                title: String::new(),
                text: "Otters hold hands.".to_owned(),
                tags: vec![],
                source_url: None,
            }))
            .await;
        let stored: WireNoteStored = serde_json::from_value(stored["result"].clone())
            .unwrap_or_else(|error| panic!("{error}: {stored}"));
        assert_eq!((stored.chunks, stored.cost_usd_micros), (1, 3));
        let started = host
            .reply_to(&ask(CapabilityCall::JobsRunWorkflow {
                workflow: "learn".to_owned(),
                inputs: json!({}),
                priority: 0,
                idempotency_key: Some("k".to_owned()),
            }))
            .await;
        let started: WireJobStarted = serde_json::from_value(started["result"].clone())
            .unwrap_or_else(|error| panic!("{error}: {started}"));
        assert!(!started.already_existed);
        let state = host
            .reply_to(&ask(CapabilityCall::JobsGet {
                job_id: started.job_id,
            }))
            .await;
        let state: Option<WireJobState> = serde_json::from_value(state["result"].clone())
            .unwrap_or_else(|error| panic!("{error}: {state}"));
        assert_eq!(state.unwrap().result, Some(json!({ "answer": 42 })));
        assert_eq!(*both.0.lock().unwrap(), ["Otters hold hands.", "learn"]);
    }
}
