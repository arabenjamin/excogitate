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
use crate::http::check_owner_key;
use crate::{Brain, BrainConfig, BrainError, ConfigError};

/// The vector width of a store created with no embedding model configured.
const DEFAULT_DIMENSION: usize = 1024;
/// How often the scheduler looks for what's due. Schedules can have seconds,
/// but this is the finest it honors them.
const SCHEDULER_TICK: Duration = Duration::from_secs(5);
/// Where the HTTP server listens unless told otherwise.
const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// How long open requests get to finish once it's told to stop.
const HTTP_GRACE: Duration = Duration::from_secs(25);

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
    /// The owner key is missing or too short.
    #[error("{variable}: {problem}")]
    OwnerKey {
        /// The variable it's read from.
        variable: String,
        /// What's wrong with it.
        problem: String,
    },
    /// The HTTP server can't be set up.
    #[error(transparent)]
    Http(#[from] crate::http::HttpError),
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

/// Serves HTTP and runs the scheduler and workers, until the process is told
/// to stop. The store takes one process, so they all share this one.
///
/// # Errors
///
/// [`RunError`] if it can't start, or a definition file is wrong.
pub async fn run_server(product: &str, version: &str) -> Result<(), RunError> {
    let prefix = product.to_uppercase();
    let owner_key = std::env::var(format!("{prefix}_OWNER_KEY")).unwrap_or_default();
    check_owner_key(&owner_key).map_err(|problem| RunError::OwnerKey {
        variable: format!("{prefix}_OWNER_KEY"),
        problem,
    })?;
    let bind = std::env::var(format!("{prefix}_BIND"))
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let Opened {
        store,
        brain,
        workers,
        definitions,
    } = open(product)?;
    let count = brain.load_definitions(&definitions, Utc::now()).await?;
    let router = brain.http_router(product, version, &owner_key)?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|source| RunError::File {
            path: bind.clone(),
            source,
        })?;
    eprintln!("{count} definitions loaded, {workers} workers, listening on {bind}");

    let transport: Arc<dyn JobTransport> =
        Arc::new(StoreTransport(Arc::clone(&store) as Arc<dyn Store>));
    let options = WorkerOptions {
        concurrency: workers as usize,
        ..WorkerOptions::default()
    };
    let worker = Worker::new(
        Arc::clone(&transport),
        brain.job_handlers(),
        options,
        format!("{product}-{}", std::process::id()),
    )
    .map(Arc::new)?;
    let scheduler = Scheduler::new(Arc::clone(&store) as Arc<dyn Store>, transport);
    let (stop, shutdown) = watch::channel(false);
    let mut http_stop = shutdown.clone();
    let mut grace_stop = shutdown.clone();
    let serving = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = http_stop.wait_for(|stopping| *stopping).await;
        })
        .into_future();
    let (served, (), (), ()) = tokio::join!(
        async move {
            // A client that won't hang up can't hold the process open.
            let grace = async {
                let _ = grace_stop.wait_for(|stopping| *stopping).await;
                tokio::time::sleep(HTTP_GRACE).await;
            };
            tokio::select! {
                served = serving => served,
                () = grace => Ok(()),
            }
        },
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
    served.map_err(RunError::Serve)
}
