//! Tools from remote MCP servers, offered as the runtime's own.
//!
//! The owner names each server and the tools taken from it
//! (`mcp_servers` in the config). At startup each is listed: a tool named
//! is registered as `<server>_<tool>`, with its input schema made strict
//! ([`schema::tighten`]) and the effect the owner gave it, and what the
//! server offers beyond those is never reachable. Every call goes through
//! dispatch like any tool's, over the same address guard as
//! `http_request`, with the key the runtime holds, and comes back
//! untrusted.
//!
//! - [`config`]: the settings.
//! - [`transport`]: Streamable HTTP, sessions included.
//! - [`listing`]: each server's last good listing, so one that's down at
//!   startup still has its tools, which fail naming it until it's back.
//! - [`tool`]: the tool a model calls.

pub mod config;
pub mod listing;
pub mod schema;
pub mod tool;
pub mod transport;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use dispatch::{Effect, ToolDefinition, ToolRegistry};
use serde_json::Value;
use store::Store;
use tools::{AddressGuard, HttpConfig};

pub use config::{McpServerConfig, local_name};
pub use listing::{LISTING, Listed, Listing, Listings};
pub use tool::{RemoteTool, Server};
pub use transport::{Connection, Key, McpError};

/// The longest description a remote tool keeps.
const MAX_DESCRIPTION_CHARS: usize = 1900;

/// The remote servers couldn't be set up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RemoteError(pub String);

/// What was installed from one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The server.
    pub server: String,
    /// The tools registered, by the names they're offered under.
    pub tools: Vec<String>,
    /// Why the server couldn't be listed, when its kept listing was used.
    pub from_kept: Option<String>,
}

fn definition(config: &McpServerConfig, listed: &Listed) -> Result<ToolDefinition, RemoteError> {
    let name = local_name(&config.name, &listed.name);
    let input = schema::tighten(&listed.input_schema).map_err(|why| {
        RemoteError(format!(
            "MCP server {}: the tool {}: {why}; leave it out of tools",
            config.name, listed.name
        ))
    })?;
    let mut description: String = listed
        .description
        .chars()
        .take(MAX_DESCRIPTION_CHARS)
        .collect();
    if description.trim().is_empty() {
        description = format!("{} on the MCP server {}.", listed.name, config.name);
    } else {
        let _ = write!(description, " (From the MCP server {}.)", config.name);
    }
    let effect = if config.read_only.contains(&listed.name) {
        Effect::Read
    } else {
        Effect::Write
    };
    Ok(ToolDefinition::new(
        name,
        description,
        input,
        schema::output_schema(),
        effect,
    ))
}

/// The tools named in `config`, from what the server listed.
fn chosen(config: &McpServerConfig, offered: &[Value]) -> Result<Vec<Listed>, RemoteError> {
    let by_name: BTreeMap<&str, &Value> = offered
        .iter()
        .filter_map(|tool| Some((tool.get("name")?.as_str()?, tool)))
        .collect();
    config
        .tools
        .iter()
        .map(|wanted| {
            let tool = by_name.get(wanted.as_str()).ok_or_else(|| {
                let names: Vec<&str> = by_name.keys().take(40).copied().collect();
                RemoteError(format!(
                    "MCP server {}: it doesn't offer {wanted}; it offers {}",
                    config.name,
                    names.join(", ")
                ))
            })?;
            Ok(Listed {
                name: wanted.clone(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(4096)
                    .collect(),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            })
        })
        .collect()
}

async fn install_one(
    registry: &mut ToolRegistry,
    listings: &Listings,
    client: &reqwest::Client,
    guard: &AddressGuard,
    config: &McpServerConfig,
    secret_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Installed, RemoteError> {
    let url = config.check().map_err(RemoteError)?;
    guard
        .check(&url)
        .map_err(|why| RemoteError(format!("MCP server {}: {why}", config.name)))?;
    let key = match (&config.header, &config.env) {
        (Some(header), Some(env)) => {
            let secret = secret_of(env)
                .filter(|secret| !secret.is_empty())
                .ok_or_else(|| {
                    RemoteError(format!("MCP server {}: {env} isn't set", config.name))
                })?;
            Some(
                Key::new(header, &config.prefix, secret)
                    .map_err(|why| RemoteError(format!("MCP server {}: {why}", config.name)))?,
            )
        }
        _ => None,
    };
    let connection = Connection::new(
        client.clone(),
        url,
        key,
        Duration::from_secs(u64::from(config.timeout_seconds)),
    );
    let (listed, from_kept) = match connection.list_tools().await {
        Ok(offered) => {
            let listed = chosen(config, &offered)?;
            listings
                .put(&Listing {
                    server: config.name.clone(),
                    listed_at: Utc::now(),
                    tools: listed.clone(),
                })
                .await
                .map_err(RemoteError)?;
            (listed, None)
        }
        Err(error) => {
            let why = format!("{error}");
            let kept = listings.get(&config.name).await.map_err(RemoteError)?;
            let Some(kept) = kept else {
                return Err(RemoteError(format!(
                    "MCP server {} couldn't be listed, and no listing of it is kept: {why}",
                    config.name
                )));
            };
            let missing: Vec<&String> = config
                .tools
                .iter()
                .filter(|wanted| !kept.tools.iter().any(|tool| &tool.name == *wanted))
                .collect();
            if let Some(wanted) = missing.first() {
                return Err(RemoteError(format!(
                    "MCP server {} couldn't be listed ({why}), and its kept listing has no {wanted}",
                    config.name
                )));
            }
            let listed = kept
                .tools
                .into_iter()
                .filter(|tool| config.tools.contains(&tool.name))
                .collect();
            (
                listed,
                Some(format!(
                    "{why}; using its listing of {}",
                    kept.listed_at.to_rfc3339()
                )),
            )
        }
    };
    let server = Arc::new(Server {
        name: config.name.clone(),
        connection,
        no_log: config.no_log,
        max_result_chars: config.max_result_chars,
    });
    let mut names = Vec::new();
    for tool in &listed {
        let definition = definition(config, tool)?;
        names.push(definition.name.clone());
        registry
            .register(Arc::new(RemoteTool::new(
                Arc::clone(&server),
                tool.name.clone(),
                definition,
            )))
            .map_err(|error| RemoteError(format!("MCP server {}: {error}", config.name)))?;
    }
    Ok(Installed {
        server: config.name.clone(),
        tools: names,
        from_kept,
    })
}

/// Lists each server and registers the tools taken from it. A server that
/// can't be listed has its kept listing used; one with none stops startup,
/// as does any setting that's wrong.
///
/// # Errors
///
/// [`RemoteError`] naming the server and what's wrong.
pub async fn install(
    registry: &mut ToolRegistry,
    store: Arc<dyn Store>,
    http: &HttpConfig,
    servers: &[McpServerConfig],
    secret_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Installed>, RemoteError> {
    if servers.is_empty() {
        return Ok(Vec::new());
    }
    let mut names = std::collections::BTreeSet::new();
    if let Some(twice) = servers.iter().find(|server| !names.insert(&server.name)) {
        return Err(RemoteError(format!(
            "MCP server {} is listed twice",
            twice.name
        )));
    }
    let guard = AddressGuard::new(http).map_err(|error| RemoteError(error.to_string()))?;
    let client = guard
        .client(&http.user_agent)
        .map_err(|error| RemoteError(error.to_string()))?;
    let listings = Listings::open(store).await.map_err(RemoteError)?;
    let mut installed = Vec::new();
    for config in servers {
        installed.push(install_one(registry, &listings, &client, &guard, config, secret_of).await?);
    }
    Ok(installed)
}
