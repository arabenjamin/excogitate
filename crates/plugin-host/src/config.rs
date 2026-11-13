//! The deployment's `plugins.yaml`: what each plugin is granted.

use plugin_api::PluginCapability;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::HostError;

/// The contents of `plugins.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// The format version.
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    /// The plugins given anything. A plugin not listed is granted nothing.
    #[schemars(length(max = 128))]
    pub plugins: Vec<PluginConfig>,
}

/// What one plugin is granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    /// The plugin's name.
    #[schemars(length(min = 1, max = 32))]
    pub name: String,
    /// The capabilities it may use.
    #[schemars(length(max = 8))]
    pub capabilities: Vec<PluginCapability>,
    /// How to start it as its own process. Left out, it's a plugin compiled
    /// into the binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessConfig>,
    /// How long one search of one of its sources may take, in seconds,
    /// inside the memory call's own limit. Over it, memory goes on
    /// without that source and says so.
    #[serde(default = "default_source_time_limit")]
    #[schemars(range(min = 1, max = 60))]
    pub source_time_limit_secs: u32,
    /// What a model must be able to do, beyond chat, to answer this
    /// plugin's `model` calls, such as `investigative`: the deployment's
    /// say, never the plugin's. Left out, the cheapest that can chat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 4))]
    pub model_needs: Vec<model::Capability>,
}

const fn default_source_time_limit() -> u32 {
    5
}

/// A plugin that runs as its own process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// The program to run.
    #[schemars(length(min = 1, max = 1024))]
    pub command: String,
    /// Its arguments.
    #[serde(default)]
    #[schemars(length(max = 32), inner(length(max = 1024)))]
    pub args: Vec<String>,
    /// Its manifest file. Its name must be this plugin's.
    #[schemars(length(min = 1, max = 1024))]
    pub manifest: String,
    /// The environment variables it may see. Its environment is otherwise
    /// empty.
    #[serde(default)]
    #[schemars(length(max = 32), inner(length(min = 1, max = 128)))]
    pub env: Vec<String>,
    /// How long one call may take, in seconds.
    #[serde(default = "default_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    pub timeout_secs: u32,
}

const fn default_timeout() -> u32 {
    30
}

impl Default for PluginsConfig {
    /// Grants nothing: what a deployment without `plugins.yaml` has.
    fn default() -> Self {
        Self {
            version: 1,
            plugins: Vec::new(),
        }
    }
}

impl PluginsConfig {
    /// Reads and checks `plugins.yaml`.
    ///
    /// # Errors
    ///
    /// [`HostError::Config`] if it isn't YAML or fails its schema.
    pub fn from_yaml(text: &str) -> Result<Self, HostError> {
        schema::parse_yaml(text).map_err(|error| HostError::Config(error.to_string()))
    }

    /// How long one search of the plugin's sources may take.
    pub(crate) fn source_time_limit(&self, plugin: &str) -> std::time::Duration {
        let seconds = self
            .plugins
            .iter()
            .find(|config| config.name == plugin)
            .map_or(default_source_time_limit(), |config| {
                config.source_time_limit_secs
            });
        std::time::Duration::from_secs(u64::from(seconds))
    }

    pub(crate) fn granted(&self, plugin: &str, capability: PluginCapability) -> bool {
        self.plugins
            .iter()
            .any(|config| config.name == plugin && config.capabilities.contains(&capability))
    }
}
