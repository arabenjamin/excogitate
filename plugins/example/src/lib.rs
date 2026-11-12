//! The example plugin, and the starting point for a new one. The same plugin
//! runs compiled in ([`ExamplePlugin`]) and as its own process
//! (`src/main.rs`), with one manifest and the same capabilities either way.

use std::sync::Arc;

use async_trait::async_trait;
use plugin_api::{
    Capabilities, DataClass, Plugin, PluginError, PluginManifest, RecordsCapability, Source,
    SourceFound, SourceGot, SourceItem, SourceQuery, Tool, ToolContext, ToolDefinition,
    ToolFailure, ToolOutput, Trust,
};

use serde_json::{Value, json};

/// The manifest, which both ways of running use.
pub const MANIFEST: &str = include_str!("../plugin.yaml");

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

/// Counts words. It needs nothing from the runtime.
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
        Ok(ToolOutput::new(
            json!({ "words": text.split_whitespace().count() }),
            Trust::Trusted,
        ))
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
            .map_err(ToolFailure::from)?;
        Ok(ToolOutput::new(
            json!({ "id": id.to_string() }),
            Trust::Trusted,
        ))
    }
}

/// The glossary: a source small enough to read in full. A real source would
/// search a lake; this one ranks its entries by the query's words they hold.
const GLOSSARY: [(&str, &str, &str); 4] = [
    (
        "receipt",
        "Receipt",
        "What a run did and spent: its tool calls, model calls, cost and outcome.",
    ),
    (
        "trajectory",
        "Trajectory",
        "Every step of a run, in order, as Harness² reads it to improve an agent.",
    ),
    (
        "capability",
        "Capability",
        "Something a plugin asks the runtime for, such as records or HTTP, granted by name.",
    ),
    (
        "source",
        "Source",
        "Something a plugin offers memory to read, searched beside memory's own notes.",
    ),
];

/// The glossary as a source.
#[derive(Debug)]
pub struct Glossary;

fn entry((id, title, text): (&str, &str, &str)) -> SourceItem {
    SourceItem::new(id, title, text).with_data_class(DataClass::Public)
}

#[async_trait]
impl Source for Glossary {
    fn name(&self) -> &'static str {
        "glossary"
    }

    async fn search(&self, query: SourceQuery, _: ToolContext) -> Result<SourceFound, ToolFailure> {
        let words: Vec<String> = query
            .text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| word.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect();
        let mut ranked: Vec<(usize, SourceItem)> = GLOSSARY
            .into_iter()
            .map(|found| {
                let text = format!("{} {}", found.1, found.2).to_lowercase();
                let shared = words.iter().filter(|word| text.contains(*word)).count();
                (shared, entry(found))
            })
            .filter(|(shared, _)| *shared > 0)
            .collect();
        ranked.sort_by(|(a, _), (b, _)| b.cmp(a));
        let most = usize::try_from(query.limit).unwrap_or(usize::MAX);
        Ok(SourceFound::new(
            ranked
                .into_iter()
                .take(most)
                .map(|(_, item)| item)
                .collect(),
        ))
    }

    async fn get(&self, id: &str, _: ToolContext) -> Result<SourceGot, ToolFailure> {
        let found = GLOSSARY.into_iter().find(|(own, ..)| *own == id);
        Ok(SourceGot::new(found.map(entry)))
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

    fn sources(&self, _: &Capabilities) -> Result<Vec<Arc<dyn Source>>, PluginError> {
        Ok(vec![Arc::new(Glossary)])
    }
}
