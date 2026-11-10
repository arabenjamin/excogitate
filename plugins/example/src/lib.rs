//! The example plugin, and the starting point for a new one. The same tool
//! code runs compiled in ([`ExamplePlugin`]) and as its own process
//! (`src/main.rs`).

use std::sync::Arc;

use async_trait::async_trait;
use plugin_api::{
    Capabilities, Plugin, PluginError, PluginManifest, RecordsCapability, Tool, ToolContext,
    ToolDefinition, ToolFailure, ToolOutput,
};
use protocol::Trust;
use serde_json::{Value, json};

/// The manifest used when compiled in.
pub const MANIFEST: &str = include_str!("../plugin.yaml");
/// The manifest used when run as a service.
pub const SERVICE_MANIFEST: &str = include_str!("../plugin.service.yaml");

const BOOKMARK: &str = "example.bookmark";

/// The definition of a tool, taken from the manifest so the two can't differ.
fn definition_from(manifest: &str, name: &str) -> Result<ToolDefinition, PluginError> {
    PluginManifest::from_yaml(manifest)
        .map_err(|error| PluginError::Other(error.to_string()))?
        .definitions()
        .into_iter()
        .find(|definition| definition.name == name)
        .ok_or_else(|| PluginError::Other(format!("{name} isn't in the manifest")))
}

/// Counts words. It needs nothing from the brain.
#[derive(Debug)]
pub struct WordCount(ToolDefinition);

impl WordCount {
    /// The tool, as declared by `manifest`.
    ///
    /// # Errors
    ///
    /// [`PluginError`] if the manifest doesn't declare it.
    pub fn from_manifest(manifest: &str) -> Result<Self, PluginError> {
        definition_from(manifest, "example_word_count").map(Self)
    }
}

#[async_trait]
impl Tool for WordCount {
    fn definition(&self) -> ToolDefinition {
        self.0.clone()
    }

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let text = arguments.get("text").and_then(Value::as_str).unwrap_or("");
        Ok(ToolOutput {
            value: json!({ "words": text.split_whitespace().count() }),
            trust: Trust::Trusted,
        })
    }
}

/// Saves a bookmark, through the records capability.
#[derive(Debug)]
struct BookmarkAdd {
    definition: ToolDefinition,
    records: Arc<dyn RecordsCapability>,
}

#[async_trait]
impl Tool for BookmarkAdd {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let id = self
            .records
            .put(BOOKMARK, None, arguments, context)
            .await
            .map_err(|error| ToolFailure(error.to_string()))?;
        Ok(ToolOutput {
            value: json!({ "id": id.to_string() }),
            trust: Trust::Trusted,
        })
    }
}

/// The compiled-in form.
#[derive(Debug)]
pub struct ExamplePlugin;

impl Plugin for ExamplePlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        Ok(vec![
            Arc::new(WordCount::from_manifest(MANIFEST)?),
            Arc::new(BookmarkAdd {
                definition: definition_from(MANIFEST, "example_bookmark_add")?,
                records: capabilities.records()?,
            }),
        ])
    }
}
