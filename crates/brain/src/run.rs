//! Starting up from the environment. Only MCP goes to stdout; everything
//! else goes to stderr.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use events::EventSink;
use model::Catalog;
use store::Store;
use store_duckdb::{DuckDbConfig, DuckDbSink, DuckDbStore};

use crate::assemble::embedding_dimension;
use crate::{Brain, BrainConfig, BrainError, ConfigError};

/// The vector width of a store created with no embedding model configured.
const DEFAULT_DIMENSION: usize = 1024;

/// The brain couldn't start, or its connection failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// A file couldn't be read or created.
    #[error("{path}: {source}")]
    File {
        /// The file.
        path: String,
        /// What went wrong.
        source: std::io::Error,
    },
    /// The models file is wrong.
    #[error("models configuration: {0}")]
    Models(#[from] model::ConfigError),
    /// The configuration is wrong.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The store couldn't be opened.
    #[error(transparent)]
    Store(#[from] store_duckdb::OpenError),
    /// The event log couldn't be opened.
    #[error("the event log: {0}")]
    Events(#[from] events::EventError),
    /// The brain couldn't be put together.
    #[error(transparent)]
    Brain(#[from] BrainError),
    /// The connection failed.
    #[error("the connection failed: {0}")]
    Serve(std::io::Error),
}

fn read(path: &Path) -> Result<String, RunError> {
    std::fs::read_to_string(path).map_err(|source| RunError::File {
        path: path.display().to_string(),
        source,
    })
}

/// Where everything is, from the environment.
struct Places {
    models: PathBuf,
    config: Option<PathBuf>,
    data: PathBuf,
    extensions: PathBuf,
}

impl Places {
    fn from(variable: &dyn Fn(&str) -> Option<String>) -> Self {
        let path = |name: &str, default: &str| {
            PathBuf::from(variable(name).unwrap_or_else(|| default.to_owned()))
        };
        Self {
            models: path("MODELS", "models.yaml"),
            config: variable("CONFIG").map(PathBuf::from),
            data: path("DATA_DIR", "data"),
            extensions: path("EXTENSION_DIR", ".cache/duckdb-extensions"),
        }
    }
}

/// Serves MCP on stdin and stdout until the client hangs up. `product` is
/// the binary's name, which also prefixes every setting: `PRODUCT_MODELS`,
/// `PRODUCT_CONFIG`, `PRODUCT_DATA_DIR` and `PRODUCT_EXTENSION_DIR`.
///
/// # Errors
///
/// [`RunError`] if it can't start or the connection fails.
pub async fn run_stdio(product: &str, version: &str) -> Result<(), RunError> {
    let prefix = product.to_uppercase();
    let variable = |name: &str| {
        std::env::var(format!("{prefix}_{name}"))
            .ok()
            .filter(|value| !value.is_empty())
    };
    let places = Places::from(&variable);
    let catalog = Catalog::from_yaml(&read(&places.models)?, |name| std::env::var(name).ok())?;
    let config = match &places.config {
        Some(path) => BrainConfig::from_yaml(&read(path)?)?,
        None => BrainConfig::default(),
    };
    if catalog.models().iter().any(|model| model.is_paid()) {
        eprintln!(
            "note: a priced model is configured, and the daily spend cap is kept in memory, \
             so a restart forgets the day's spend"
        );
    }
    std::fs::create_dir_all(&places.data).map_err(|source| RunError::File {
        path: places.data.display().to_string(),
        source,
    })?;
    let store = Arc::new(DuckDbStore::open(&DuckDbConfig {
        path: Some(places.data.join("brain.duckdb")),
        extension_dir: places.extensions,
        embedding_dimension: embedding_dimension(&catalog).unwrap_or(DEFAULT_DIMENSION),
    })?);
    let sink: Arc<dyn EventSink> =
        Arc::new(DuckDbSink::open(Some(&places.data.join("events.duckdb")))?);
    let brain = Brain::assemble(
        Arc::clone(&store) as Arc<dyn Store>,
        sink,
        catalog,
        config,
        &|name| std::env::var(name).ok(),
    )?;
    let tools = brain.mcp_tools();
    drop(brain);
    let served = plugin_api::serve_stdio(product, version, tools)
        .await
        .map_err(RunError::Serve);
    match Arc::try_unwrap(store) {
        Ok(store) => store.close(),
        Err(_) => eprintln!("note: the store was still in use at shutdown"),
    }
    served
}
