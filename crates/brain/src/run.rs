//! Starting up from the environment. Only MCP goes to stdout; everything
//! else goes to stderr.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use events::EventSink;
use model::Catalog;
use store::Store;
use store_duckdb::{DuckDbConfig, DuckDbSink, DuckDbStore};
use tokio::sync::watch;
use work::{
    DefinitionError, JobTransport, Scheduler, StoreTransport, Worker, WorkerError, WorkerOptions,
};

use crate::assemble::embedding_dimension;
use crate::definitions::load_definitions;
use crate::{Brain, BrainConfig, BrainError, ConfigError};

/// The vector width of a store created with no embedding model configured.
const DEFAULT_DIMENSION: usize = 1024;
/// How often the scheduler looks for what's due. Schedules can have seconds,
/// but this is the finest it honors them.
const SCHEDULER_TICK: Duration = Duration::from_secs(5);

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
    /// An agent, workflow or schedule can't be used.
    #[error(transparent)]
    Definition(#[from] DefinitionError),
    /// The workers can't be set up.
    #[error(transparent)]
    Worker(#[from] WorkerError),
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
    definitions: PathBuf,
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
            definitions: path("DEFINITIONS", "definitions"),
        }
    }
}

/// Everything both ways of running share.
struct Opened {
    store: Arc<DuckDbStore>,
    brain: Brain,
    workers: u32,
    definitions: PathBuf,
}

fn open(product: &str) -> Result<Opened, RunError> {
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
    let workers = config.workers;
    let brain = Brain::assemble(
        Arc::clone(&store) as Arc<dyn Store>,
        sink,
        catalog,
        config,
        &|name| std::env::var(name).ok(),
    )?;
    Ok(Opened {
        store,
        brain,
        workers,
        definitions: places.definitions,
    })
}

fn close(store: Arc<DuckDbStore>) {
    match Arc::try_unwrap(store) {
        Ok(store) => store.close(),
        Err(_) => eprintln!("note: the store was still in use at shutdown"),
    }
}

/// Serves MCP on stdin and stdout until the client hangs up. `product` is
/// the binary's name, which also prefixes every setting.
///
/// # Errors
///
/// [`RunError`] if it can't start or the connection fails.
pub async fn run_stdio(product: &str, version: &str) -> Result<(), RunError> {
    let Opened { store, brain, .. } = open(product)?;
    let tools = brain.mcp_tools();
    drop(brain);
    let served = plugin_api::serve_stdio(product, version, tools)
        .await
        .map_err(RunError::Serve);
    close(store);
    served
}

/// Resolves when the process is told to stop.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Runs the scheduler and workers until the process is told to stop.
///
/// # Errors
///
/// [`RunError`] if it can't start, or a definition file is wrong.
pub async fn run_worker(product: &str) -> Result<(), RunError> {
    let Opened {
        store,
        brain,
        workers,
        definitions,
    } = open(product)?;
    let handlers = brain.job_handlers();
    let count = load_definitions(store.as_ref(), &definitions, &brain, Utc::now()).await?;
    eprintln!("{count} definitions loaded, {workers} workers");

    let transport: Arc<dyn JobTransport> =
        Arc::new(StoreTransport(Arc::clone(&store) as Arc<dyn Store>));
    let options = WorkerOptions {
        concurrency: workers as usize,
        ..WorkerOptions::default()
    };
    let worker = Worker::new(
        Arc::clone(&transport),
        handlers,
        options,
        format!("{product}-{}", std::process::id()),
    )
    .map(Arc::new)?;
    let scheduler = Scheduler::new(Arc::clone(&store) as Arc<dyn Store>, transport);
    let (stop, shutdown) = watch::channel(false);
    tokio::join!(
        worker.run(shutdown.clone()),
        scheduler.run(SCHEDULER_TICK, shutdown),
        async {
            stop_signal().await;
            eprintln!("stopping");
            // Nobody can be left waiting on a channel nothing listens to.
            let _ = stop.send(true);
        },
    );
    drop(brain);
    close(store);
    Ok(())
}
