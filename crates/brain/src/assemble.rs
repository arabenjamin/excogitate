//! Builds the brain from its parts.

use std::sync::Arc;

use dispatch::ToolRegistry;
use events::EventSink;
use memory::{ChunkConfig, Embedder, GatewayEmbedder, Memory};
use model::{Capability, Catalog, Gateway, OpenAiClient};
use plugin_api::Tool;
use store::Store;
use tools::{HttpConfigError, HttpRequestTool, MemoryTool};

use crate::BrainConfig;
use crate::ledger::StoreLedger;
use crate::mcp::{ChatTool, DispatchedTool};

/// What embedding is for, which decides whether it may pay.
const EMBEDDING_PURPOSE: &str = "memory";

/// The brain couldn't be put together.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BrainError {
    /// `http_request` can't be set up.
    #[error(transparent)]
    Http(#[from] HttpConfigError),
    /// A bundled tool was refused, which is a bug.
    #[error(transparent)]
    Registry(#[from] dispatch::RegistryError),
    /// The client for model servers can't be built.
    #[error("couldn't build the model client: {0}")]
    Client(String),
}

/// A brain ready to serve.
#[derive(Debug, Clone)]
pub struct Brain {
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) sink: Arc<dyn EventSink>,
    pub(crate) config: Arc<BrainConfig>,
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

impl Brain {
    /// Puts the parts together. `secret_of` looks up the keys configured for
    /// `http_request`.
    ///
    /// # Errors
    ///
    /// [`BrainError`] if a part can't be set up.
    pub fn assemble(
        store: Arc<dyn Store>,
        sink: Arc<dyn EventSink>,
        catalog: Catalog,
        config: BrainConfig,
        secret_of: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, BrainError> {
        let dimension = embedding_dimension(&catalog);
        let client = OpenAiClient::new().map_err(|error| BrainError::Client(error.to_string()))?;
        let gateway = Arc::new(Gateway::new(
            catalog,
            Arc::new(client),
            Arc::new(StoreLedger(Arc::clone(&store))),
        ));
        let embedder = dimension.map(|width| {
            Arc::new(GatewayEmbedder::new(
                Arc::clone(&gateway),
                width,
                EMBEDDING_PURPOSE,
            )) as Arc<dyn Embedder>
        });
        let memory = Memory::new(store, embedder, ChunkConfig::default());
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(MemoryTool::new(memory)))?;
        registry.register(Arc::new(HttpRequestTool::new(&config.http, secret_of)?))?;
        Ok(Self {
            registry: Arc::new(registry),
            gateway,
            sink,
            config: Arc::new(config),
        })
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
