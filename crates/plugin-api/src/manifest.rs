//! The plugin manifest, and everything checked about it before a plugin can
//! do anything.

use std::collections::HashSet;

use schema::{Refusal, YamlError, check_strict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::RecordDecls;

use crate::{Effect, ToolDefinition};

/// The version of this contract.
pub const API_VERSION: u32 = 1;

/// Names a plugin can't take: the core keeps records under them, or its own
/// tools start with them, or they'd read as the core's.
pub const RESERVED_NAMES: [&str; 12] = [
    "harness", "http", "memory", "delegate", "chat", "runtime", "core", "plugin", "plugins",
    "store", "system", "tool",
];

/// A plugin's `plugin.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PluginManifest {
    /// The contract version it's written against.
    #[schemars(range(min = 1, max = 1))]
    pub api_version: u32,
    /// Its name, which prefixes its tools and record types.
    #[schemars(length(min = 1, max = 32), regex(pattern = r"^[a-z][a-z0-9]*$"))]
    pub name: String,
    /// Its version.
    #[schemars(length(min = 1, max = 32))]
    pub version: String,
    /// What it's for.
    #[schemars(length(max = 1024))]
    pub description: String,
    /// Its tools.
    #[schemars(length(max = 64))]
    pub tools: Vec<ToolManifest>,
    /// What it needs from the runtime.
    #[serde(default)]
    #[schemars(length(max = 8))]
    pub capabilities: Vec<PluginCapability>,
    /// The record types and relations it keeps.
    pub records: Option<RecordDecls>,
    /// What it offers memory to read. Needs the `sources` capability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8))]
    pub sources: Vec<SourceManifest>,
}

/// One source, as its plugin declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SourceManifest {
    /// Its name. Memory names it `plugin.name`.
    #[schemars(length(min = 1, max = 32), regex(pattern = r"^[a-z][a-z0-9_]*$"))]
    pub name: String,
    /// What it holds, for whoever chooses an agent's sources.
    #[schemars(length(max = 1024))]
    pub description: String,
    /// Whether its items may carry their own class. A source that isn't
    /// public is private: every item it returns is.
    #[serde(default)]
    pub public: bool,
    /// Whether it can be searched by words.
    #[serde(default)]
    pub words: bool,
    /// The vectors it holds, if it can be searched by them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors: Option<SourceVectors>,
}

/// The vectors a source holds: the query's vector reaches it only when the
/// runtime embedded the query with this model, at this width.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SourceVectors {
    /// The embedding model, as the deployment's model catalog names it or
    /// as its provider does.
    #[schemars(length(min = 1, max = 128))]
    pub model: String,
    /// The width of its vectors.
    #[schemars(range(min = 1, max = 65_536))]
    pub dimension: u32,
}

/// One tool, as its plugin declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ToolManifest {
    /// Its name, as `plugin_tool`.
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-z][a-z0-9_]*$"))]
    pub name: String,
    /// What it does, for the model.
    #[schemars(length(max = 2048))]
    pub description: String,
    /// The JSON Schema of its arguments.
    #[schemars(extend("x-extension" = "the tool's own schema, checked for strictness"))]
    pub input_schema: Value,
    /// The JSON Schema of its result.
    #[schemars(extend("x-extension" = "the tool's own schema, checked for strictness"))]
    pub output_schema: Value,
    /// What it can do to the world.
    pub effect: Effect,
}

impl ToolManifest {
    /// This tool as a definition.
    #[must_use]
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.clone(),
            self.description.clone(),
            self.input_schema.clone(),
            self.output_schema.clone(),
            self.effect,
        )
    }
}

/// A thing a plugin can ask for. A closed list, granted by name.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PluginCapability {
    /// Its own records in the store.
    Records,
    /// HTTP through the runtime.
    Http,
    /// Model calls through the runtime.
    Model,
    /// Notes in the runtime's long-term memory, which its search finds.
    Memory,
    /// Workflow runs started as jobs, and the state of the ones it started.
    Jobs,
    /// Offering memory the sources its manifest declares.
    Sources,
}

/// A manifest that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// It isn't YAML.
    #[error("manifest isn't valid YAML: {0}")]
    Yaml(String),
    /// It fails the manifest schema.
    #[error("manifest fails its schema: {0}")]
    Schema(#[from] Refusal),
    /// It meets the schema but breaks a rule.
    #[error("manifest: {0}")]
    Invalid(String),
}

fn invalid(message: String) -> ManifestError {
    ManifestError::Invalid(message)
}

impl PluginManifest {
    /// Reads and checks a manifest.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] if it isn't YAML, fails its schema or breaks a rule.
    pub fn from_yaml(text: &str) -> Result<Self, ManifestError> {
        let manifest: Self = schema::parse_yaml(text).map_err(|error| match error {
            YamlError::NotYaml(problem) => ManifestError::Yaml(problem),
            YamlError::Schema(refusal) => ManifestError::Schema(refusal),
            other => invalid(other.to_string()),
        })?;
        manifest.check_rules()?;
        Ok(manifest)
    }

    fn check_rules(&self) -> Result<(), ManifestError> {
        if RESERVED_NAMES.contains(&self.name.as_str()) {
            return Err(invalid(format!(
                "{} is a name the core keeps for itself",
                self.name
            )));
        }
        let tool_prefix = format!("{}_", self.name);
        let mut names = HashSet::new();
        for tool in &self.tools {
            if !tool.name.starts_with(&tool_prefix) || tool.name.len() == tool_prefix.len() {
                return Err(invalid(format!(
                    "tool {} must be named {tool_prefix}something",
                    tool.name
                )));
            }
            if !names.insert(&tool.name) {
                return Err(invalid(format!("tool {} repeats", tool.name)));
            }
            for (label, schema) in [
                ("input", &tool.input_schema),
                ("output", &tool.output_schema),
            ] {
                if let Some(violation) = check_strict(schema).first() {
                    return Err(invalid(format!(
                        "tool {}: {label} schema isn't strict at {violation}",
                        tool.name
                    )));
                }
            }
        }
        self.check_records()?;
        self.check_sources()
    }

    fn check_sources(&self) -> Result<(), ManifestError> {
        let wants = self.capabilities.contains(&PluginCapability::Sources);
        match (wants, self.sources.is_empty()) {
            (true, true) => {
                return Err(invalid(
                    "the sources capability needs declared sources".to_owned(),
                ));
            }
            (false, false) => {
                return Err(invalid(
                    "declared sources need the sources capability".to_owned(),
                ));
            }
            _ => {}
        }
        let mut names = HashSet::new();
        for source in &self.sources {
            if !names.insert(&source.name) {
                return Err(invalid(format!("source {} repeats", source.name)));
            }
            if !source.words && source.vectors.is_none() {
                return Err(invalid(format!(
                    "source {} can be searched neither by words nor by vectors",
                    source.name
                )));
            }
        }
        Ok(())
    }

    fn check_records(&self) -> Result<(), ManifestError> {
        let wants_records = self.capabilities.contains(&PluginCapability::Records);
        let Some(records) = &self.records else {
            if wants_records {
                return Err(invalid(
                    "the records capability needs declared record types".to_owned(),
                ));
            }
            return Ok(());
        };
        if !wants_records {
            return Err(invalid(
                "declared record types need the records capability".to_owned(),
            ));
        }
        let prefix = self.record_prefix();
        let relations = records.relations.iter();
        let names = records
            .types
            .iter()
            .map(|t| &t.name)
            .chain(relations.clone().map(|r| &r.name))
            .chain(relations.clone().map(|r| &r.from))
            .chain(relations.map(|r| &r.to));
        for name in names {
            if !name.starts_with(&prefix) {
                return Err(invalid(format!("{name} must start with {prefix}")));
            }
        }
        Ok(())
    }

    /// The prefix every one of its record types and relations carries.
    #[must_use]
    pub fn record_prefix(&self) -> String {
        format!("{}.", self.name)
    }

    /// Its tools, as definitions.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(ToolManifest::definition).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str =
        "{type: object, additionalProperties: false, properties: {a: {type: integer}}}";

    fn manifest(tool_name: &str, input: &str, tail: &str) -> String {
        format!(
            "api_version: 1\nname: demo\nversion: 1.0.0\ndescription: d\ntools:\n  - name: {tool_name}\n    description: t\n    input_schema: {input}\n    output_schema: {INPUT}\n    effect: read\n{tail}"
        )
    }

    fn records(type_name: &str, to: &str) -> String {
        format!(
            "capabilities: [records]\nrecords:\n  types:\n    - name: {type_name}\n      schema: {INPUT}\n      filterable: []\n  relations:\n    - {{name: demo.rel, from: demo.a, to: {to}}}\n"
        )
    }

    #[test]
    fn a_good_manifest_loads() {
        let loaded = PluginManifest::from_yaml(&manifest("demo_tool", INPUT, "")).unwrap();
        assert_eq!(loaded.definitions().len(), 1);
        assert_eq!(loaded.record_prefix(), "demo.");
    }

    #[test]
    fn records_with_the_prefix_load() {
        let text = manifest("demo_tool", INPUT, &records("demo.a", "demo.a"));
        assert!(PluginManifest::from_yaml(&text).is_ok());
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let text = manifest("demo_tool", INPUT, "surprise: 1\n");
        assert!(matches!(
            PluginManifest::from_yaml(&text),
            Err(ManifestError::Schema(_))
        ));
    }

    #[test]
    fn a_tool_without_the_prefix_is_refused() {
        for name in ["memory", "other_tool", "demo_"] {
            let text = manifest(name, INPUT, "");
            assert!(PluginManifest::from_yaml(&text).is_err(), "{name}");
        }
    }

    #[test]
    fn a_loose_tool_schema_is_refused() {
        let loose = "{type: object, additionalProperties: false, properties: {a: {type: string}}}";
        let text = manifest("demo_tool", loose, "");
        let error = PluginManifest::from_yaml(&text).unwrap_err();
        assert!(error.to_string().contains("strict"), "{error}");
    }

    #[test]
    fn a_tool_cannot_repeat() {
        let one = manifest("demo_tool", INPUT, "");
        let (_, tool) = one.split_once("tools:\n").unwrap();
        let text = format!("{one}{tool}");
        assert!(PluginManifest::from_yaml(&text).is_err());
    }

    #[test]
    fn another_plugins_namespace_is_refused() {
        let text = manifest("demo_tool", INPUT, &records("other.a", "demo.a"));
        assert!(PluginManifest::from_yaml(&text).is_err());
        let text = manifest("demo_tool", INPUT, &records("demo.a", "other.b"));
        assert!(PluginManifest::from_yaml(&text).is_err());
    }

    #[test]
    fn records_and_their_capability_go_together() {
        let without = manifest("demo_tool", INPUT, "capabilities: [records]\n");
        assert!(PluginManifest::from_yaml(&without).is_err());
        let declared = records("demo.a", "demo.a").replace("capabilities: [records]\n", "");
        let text = manifest("demo_tool", INPUT, &declared);
        assert!(PluginManifest::from_yaml(&text).is_err());
    }

    #[test]
    fn a_name_the_core_keeps_is_refused() {
        for name in ["harness", "http", "memory"] {
            let text = manifest(&format!("{name}_tool"), INPUT, "")
                .replace("name: demo", &format!("name: {name}"));
            let error = PluginManifest::from_yaml(&text).unwrap_err();
            assert!(error.to_string().contains("keeps for itself"), "{error}");
        }
    }

    fn sources(capability: bool, declared: &str) -> String {
        let capabilities = if capability {
            "capabilities: [sources]\n"
        } else {
            ""
        };
        format!("{capabilities}sources:\n{declared}")
    }

    #[test]
    fn sources_load_with_their_capability() {
        let declared = "  - {name: notes, description: d, words: true}\n  - name: lake\n    description: d\n    vectors: {model: m, dimension: 4}\n";
        let loaded =
            PluginManifest::from_yaml(&manifest("demo_tool", INPUT, &sources(true, declared)))
                .unwrap();
        assert_eq!(loaded.sources.len(), 2);
        assert!(
            loaded.sources.iter().all(|source| !source.public),
            "private unless declared public"
        );
        let without = manifest("demo_tool", INPUT, &sources(false, declared));
        assert!(PluginManifest::from_yaml(&without).is_err());
        let empty = manifest("demo_tool", INPUT, "capabilities: [sources]\n");
        assert!(PluginManifest::from_yaml(&empty).is_err());
    }

    #[test]
    fn a_source_needs_a_way_to_be_searched_and_a_name_of_its_own() {
        let neither = sources(true, "  - {name: lake, description: d}\n");
        let error = PluginManifest::from_yaml(&manifest("demo_tool", INPUT, &neither)).unwrap_err();
        assert!(error.to_string().contains("neither"), "{error}");
        let twice = sources(
            true,
            "  - {name: a, description: d, words: true}\n  - {name: a, description: d, words: true}\n",
        );
        let error = PluginManifest::from_yaml(&manifest("demo_tool", INPUT, &twice)).unwrap_err();
        assert!(error.to_string().contains("repeats"), "{error}");
    }

    #[test]
    fn a_wrong_contract_version_is_refused() {
        let text = manifest("demo_tool", INPUT, "").replace("api_version: 1", "api_version: 2");
        assert!(PluginManifest::from_yaml(&text).is_err());
    }
}
