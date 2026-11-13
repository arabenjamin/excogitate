//! The `newsbrief` plugin: what it does is its manifest's description.
//!
//! The brief's sections are `newsbrief.section` records, one per section,
//! managed with `newsbrief_section_set` and `newsbrief_section_remove` and
//! read in order by `newsbrief_sections`, the workflow's first step. The
//! workflow, its writer agent and a schedule are files in `definitions/`:
//! a plugin offers tools, and definitions say how they're used.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use plugin_api::{
    Capabilities, FieldOp, FieldValue, PageRequest, Plugin, PluginError, PluginManifest, Record,
    RecordQuery, RecordsCapability, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput,
    Trust,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The manifest.
pub const MANIFEST: &str = include_str!("../plugin.yaml");

const SECTION: &str = "newsbrief.section";
/// The most sections a brief has.
const MAX_SECTIONS: u32 = 50;

/// The compiled-in plugin.
#[derive(Debug)]
pub struct NewsbriefPlugin;

impl Plugin for NewsbriefPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        let records = capabilities.records()?;
        Ok(PluginManifest::from_yaml(MANIFEST)
            .map_err(|error| PluginError::Other(error.to_string()))?
            .definitions()
            .into_iter()
            .map(|definition| {
                Arc::new(NewsbriefTool {
                    definition,
                    records: Arc::clone(&records),
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

/// One section of the brief, as its record keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    name: String,
    query: String,
    #[serde(default)]
    sites: Vec<String>,
    #[serde(default)]
    order: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}

/// Every tool the manifest declares; `run` goes by its name.
#[derive(Debug)]
struct NewsbriefTool {
    definition: ToolDefinition,
    records: Arc<dyn RecordsCapability>,
}

#[async_trait]
impl Tool for NewsbriefTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let value = match self.definition.name.as_str() {
            "newsbrief_sections" => self.sections(context).await?,
            "newsbrief_section_set" => self.set(arguments_of(arguments)?, context).await?,
            "newsbrief_section_remove" => self.remove(arguments_of(arguments)?, context).await?,
            other => return Err(ToolFailure::new(format!("{other} isn't a newsbrief tool"))),
        };
        // The sections are the owner's own words, but a model may have set
        // them: never instructions.
        Ok(ToolOutput::new(value, Trust::Untrusted))
    }
}

fn arguments_of<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, ToolFailure> {
    serde_json::from_value(arguments).map_err(|error| ToolFailure::new(error.to_string()))
}

fn section_of(record: &Record) -> Result<Section, ToolFailure> {
    serde_json::from_value(record.data.clone())
        .map_err(|error| ToolFailure::new(format!("a stored section can't be read: {error}")))
}

fn data_of(section: &Section) -> Result<Value, ToolFailure> {
    serde_json::to_value(section).map_err(|error| ToolFailure::new(error.to_string()))
}

impl NewsbriefTool {
    /// The section records with this name: one at most, unless two
    /// writes raced.
    async fn named(&self, name: &str, context: ToolContext) -> Result<Vec<Record>, ToolFailure> {
        let page = PageRequest::new(MAX_SECTIONS, 0)
            .map_err(|error| ToolFailure::new(error.to_string()))?;
        let query = RecordQuery::new(SECTION, page).with_filter(
            "name",
            FieldOp::Eq,
            FieldValue::Text(name.to_owned()),
        );
        Ok(self.records.query(query, context).await?.items)
    }

    async fn sections(&self, context: ToolContext) -> Result<Value, ToolFailure> {
        let page = PageRequest::new(MAX_SECTIONS, 0)
            .map_err(|error| ToolFailure::new(error.to_string()))?;
        let found = self
            .records
            .query(RecordQuery::new(SECTION, page), context)
            .await?;
        let mut sections = found
            .items
            .iter()
            .map(section_of)
            .collect::<Result<Vec<_>, _>>()?;
        if sections.is_empty() {
            return Err(ToolFailure::new(
                "the brief has no sections yet: add one with newsbrief_section_set",
            ));
        }
        sections.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
        Ok(json!({
            "date": Utc::now().date_naive().to_string(),
            "sections": sections,
        }))
    }

    async fn set(&self, section: Section, context: ToolContext) -> Result<Value, ToolFailure> {
        let section = Section {
            name: section.name.trim().to_owned(),
            query: section.query.trim().to_owned(),
            ..section
        };
        let existing = self.named(&section.name, context).await?;
        // A duplicate left by a race goes; the first is replaced in place.
        for extra in existing.iter().skip(1) {
            self.records.delete(SECTION, extra.id, context).await?;
        }
        let replacing = existing.first().map(|record| record.id);
        self.records
            .put(SECTION, replacing, data_of(&section)?, context)
            .await?;
        let replaced = replacing.is_some();
        Ok(json!({ "section": section, "replaced": replaced }))
    }

    async fn remove(&self, args: NameArgs, context: ToolContext) -> Result<Value, ToolFailure> {
        let existing = self.named(args.name.trim(), context).await?;
        for record in &existing {
            self.records.delete(SECTION, record.id, context).await?;
        }
        Ok(json!({ "removed": !existing.is_empty() }))
    }
}
