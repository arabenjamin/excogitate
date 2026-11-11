//! A whole plugin in its own process, with the capabilities it declared.
//!
//! [`serve_plugin_stdio`] runs any [`Plugin`] as an MCP server over stdio.
//! Its tools get the capabilities its manifest declares as proxies: each
//! call goes back to the host over the same connection, for the tool call
//! it's made in. The host checks it against what the deployment granted and
//! uses its own record of the run, so the process can't widen either.
//!
//! Messages are read and written concurrently, so a tool waiting on the
//! host's answer doesn't stop that answer being read.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use protocol::RunId;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use store::{Direction, Page, PageRequest, Record, RecordId, RecordQuery};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::wire::{
    CAPABILITY_METHOD, CONTEXT_META, CapabilityCall, WireContext, WireHttpResponse, WireModelReply,
    WirePage, WireRecord, error_of, http_call, page_from, query_call, record_of,
};
use crate::{
    Capabilities, CapabilityError, HttpCapability, HttpRequest, HttpResponse, McpHandler,
    ModelCapability, ModelReply, Plugin, PluginCapability, PluginManifest, RecordsCapability,
    ToolContext, read_line,
};

tokio::task_local! {
    /// The run of the tool call being served, for capability calls that
    /// don't carry a context of their own.
    static CURRENT_RUN: RunId;
}

type Waiters = Mutex<HashMap<u64, oneshot::Sender<Value>>>;

/// The process's side of the connection: what it sends the host, and who's
/// waiting for which answer.
#[derive(Debug)]
struct Link {
    outgoing: mpsc::Sender<String>,
    waiting: Waiters,
    next_id: AtomicU64,
}

impl Link {
    fn waiters(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Value>>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn call<T: DeserializeOwned>(
        &self,
        run: RunId,
        call: CapabilityCall,
    ) -> Result<T, CapabilityError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.waiters().insert(id, sender);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": CAPABILITY_METHOD,
            "params": { "run": run.as_uuid(), "call": call },
        });
        let gone = || CapabilityError::Failed("the host has gone".to_owned());
        if self.outgoing.send(message.to_string()).await.is_err() {
            self.waiters().remove(&id);
            return Err(gone());
        }
        let reply = receiver.await.map_err(|_| gone())?;
        if let Some(error) = reply.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("failed");
            return Err(error_of(code, message));
        }
        serde_json::from_value(reply.get("result").cloned().unwrap_or(Value::Null))
            .map_err(|error| CapabilityError::Failed(format!("the host's answer: {error}")))
    }

    fn answer(&self, reply: Value) {
        let waiter = reply
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| self.waiters().remove(&id));
        if let Some(waiter) = waiter {
            // The tool may have given up; that's fine.
            let _ = waiter.send(reply);
        }
    }
}

/// The run of the call being served.
fn current_run() -> Result<RunId, CapabilityError> {
    CURRENT_RUN
        .try_with(|run| *run)
        .map_err(|_| CapabilityError::Refused("only a tool call can use a capability".to_owned()))
}

#[derive(Debug)]
struct RemoteRecords(Arc<Link>);

#[async_trait]
impl RecordsCapability for RemoteRecords {
    async fn put(
        &self,
        record_type: &str,
        id: Option<RecordId>,
        data: Value,
        context: ToolContext,
    ) -> Result<RecordId, CapabilityError> {
        let call = CapabilityCall::RecordsPut {
            record_type: record_type.to_owned(),
            id: id.map(|id| id.as_uuid()),
            data,
        };
        let id: uuid::Uuid = self.0.call(context.run, call).await?;
        Ok(RecordId::from_uuid(id))
    }

    async fn get(
        &self,
        record_type: &str,
        id: RecordId,
    ) -> Result<Option<Record>, CapabilityError> {
        let call = CapabilityCall::RecordsGet {
            record_type: record_type.to_owned(),
            id: id.as_uuid(),
        };
        let found: Option<WireRecord> = self.0.call(current_run()?, call).await?;
        found.map(record_of).transpose()
    }

    async fn query(&self, query: RecordQuery) -> Result<Page<Record>, CapabilityError> {
        let page: WirePage = self.0.call(current_run()?, query_call(query)).await?;
        page_from(page)
    }

    async fn delete(&self, record_type: &str, id: RecordId) -> Result<(), CapabilityError> {
        let call = CapabilityCall::RecordsDelete {
            record_type: record_type.to_owned(),
            id: id.as_uuid(),
        };
        self.0.call(current_run()?, call).await
    }

    async fn link(
        &self,
        relation: &str,
        from: RecordId,
        to: RecordId,
    ) -> Result<(), CapabilityError> {
        let call = CapabilityCall::RecordsLink {
            relation: relation.to_owned(),
            from: from.as_uuid(),
            to: to.as_uuid(),
        };
        self.0.call(current_run()?, call).await
    }

    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
    ) -> Result<Page<Record>, CapabilityError> {
        let call = CapabilityCall::RecordsNeighbors {
            relation: relation.to_owned(),
            id: id.as_uuid(),
            direction: direction.into(),
            limit: page.limit(),
            offset: page.offset(),
        };
        let page: WirePage = self.0.call(current_run()?, call).await?;
        page_from(page)
    }
}

#[derive(Debug)]
struct RemoteHttp(Arc<Link>);

#[async_trait]
impl HttpCapability for RemoteHttp {
    async fn request(
        &self,
        request: HttpRequest,
        context: ToolContext,
    ) -> Result<HttpResponse, CapabilityError> {
        let response: WireHttpResponse = self.0.call(context.run, http_call(request)).await?;
        Ok(response.into())
    }
}

#[derive(Debug)]
struct RemoteModel(Arc<Link>);

#[async_trait]
impl ModelCapability for RemoteModel {
    async fn complete(
        &self,
        prompt: &str,
        context: ToolContext,
    ) -> Result<ModelReply, CapabilityError> {
        let call = CapabilityCall::Model {
            prompt: prompt.to_owned(),
        };
        let reply: WireModelReply = self.0.call(context.run, call).await?;
        Ok(reply.into())
    }
}

/// Proxies for the capabilities the manifest declares. The host refuses to
/// start a plugin that asks for one it wasn't granted, so these are the ones
/// it has.
fn remote_capabilities(manifest: &PluginManifest, link: &Arc<Link>) -> Capabilities {
    let mut capabilities = Capabilities::default();
    for capability in &manifest.capabilities {
        match capability {
            PluginCapability::Records => {
                capabilities.records = Some(Arc::new(RemoteRecords(Arc::clone(link))));
            }
            PluginCapability::Http => {
                capabilities.http = Some(Arc::new(RemoteHttp(Arc::clone(link))));
            }
            PluginCapability::Model => {
                capabilities.model = Some(Arc::new(RemoteModel(Arc::clone(link))));
            }
        }
    }
    capabilities
}

/// The run a tool call is for, from its `_meta`.
fn run_of(message: &Value) -> Option<RunId> {
    let context = message.get("params")?.get("_meta")?.get(CONTEXT_META)?;
    let context: WireContext = serde_json::from_value(context.clone()).ok()?;
    Some(RunId::from_uuid(context.run))
}

/// Serves `plugin` over any byte stream until it ends.
///
/// # Errors
///
/// The I/O error, or the problem if the plugin's manifest or tools can't be
/// used.
pub async fn serve_plugin<R, W>(
    plugin: &dyn Plugin,
    reader: R,
    mut writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let manifest = PluginManifest::from_yaml(plugin.manifest()).map_err(std::io::Error::other)?;
    let (outgoing, mut queue) = mpsc::channel::<String>(64);
    let link = Arc::new(Link {
        outgoing: outgoing.clone(),
        waiting: Mutex::default(),
        next_id: AtomicU64::new(1),
    });
    let tools = plugin
        .tools(&remote_capabilities(&manifest, &link))
        .map_err(std::io::Error::other)?;
    let handler = Arc::new(McpHandler::new(&manifest.name, &manifest.version, tools)?);
    let writing = tokio::spawn(async move {
        while let Some(mut line) = queue.recv().await {
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    let mut reader = BufReader::new(reader);
    while let Some(line) = read_line(&mut reader).await? {
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if message.get("method").is_none() {
            link.answer(message);
            continue;
        }
        let (handler, outgoing) = (Arc::clone(&handler), outgoing.clone());
        tokio::spawn(async move {
            let reply = match run_of(&message) {
                Some(run) => CURRENT_RUN.scope(run, handler.handle(&message)).await,
                None => handler.handle(&message).await,
            };
            if let Some(reply) = reply {
                let _ = outgoing.send(reply).await;
            }
        });
    }
    // Anyone still waiting on the host hears it's gone.
    link.waiters().clear();
    drop(outgoing);
    writing.abort();
    Ok(())
}

/// Serves `plugin` as an MCP server on stdin and stdout, with the
/// capabilities its manifest declares reaching back to the host.
///
/// # Errors
///
/// The I/O error, or the problem if the plugin can't be set up.
pub async fn serve_plugin_stdio(plugin: &dyn Plugin) -> std::io::Result<()> {
    serve_plugin(plugin, tokio::io::stdin(), tokio::io::stdout()).await
}
