//! The deployment's `plugins.yaml`: what each plugin is granted.

use plugin_api::PluginCapability;
use schema::{Validator, bounded};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
        let raw: Value = serde_saphyr::from_str(text)
            .map_err(|error| HostError::Config(bounded(&error.to_string())))?;
        Validator::for_type::<Self>()
            .map_err(|error| HostError::Config(error.to_string()))?
            .parse(raw)
            .map_err(|refusal| HostError::Config(refusal.to_string()))
    }

    pub(crate) fn granted(&self, plugin: &str, capability: PluginCapability) -> bool {
        self.plugins
            .iter()
            .any(|config| config.name == plugin && config.capabilities.contains(&capability))
    }
}
