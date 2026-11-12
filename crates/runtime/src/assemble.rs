//! Builds the runtime from its parts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dispatch::{StopSignal, ToolRegistry};
use events::EventSink;
use memory::{ChunkConfig, Embedder, GatewayEmbedder, Memory};
use model::{Capability, Catalog, Gateway, OpenAiClient};
use plugin_api::{Plugin, PluginManifest, Tool};
use plugin_host::{HostError, PluginHost, PluginsConfig, ProcessConfig, ServiceSpec};
use store::Store;
use tools::{HttpConfigError, HttpRequestTool, MemoryTool};
use workflow::{DELEGATE_TOOL, Engine, EngineLimits};

use crate::Config;
use crate::capability::RuntimeCapabilities;
use crate::delegate::DelegateTool;
use crate::ledger::StoreLedger;
use crate::mcp::{ChatTool, DispatchedTool};

/// What embedding is for, which decides whether it may pay.
const EMBEDDING_PURPOSE: &str = "memory";

/// The runtime couldn't be put together.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// `http_request` can't be set up.
    #[error(transparent)]
    Http(#[from] HttpConfigError),
    /// A bundled tool was refused, which is a bug.
    #[error(transparent)]
    Registry(#[from] dispatch::RegistryError),
    /// The client for model servers can't be built.
    #[error("couldn't build the model client: {0}")]
    Client(String),
    /// A setting names something that doesn't exist.
    #[error("{0}")]
    Config(String),
    /// A plugin was refused. Nothing starts with a plugin missing.
    #[error(transparent)]
    Plugin(#[from] HostError),
}

/// The plugins to install, and what the deployment grants them.
#[derive(Debug, Clone, Default)]
pub struct Plugins {
    /// The plugins compiled into this binary.
    pub compiled: Vec<Arc<dyn Plugin>>,
    /// What each may use, from `plugins.yaml`. A plugin not listed there is
    /// granted nothing, so one that asks for a capability is refused.
    pub grants: PluginsConfig,
}

/// Starts a plugin `plugins.yaml` says runs as its own process.
async fn install_process(
    host: &mut PluginHost,
    name: &str,
    process: &ProcessConfig,
) -> Result<(), RuntimeError> {
    let manifest = std::fs::read_to_string(&process.manifest).map_err(|error| {
        RuntimeError::Config(format!(
            "plugin {name}: its manifest {}: {error}",
            process.manifest
        ))
    })?;
    let declared = PluginManifest::from_yaml(&manifest).map_err(HostError::from)?;
    if declared.name != name {
        return Err(RuntimeError::Config(format!(
            "plugin {name}: its manifest names it {}",
            declared.name
        )));
    }
    let spec = ServiceSpec {
        command: process.command.clone(),
        args: process.args.clone(),
        env_allow: process.env.clone(),
        timeout_secs: process.timeout_secs,
    };
    host.install_service(&manifest, &spec).await?;
    Ok(())
}

/// A runtime ready to serve.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) sink: Arc<dyn EventSink>,
    pub(crate) config: Arc<Config>,
    pub(crate) workflows: Engine,
    /// Where definition files live, once the server has loaded them. Harness²
    /// writes agent files here, and only here.
    pub(crate) definitions: Option<Arc<PathBuf>>,
    /// Held while a change to an agent's file is made, so two never race.
    pub(crate) changing: Arc<tokio::sync::Mutex<()>>,
    /// The plugins installed, by name.
    pub(crate) plugins: Arc<Vec<String>>,
}

/// The width of the vectors, if the catalog has an embedding model.
#[must_use]
pub(crate) fn embedding_dimension(catalog: &Catalog) -> Option<usize> {
    catalog
        .models()
        .iter()
        .find(|model| model.can(Capability::Embeddings))
        .and_then(|model| model.embedding_dimension)
}

impl Runtime {
    /// Ends every run still going, at its next step or mid-call, so each
    /// finishes with its receipt. For shutdown.
    pub fn stop_runs(&self) {
        self.workflows.stop.stop();
    }

    /// Puts the parts together. `secret_of` looks up the keys configured for
    /// `http_request`.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] if a part can't be set up.
    pub fn assemble(
        store: Arc<dyn Store>,
        sink: Arc<dyn EventSink>,
        catalog: Catalog,
        config: Config,
        secret_of: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, RuntimeError> {
        let (gateway, registry, _, _) = Self::bundled(&store, catalog, &config, secret_of)?;
        Self::finish(store, sink, gateway, registry, config)
    }

    /// Puts the parts together with plugins installed. Each is checked
    /// against its manifest and granted only what `plugins.grants` allows;
    /// any refusal stops the build.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] if a part can't be set up or a plugin is refused.
    pub async fn build(
        store: Arc<dyn Store>,
        sink: Arc<dyn EventSink>,
        catalog: Catalog,
        config: Config,
        secret_of: &dyn Fn(&str) -> Option<String>,
        plugins: Plugins,
    ) -> Result<Self, RuntimeError> {
        let (gateway, registry, http, memory) = Self::bundled(&store, catalog, &config, secret_of)?;
        let source = RuntimeCapabilities {
            http,
            gateway: Arc::clone(&gateway),
            max_output_tokens: config.max_output_tokens,
            memory,
            store: Arc::clone(&store),
        };
        let processes: Vec<(String, ProcessConfig)> = plugins
            .grants
            .plugins
            .iter()
            .filter_map(|plugin| Some((plugin.name.clone(), plugin.process.clone()?)))
            .collect();
        let mut host = PluginHost::new(Arc::clone(&store), plugins.grants, registry)
            .with_capabilities(Arc::new(source));
        for plugin in &plugins.compiled {
            host.install_compiled(plugin.as_ref()).await?;
        }
        for (name, process) in processes {
            install_process(&mut host, &name, &process).await?;
        }
        let installed = host.installed();
        let mut runtime = Self::finish(store, sink, gateway, host.into_registry(), config)?;
        runtime.plugins = Arc::new(installed);
        Ok(runtime)
    }

    /// The gateway, a registry holding the two bundled tools, and the HTTP
    /// tool again, for plugins granted `http`.
    fn bundled(
        store: &Arc<dyn Store>,
        catalog: Catalog,
        config: &Config,
        secret_of: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(Arc<Gateway>, ToolRegistry, Arc<HttpRequestTool>, Memory), RuntimeError> {
        let dimension = embedding_dimension(&catalog);
        let client =
            OpenAiClient::new().map_err(|error| RuntimeError::Client(error.to_string()))?;
        let gateway = Arc::new(Gateway::new(
            catalog,
            Arc::new(client),
            Arc::new(StoreLedger(Arc::clone(store))),
        ));
        let embedder = dimension.map(|width| {
            Arc::new(GatewayEmbedder::new(
                Arc::clone(&gateway),
                width,
                EMBEDDING_PURPOSE,
            )) as Arc<dyn Embedder>
        });
        let memory = Memory::new(Arc::clone(store), embedder, ChunkConfig::default());
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(MemoryTool::new(memory.clone())))?;
        let http = Arc::new(HttpRequestTool::new(&config.http, secret_of)?);
        registry.register(Arc::clone(&http) as Arc<dyn Tool>)?;
        Ok((gateway, registry, http, memory))
    }

    /// The rest, once every tool is in the registry.
    fn finish(
        store: Arc<dyn Store>,
        sink: Arc<dyn EventSink>,
        gateway: Arc<Gateway>,
        registry: ToolRegistry,
        config: Config,
    ) -> Result<Self, RuntimeError> {
        let mut names: Vec<String> = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        if names.iter().any(|name| name == DELEGATE_TOOL) {
            return Err(RuntimeError::Config(format!(
                "a plugin's tool is named {DELEGATE_TOOL}, which the runtime keeps for itself"
            )));
        }
        names.push(DELEGATE_TOOL.to_owned());
        if let Some(missing) = config
            .harness
            .probe_reads
            .iter()
            .find(|name| !names.contains(name))
        {
            return Err(RuntimeError::Config(format!(
                "harness.probe_reads names {missing}, which isn't a tool"
            )));
        }
        let limits = EngineLimits {
            agent_budget: config.budget,
            agent_result_chars: config.max_result_chars,
            agent_output_tokens: config.max_output_tokens,
            time_limit: Duration::from_secs(u64::from(config.workflow_time_limit_seconds)),
        };
        // `delegate` runs agents through the registry it's in, so it gets the
        // registry as it's made, weakly.
        let mut refused = None;
        let stop = StopSignal::new();
        let registry = Arc::new_cyclic(|weak| {
            let mut registry = registry;
            let delegate = DelegateTool {
                registry: weak.clone(),
                gateway: Arc::clone(&gateway),
                sink: Arc::clone(&sink),
                store: Arc::clone(&store),
                limits,
                stop: stop.clone(),
                probe_reads: config.harness.probe_reads.iter().cloned().collect(),
            };
            if let Err(error) = registry.register(Arc::new(delegate)) {
                refused = Some(error);
            }
            registry
        });
        if let Some(error) = refused {
            return Err(error.into());
        }
        let workflows = Engine {
            registry: Arc::clone(&registry),
            gateway: Arc::clone(&gateway),
            sink: Arc::clone(&sink),
            store,
            limits,
            stop,
        };
        Ok(Self {
            registry,
            gateway,
            sink,
            workflows,
            config: Arc::new(config),
            definitions: None,
            changing: Arc::new(tokio::sync::Mutex::new(())),
            plugins: Arc::new(Vec::new()),
        })
    }

    /// The plugins installed, by name.
    #[must_use]
    pub fn plugins(&self) -> &[String] {
        &self.plugins
    }

    /// Says where the definition files are, so Harness² can change an agent's
    /// file. Without it, a supported edit is kept and never adopted.
    #[must_use]
    pub fn with_definitions(mut self, root: PathBuf) -> Self {
        self.definitions = Some(Arc::new(root));
        self
    }

    /// What it offers over MCP: a chat, and each tool but `delegate`. All of them
    /// go through dispatch, so a client gets no way around its checks.
    #[must_use]
    pub fn mcp_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(ChatTool::new(self.clone()))];
        // A hand-off is the front door's, reached through `chat`.
        for definition in self
            .registry
            .definitions()
            .into_iter()
            .filter(|definition| definition.name != DELEGATE_TOOL)
        {
            tools.push(Arc::new(DispatchedTool::new(self.clone(), definition)));
        }
        tools
    }
}
