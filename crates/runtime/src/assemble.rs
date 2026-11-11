//! Builds the runtime from its parts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dispatch::ToolRegistry;
use events::EventSink;
use memory::{ChunkConfig, Embedder, GatewayEmbedder, Memory};
use model::{Capability, Catalog, Gateway, OpenAiClient};
use plugin_api::{Plugin, Tool};
use plugin_host::{HostError, PluginHost, PluginsConfig};
use store::Store;
use tools::{HttpConfigError, HttpRequestTool, MemoryTool};
use workflow::{Engine, EngineLimits};

use crate::Config;
use crate::capability::RuntimeCapabilities;
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
        let (gateway, registry, _) = Self::bundled(&store, catalog, &config, secret_of)?;
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
        let (gateway, registry, http) = Self::bundled(&store, catalog, &config, secret_of)?;
        let source = RuntimeCapabilities {
            http,
            gateway: Arc::clone(&gateway),
            max_output_tokens: config.max_output_tokens,
        };
        let mut host = PluginHost::new(Arc::clone(&store), plugins.grants, registry)
            .with_capabilities(Arc::new(source));
        for plugin in &plugins.compiled {
            host.install_compiled(plugin.as_ref()).await?;
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
    ) -> Result<(Arc<Gateway>, ToolRegistry, Arc<HttpRequestTool>), RuntimeError> {
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
        registry.register(Arc::new(MemoryTool::new(memory)))?;
        let http = Arc::new(HttpRequestTool::new(&config.http, secret_of)?);
        registry.register(Arc::clone(&http) as Arc<dyn Tool>)?;
        Ok((gateway, registry, http))
    }

    /// The rest, once every tool is in the registry.
    fn finish(
        store: Arc<dyn Store>,
        sink: Arc<dyn EventSink>,
        gateway: Arc<Gateway>,
        registry: ToolRegistry,
        config: Config,
    ) -> Result<Self, RuntimeError> {
        let names: Vec<String> = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
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
        let registry = Arc::new(registry);
        let workflows = Engine {
            registry: Arc::clone(&registry),
            gateway: Arc::clone(&gateway),
            sink: Arc::clone(&sink),
            store,
            limits: EngineLimits {
                agent_budget: config.budget,
                agent_result_chars: config.max_result_chars,
                agent_output_tokens: config.max_output_tokens,
                time_limit: Duration::from_secs(u64::from(config.workflow_time_limit_seconds)),
            },
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

    /// What it offers over MCP: a chat, and each bundled tool. All of them
    /// go through dispatch, so a client gets no way around its checks.
    #[must_use]
    pub fn mcp_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(ChatTool::new(self.clone()))];
        for definition in self.registry.definitions() {
            tools.push(Arc::new(DispatchedTool::new(self.clone(), definition)));
        }
        tools
    }
}
