//! Starting up from the environment. Only MCP goes to stdout; everything
//! else goes to stderr.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use events::EventSink;
use model::Catalog;
use plugin_api::Plugin;
use plugin_host::PluginsConfig;
use store::Store;
use store_duckdb::{DuckDbConfig, DuckDbSink, DuckDbStore};
use tokio::sync::watch;
use work::{
    DefinitionError, JobTransport, Scheduler, StoreTransport, Worker, WorkerError, WorkerOptions,
};

use crate::assemble::embedding_dimension;
use crate::http::check_owner_key;
use crate::{Config, ConfigError, Plugins, Runtime, RuntimeError};

/// The vector width of a store created with no embedding model configured.
const DEFAULT_DIMENSION: usize = 1024;
/// How often the scheduler looks for what's due. Schedules can have seconds,
/// but this is the finest it honors them.
const SCHEDULER_TICK: Duration = Duration::from_secs(5);
/// Where the HTTP server listens unless told otherwise.
const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// How long open requests get to finish once it's told to stop.
const HTTP_GRACE: Duration = Duration::from_secs(25);

/// The runtime couldn't start, or its connection failed.
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
    /// The store setting names a store this build doesn't have, or one it
    /// needs is missing.
    #[error("{variable}: {problem}")]
    StoreSetting {
        /// The variable it's read from.
        variable: String,
        /// What's wrong with it.
        problem: String,
    },
    /// A store other than the default couldn't be opened.
    #[error("the store: {0}")]
    OpenStore(store::StoreError),
    /// The event log couldn't be opened.
    #[error("the event log: {0}")]
    Events(#[from] events::EventError),
    /// The runtime couldn't be put together.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
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
    plugins: Option<PathBuf>,
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
            plugins: variable("PLUGINS").map(PathBuf::from),
        }
    }
}

/// The store that was opened, which decides what closing it takes.
enum OpenStore {
    DuckDb(Arc<DuckDbStore>),
    #[cfg(feature = "neo4j")]
    Neo4j(Arc<store_neo4j::Neo4jStore>),
}

impl OpenStore {
    fn shared(&self) -> Arc<dyn Store> {
        match self {
            Self::DuckDb(store) => Arc::clone(store) as Arc<dyn Store>,
            #[cfg(feature = "neo4j")]
            Self::Neo4j(store) => Arc::clone(store) as Arc<dyn Store>,
        }
    }

    /// The `DuckDB` writer has to finish before the process ends. Neo4j's
    /// connections close on their own.
    fn close(self) {
        match self {
            Self::DuckDb(store) => match Arc::try_unwrap(store) {
                Ok(store) => store.close(),
                Err(_) => eprintln!("note: the store was still in use at shutdown"),
            },
            #[cfg(feature = "neo4j")]
            Self::Neo4j(_) => {}
        }
    }
}

/// Opens the store the `STORE` setting names: `duckdb`, the default, or
/// `neo4j` in a build with that feature.
#[cfg_attr(
    not(feature = "neo4j"),
    expect(clippy::unused_async, reason = "only the neo4j store connects")
)]
async fn open_store(
    prefix: &str,
    variable: &dyn Fn(&str) -> Option<String>,
    places: &Places,
    dimension: usize,
) -> Result<OpenStore, RunError> {
    let setting = |problem: String| RunError::StoreSetting {
        variable: format!("{prefix}_STORE"),
        problem,
    };
    match variable("STORE").as_deref() {
        None | Some("duckdb") => Ok(OpenStore::DuckDb(Arc::new(DuckDbStore::open(
            &DuckDbConfig {
                path: Some(places.data.join("store.duckdb")),
                extension_dir: places.extensions.clone(),
                embedding_dimension: dimension,
            },
        )?))),
        #[cfg(feature = "neo4j")]
        Some("neo4j") => {
            let required = |name: &str| {
                variable(name).ok_or_else(|| RunError::StoreSetting {
                    variable: format!("{prefix}_{name}"),
                    problem: "the neo4j store needs it".to_owned(),
                })
            };
            let config = store_neo4j::Neo4jConfig {
                uri: required("NEO4J_URI")?,
                user: variable("NEO4J_USER").unwrap_or_else(|| "neo4j".to_owned()),
                password: required("NEO4J_PASSWORD")?,
                database: variable("NEO4J_DATABASE"),
                embedding_dimension: dimension,
            };
            let store = store_neo4j::Neo4jStore::connect(&config)
                .await
                .map_err(RunError::OpenStore)?;
            Ok(OpenStore::Neo4j(Arc::new(store)))
        }
        #[cfg(not(feature = "neo4j"))]
        Some("neo4j") => Err(setting(
            "this build has no neo4j store; build it with the neo4j feature".to_owned(),
        )),
        Some(other) => Err(setting(format!(
            "unknown store {other}; expected duckdb or neo4j"
        ))),
    }
}

/// Everything both ways of running share.
struct Opened {
    store: OpenStore,
    runtime: Runtime,
    workers: u32,
    definitions: PathBuf,
}

async fn open(product: &str, compiled: &[Arc<dyn Plugin>]) -> Result<Opened, RunError> {
    let prefix = product.to_uppercase();
    let variable = |name: &str| {
        std::env::var(format!("{prefix}_{name}"))
            .ok()
            .filter(|value| !value.is_empty())
    };
    let places = Places::from(&variable);
    let catalog = Catalog::from_yaml(&read(&places.models)?, |name| std::env::var(name).ok())?;
    let mut config = match &places.config {
        Some(path) => Config::from_yaml(&read(path)?)?,
        None => Config::default(),
    };
    // Unless the configuration names another, requests say which program sent them.
    if config.http.user_agent == tools::HttpConfig::default().user_agent {
        product.clone_into(&mut config.http.user_agent);
    }
    std::fs::create_dir_all(&places.data).map_err(|source| RunError::File {
        path: places.data.display().to_string(),
        source,
    })?;
    let dimension = embedding_dimension(&catalog).unwrap_or(DEFAULT_DIMENSION);
    let store = open_store(&prefix, &variable, &places, dimension).await?;
    let sink: Arc<dyn EventSink> =
        Arc::new(DuckDbSink::open(Some(&places.data.join("events.duckdb")))?);
    let workers = config.workers;
    let grants = match &places.plugins {
        Some(path) => PluginsConfig::from_yaml(&read(path)?).map_err(RuntimeError::from)?,
        None => PluginsConfig::default(),
    };
    let plugins = Plugins {
        compiled: compiled.to_vec(),
        grants,
    };
    let runtime = Runtime::build(
        store.shared(),
        sink,
        catalog,
        config,
        &|name| std::env::var(name).ok(),
        plugins,
    )
    .await?;
    Ok(Opened {
        store,
        runtime,
        workers,
        definitions: places.definitions,
    })
}

/// Serves MCP on stdin and stdout until the client hangs up. `product` is
/// the binary's name, which also prefixes every setting.
///
/// # Errors
///
/// [`RunError`] if it can't start or the connection fails.
pub async fn run_stdio(
    product: &str,
    version: &str,
    plugins: &[Arc<dyn Plugin>],
) -> Result<(), RunError> {
    let Opened { store, runtime, .. } = open(product, plugins).await?;
    let tools = runtime.mcp_tools();
    drop(runtime);
    let served = plugin_api::serve_stdio(product, version, tools)
        .await
        .map_err(RunError::Serve);
    store.close();
    served
}

/// Relays MCP on stdin and stdout to a running server, reading the server's
/// address and the owner key from the environment. It opens no store, so it
/// can run beside the server.
///
/// # Errors
///
/// [`RunError`] if the key is missing or stdin or stdout fails.
pub async fn run_connect(product: &str, url: Option<&str>) -> Result<(), RunError> {
    let prefix = product.to_uppercase();
    let owner_key = std::env::var(format!("{prefix}_OWNER_KEY")).unwrap_or_default();
    check_owner_key(&owner_key).map_err(|problem| RunError::OwnerKey {
        variable: format!("{prefix}_OWNER_KEY"),
        problem,
    })?;
    let url = url.map_or_else(|| format!("http://{DEFAULT_BIND}"), str::to_owned);
    crate::relay(&url, &owner_key, tokio::io::stdin(), tokio::io::stdout())
        .await
        .map_err(RunError::Serve)
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
pub async fn run_server(
    product: &str,
    version: &str,
    plugins: &[Arc<dyn Plugin>],
) -> Result<(), RunError> {
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
        runtime,
        workers,
        definitions,
    } = open(product, plugins).await?;
    let runtime = runtime.with_definitions(definitions.clone());
    let count = runtime.load_definitions(&definitions, Utc::now()).await?;
    let router = runtime.http_router(product, version, &owner_key)?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|source| RunError::File {
            path: bind.clone(),
            source,
        })?;
    let installed = runtime.plugins().join(", ");
    let installed = if installed.is_empty() {
        "none".to_owned()
    } else {
        installed
    };
    eprintln!(
        "{count} definitions loaded, plugins: {installed}, {workers} workers, listening on {bind}"
    );

    let transport: Arc<dyn JobTransport> = Arc::new(StoreTransport(store.shared()));
    let options = WorkerOptions {
        concurrency: workers as usize,
        ..WorkerOptions::default()
    };
    let worker = Worker::new(
        Arc::clone(&transport),
        runtime.job_handlers(),
        options,
        format!("{product}-{}", std::process::id()),
    )
    .map(Arc::new)?;
    let scheduler = Scheduler::new(store.shared(), transport);
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
    drop(runtime);
    store.close();
    served.map_err(RunError::Serve)
}
