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

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;
use store::{Direction, Page, PageRequest, Record, RecordId, RecordQuery};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::rpc::{self, Peer, RpcError};
use crate::serve_sources::PluginHandler;
use crate::wire::{
    CAPABILITY_METHOD, CapabilityCall, CapabilityRequest, WireHttpResponse, WireModelReply,
    WirePage, WireRecord, error_of, http_call, page_from, query_call, record_of,
};
use crate::{
    Capabilities, CapabilityError, HttpCapability, HttpRequest, HttpResponse, McpHandler,
    ModelCapability, ModelReply, Plugin, PluginCapability, PluginManifest, RecordsCapability,
    ToolContext,
};

/// The process's side of the connection, which its capabilities call back
/// through.
#[derive(Debug)]
struct Link(Arc<Peer>);

impl Link {
    /// Asks the host, for the tool call `context` belongs to.
    async fn call<T: DeserializeOwned>(
        &self,
        context: ToolContext,
        call: CapabilityCall,
    ) -> Result<T, CapabilityError> {
        let token = context.call.ok_or_else(|| {
            CapabilityError::Refused(
                "only a tool call from the host can use a capability".to_owned(),
            )
        })?;
        let params = serde_json::to_value(CapabilityRequest { token, call })
            .map_err(|error| CapabilityError::Failed(error.to_string()))?;
        let result = self
            .0
            .request(CAPABILITY_METHOD, params, None)
            .await
            .map_err(|error| match error {
                RpcError::Remote { code, message } => error_of(code, &message),
                other => CapabilityError::Failed(other.to_string()),
            })?;
        serde_json::from_value(result)
            .map_err(|error| CapabilityError::Failed(format!("the host's answer: {error}")))
    }
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
        let id: uuid::Uuid = self.0.call(context, call).await?;
        Ok(RecordId::from_uuid(id))
    }

    async fn get(
        &self,
        record_type: &str,
        id: RecordId,
        context: ToolContext,
    ) -> Result<Option<Record>, CapabilityError> {
        let call = CapabilityCall::RecordsGet {
            record_type: record_type.to_owned(),
            id: id.as_uuid(),
        };
        let found: Option<WireRecord> = self.0.call(context, call).await?;
        found.map(record_of).transpose()
    }

    async fn query(
        &self,
        query: RecordQuery,
        context: ToolContext,
    ) -> Result<Page<Record>, CapabilityError> {
        let page: WirePage = self.0.call(context, query_call(query)).await?;
        page_from(page)
    }

    async fn delete(
        &self,
        record_type: &str,
        id: RecordId,
        context: ToolContext,
    ) -> Result<(), CapabilityError> {
        let call = CapabilityCall::RecordsDelete {
            record_type: record_type.to_owned(),
            id: id.as_uuid(),
        };
        self.0.call(context, call).await
    }

    async fn link(
        &self,
        relation: &str,
        from: RecordId,
        to: RecordId,
        context: ToolContext,
    ) -> Result<(), CapabilityError> {
        let call = CapabilityCall::RecordsLink {
            relation: relation.to_owned(),
            from: from.as_uuid(),
            to: to.as_uuid(),
        };
        self.0.call(context, call).await
    }

    async fn put_many(
        &self,
        records: Vec<crate::RecordWrite>,
        context: ToolContext,
    ) -> Result<Vec<RecordId>, CapabilityError> {
        crate::check_batch(records.len())?;
        let call = CapabilityCall::RecordsPutMany {
            records: records
                .into_iter()
                .map(|write| crate::wire::WireRecordWrite {
                    record_type: write.record_type,
                    id: write.id.map(|id| id.as_uuid()),
                    data: write.data,
                })
                .collect(),
        };
        let ids: Vec<uuid::Uuid> = self.0.call(context, call).await?;
        Ok(ids.into_iter().map(RecordId::from_uuid).collect())
    }

    async fn link_many(
        &self,
        links: Vec<crate::RecordLink>,
        context: ToolContext,
    ) -> Result<(), CapabilityError> {
        crate::check_batch(links.len())?;
        let call = CapabilityCall::RecordsLinkMany {
            links: links
                .into_iter()
                .map(|link| crate::wire::WireLink {
                    relation: link.relation,
                    from: link.from.as_uuid(),
                    to: link.to.as_uuid(),
                })
                .collect(),
        };
        self.0.call(context, call).await
    }

    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
        context: ToolContext,
    ) -> Result<Page<Record>, CapabilityError> {
        let call = CapabilityCall::RecordsNeighbors {
            relation: relation.to_owned(),
            id: id.as_uuid(),
            direction: direction.into(),
            limit: page.limit(),
            offset: page.offset(),
        };
        let page: WirePage = self.0.call(context, call).await?;
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
        let response: WireHttpResponse = self.0.call(context, http_call(request)).await?;
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
        let reply: WireModelReply = self.0.call(context, call).await?;
        Ok(reply.into())
    }
}

#[derive(Debug)]
struct RemoteMemory(Arc<Link>);

#[async_trait]
impl crate::MemoryCapability for RemoteMemory {
    async fn store(
        &self,
        note: crate::NoteToStore,
        context: ToolContext,
    ) -> Result<crate::NoteStored, CapabilityError> {
        note.check()?;
        let call = CapabilityCall::MemoryStore {
            title: note.title,
            text: note.text,
            tags: note.tags,
            source_url: note.source_url,
        };
        let stored: crate::wire::WireNoteStored = self.0.call(context, call).await?;
        Ok(stored.into())
    }
}

#[derive(Debug)]
struct RemoteJobs(Arc<Link>);

#[async_trait]
impl crate::JobsCapability for RemoteJobs {
    async fn run_workflow(
        &self,
        run: crate::RunWorkflow,
        context: ToolContext,
    ) -> Result<crate::JobStarted, CapabilityError> {
        let call = CapabilityCall::JobsRunWorkflow {
            workflow: run.workflow,
            inputs: run.inputs,
            priority: run.priority,
            idempotency_key: run.idempotency_key,
        };
        let started: crate::wire::WireJobStarted = self.0.call(context, call).await?;
        Ok(crate::JobStarted::new(
            started.job_id,
            started.already_existed,
        ))
    }

    async fn job(
        &self,
        job_id: uuid::Uuid,
        context: ToolContext,
    ) -> Result<Option<crate::JobState>, CapabilityError> {
        let call = CapabilityCall::JobsGet { job_id };
        let found: Option<crate::wire::WireJobState> = self.0.call(context, call).await?;
        Ok(found.map(Into::into))
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
            PluginCapability::Memory => {
                capabilities.memory = Some(Arc::new(RemoteMemory(Arc::clone(link))));
            }
            PluginCapability::Jobs => {
                capabilities.jobs = Some(Arc::new(RemoteJobs(Arc::clone(link))));
            }
            // Offered, not asked for: there's nothing to call back.
            PluginCapability::Sources => {}
        }
    }
    capabilities
}

/// Serves `plugin` over any byte stream until it ends.
///
/// # Errors
///
/// The I/O error, or the problem if the plugin's manifest or tools can't be
/// used.
pub async fn serve_plugin<R, W>(plugin: &dyn Plugin, reader: R, writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let manifest = PluginManifest::from_yaml(plugin.manifest()).map_err(std::io::Error::other)?;
    let (peer, io) = Peer::new(rpc::Limits::default());
    let link = Arc::new(Link(Arc::clone(&peer)));
    let capabilities = remote_capabilities(&manifest, &link);
    let tools = plugin.tools(&capabilities).map_err(std::io::Error::other)?;
    let sources = plugin
        .sources(&capabilities)
        .map_err(std::io::Error::other)?;
    let declared: BTreeSet<&str> = manifest.sources.iter().map(|s| s.name.as_str()).collect();
    let built: BTreeSet<&str> = sources.iter().map(|source| source.name()).collect();
    if declared != built || built.len() != sources.len() {
        return Err(std::io::Error::other(
            "its sources aren't the ones its manifest declares",
        ));
    }
    let handler = Arc::new(PluginHandler {
        mcp: McpHandler::new(&manifest.name, &manifest.version, tools)?,
        sources: sources
            .iter()
            .map(|source| (source.name().to_owned(), Arc::clone(source)))
            .collect(),
    });
    peer.run(io, reader, writer, handler).await
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#[allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]
mod tests {
    use std::sync::Mutex;

    use protocol::{RunId, Trust};
    use serde_json::json;
    use tokio::io::duplex;

    use super::*;
    use crate::rpc::{Handler, failure, success};
    use crate::wire::{CONTEXT_META, REFUSED_CODE, WireContext};
    use crate::{PluginError, Tool, ToolDefinition, ToolFailure, ToolOutput};

    const MANIFEST: &str = r"
api_version: 1
name: demo
version: 1.0.0
description: d
tools:
  - name: demo_ask
    description: Asks a model.
    input_schema: {type: object, additionalProperties: false, properties: {q: {type: string, maxLength: 10}}}
    output_schema: {type: object, additionalProperties: false, properties: {text: {type: string, maxLength: 100}}}
    effect: read
capabilities: [model]
";

    #[derive(Debug)]
    struct Ask(Arc<dyn ModelCapability>);

    #[async_trait]
    impl Tool for Ask {
        fn definition(&self) -> ToolDefinition {
            PluginManifest::from_yaml(MANIFEST).unwrap().definitions()[0].clone()
        }

        async fn run(
            &self,
            arguments: Value,
            context: ToolContext,
        ) -> Result<ToolOutput, ToolFailure> {
            let prompt = arguments["q"].as_str().unwrap_or_default();
            let reply = self.0.complete(prompt, context).await?;
            Ok(ToolOutput::new(
                json!({ "text": reply.text }),
                Trust::Untrusted,
            ))
        }
    }

    #[derive(Debug)]
    struct Demo;

    impl Plugin for Demo {
        fn manifest(&self) -> &str {
            MANIFEST
        }

        fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
            Ok(vec![Arc::new(Ask(capabilities.model()?))])
        }
    }

    /// The host's side: answers a model call made with the token it sent,
    /// and refuses any other.
    #[derive(Default)]
    struct Host {
        live: Mutex<Option<String>>,
        asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Handler for Host {
        async fn handle(&self, message: Value) -> Option<Value> {
            let id = message.get("id")?.clone();
            let params = &message["params"];
            let token = params["token"].as_str().map(str::to_owned);
            if message["method"] != CAPABILITY_METHOD || token != *self.live.lock().unwrap() {
                return Some(failure(&id, REFUSED_CODE, "not for a call I sent"));
            }
            let prompt = params["call"]["prompt"].as_str().unwrap().to_owned();
            self.asked.lock().unwrap().push(prompt.clone());
            let reply = WireModelReply::from(ModelReply::new(format!("re: {prompt}"), "m", 3));
            Some(success(&id, &serde_json::to_value(reply).unwrap()))
        }
    }

    /// The plugin served over an in-memory pipe, and the host's peer.
    fn connect() -> (Arc<Peer>, Arc<Host>) {
        let (plugin_end, host_end) = duplex(1 << 16);
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(plugin_end);
            serve_plugin(&Demo, reader, writer).await
        });
        let host = Arc::new(Host::default());
        let (peer, io) = Peer::new(rpc::Limits::default());
        let (running, handler) = (Arc::clone(&peer), Arc::clone(&host));
        tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(host_end);
            running.run(io, reader, writer, handler).await
        });
        (peer, host)
    }

    fn call_params(question: &str, token: Option<uuid::Uuid>) -> Value {
        let mut params = json!({ "name": "demo_ask", "arguments": { "q": question } });
        if let Some(token) = token {
            let context = WireContext {
                run: RunId::generate().as_uuid(),
                public: false,
                budget_usd_micros: 9,
                call: token,
            };
            params["_meta"] = json!({ CONTEXT_META: context });
        }
        params
    }

    #[tokio::test]
    async fn a_tool_reaches_the_host_with_its_calls_token() {
        let (peer, host) = connect();
        let listed = peer.request("tools/list", json!({}), None).await.unwrap();
        assert_eq!(listed["tools"][0]["name"], "demo_ask");

        let token = uuid::Uuid::new_v4();
        *host.live.lock().unwrap() = Some(token.to_string());
        let result = peer
            .request("tools/call", call_params("hi", Some(token)), None)
            .await
            .unwrap();
        assert_eq!(result["structuredContent"]["text"], "re: hi", "{result}");
        assert_eq!(host.asked.lock().unwrap().as_slice(), ["hi"]);
    }

    #[tokio::test]
    async fn a_call_the_host_didnt_send_gets_no_capability() {
        let (peer, host) = connect();
        // No context at all: the process refuses before asking the host.
        let result = peer
            .request("tools/call", call_params("hi", None), None)
            .await
            .unwrap();
        assert_eq!(result["isError"], true, "{result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("only a tool call from the host"),
            "{result}"
        );
        // A token the host no longer honours: its refusal comes back as such.
        let result = peer
            .request(
                "tools/call",
                call_params("hi", Some(uuid::Uuid::new_v4())),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result["isError"], true, "{result}");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("refused: not for a call"),
            "{result}"
        );
        assert!(host.asked.lock().unwrap().is_empty());
    }
}
