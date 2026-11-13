{%- set caps = capabilities | split(pat=",") -%}
{%- if tools %}{% set tool_list = tools | split(pat=",") %}{% else %}{% set tool_list = [] %}{% endif -%}
{%- set granted = [] -%}
{%- for capability in ["records", "http", "model", "memory", "jobs"] -%}
{%- if capability in caps %}{% set_global granted = granted | concat(with=capability) %}{% endif -%}
{%- endfor -%}
{%- set kind = name | title -%}
//! The `{{ name }}` plugin: what it does is its manifest's description.
//!
//! The same plugin runs compiled in ([`{{ kind }}Plugin`]) and as its own
//! process (`src/main.rs`), with one manifest and the same capabilities
//! either way. Each tool's arguments have met its input schema before it
//! runs, and its result is checked against its output schema.

use std::sync::Arc;

use async_trait::async_trait;
use plugin_api::{
    Capabilities, Plugin, PluginError,
{%- if tool_list %} PluginManifest, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput, Trust,{% else %} Tool,{% endif %}
{%- if "sources" in caps %} Source, SourceFound, SourceGot, SourceQuery,{% if not tool_list %} ToolContext, ToolFailure,{% endif %}{% endif %}
{%- if "records" in granted and tool_list %} RecordsCapability,{% endif %}
{%- if "http" in granted and tool_list %} HttpCapability,{% endif %}
{%- if "model" in granted and tool_list %} ModelCapability,{% endif %}
{%- if "memory" in granted and tool_list %} MemoryCapability,{% endif %}
{%- if "jobs" in granted and tool_list %} JobsCapability,{% endif %}
};
{%- if tool_list %}
use serde_json::{Value, json};
{%- endif %}

/// The manifest.
pub const MANIFEST: &str = include_str!("../plugin.yaml");

/// The compiled-in plugin.
#[derive(Debug)]
pub struct {{ kind }}Plugin;

impl Plugin for {{ kind }}Plugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }
{% if tool_list %}
    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
{%- if granted %}
        let granted = Granted {
{%- for capability in granted %}
            {{ capability }}: capabilities.{{ capability }}()?,
{%- endfor %}
        };
{%- else %}
        let _ = capabilities;
{%- endif %}
        Ok(PluginManifest::from_yaml(MANIFEST)
            .map_err(|error| PluginError::Other(error.to_string()))?
            .definitions()
            .into_iter()
            .map(|definition| {
                Arc::new({{ kind }}Tool {
                    definition,
{%- if granted %}
                    granted: granted.clone(),
{%- endif %}
                }) as Arc<dyn Tool>
            })
            .collect())
    }
{%- else %}
    fn tools(&self, _capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        Ok(Vec::new())
    }
{%- endif %}
{%- if "sources" in caps %}

    fn sources(&self, _capabilities: &Capabilities) -> Result<Vec<Arc<dyn Source>>, PluginError> {
        Ok(vec![Arc::new({{ kind }}Source)])
    }
{%- endif %}
}
{%- if tool_list %}
{%- if granted %}

/// What the runtime granted the plugin, as its manifest asked.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "the template's stubs use none of it yet")]
struct Granted {
{%- for capability in granted %}
    {{ capability }}: Arc<dyn {{ capability | title }}Capability>,
{%- endfor %}
}
{%- endif %}

/// Every tool the manifest declares; `run` goes by its name.
#[derive(Debug)]
struct {{ kind }}Tool {
    definition: ToolDefinition,
{%- if granted %}
    #[allow(dead_code, reason = "the template's stubs use none of it yet")]
    granted: Granted,
{%- endif %}
}

#[async_trait]
impl Tool for {{ kind }}Tool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let value = match self.definition.name.as_str() {
{%- for tool in tool_list %}
            "{{ name }}_{{ tool }}" => self.{{ tool }}(arguments, context).await?,
{%- endfor %}
            other => return Err(ToolFailure::new(format!("{other} isn't a {{ name }} tool"))),
        };
        // Untrusted unless every word of it is the plugin's own: what came
        // from outside or from a model must never be read as instructions.
        Ok(ToolOutput::new(value, Trust::Untrusted))
    }
}

impl {{ kind }}Tool {
{%- for tool in tool_list %}
    /// TODO: `{{ name }}_{{ tool }}`.
    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::unused_self,
        clippy::needless_pass_by_value,
        reason = "a stub: the real one uses them"
    )]
    async fn {{ tool }}(&self, _arguments: Value, _context: ToolContext) -> Result<Value, ToolFailure> {
        Ok(json!({}))
    }
{%- if not loop.last %}
{% endif %}
{%- endfor %}
}
{%- endif %}
{%- if "sources" in caps %}

/// What the plugin offers memory to read.
#[derive(Debug)]
struct {{ kind }}Source;

#[async_trait]
impl Source for {{ kind }}Source {
    fn name(&self) -> &'static str {
        "{{ name }}"
    }

    /// TODO: the items matching the query, best first.
    async fn search(&self, _query: SourceQuery, _context: ToolContext) -> Result<SourceFound, ToolFailure> {
        Ok(SourceFound::new(Vec::new()))
    }

    /// TODO: one item by its id.
    async fn get(&self, _id: &str, _context: ToolContext) -> Result<SourceGot, ToolFailure> {
        Ok(SourceGot::new(None))
    }
}
{%- endif %}
