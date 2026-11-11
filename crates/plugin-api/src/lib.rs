//! The plugin contract. It's opinionated about how a plugin is built and has
//! no opinion on what a plugin does.
//!
//! A plugin has a manifest ([`PluginManifest`]) that declares its tools with
//! their schemas and names the narrow [capabilities](Capabilities) it needs.
//! Nothing here lets a plugin call another tool. It can run compiled in, as a
//! [`Plugin`] behind a cargo feature, or as its own process through [`serve`].

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod capability;
mod manifest;
mod serve;
mod tool;

pub use capability::{
    Capabilities, CapabilityError, HttpCapability, HttpRequest, HttpResponse, ModelCapability,
    ModelReply, RecordsCapability,
};
pub use manifest::{API_VERSION, ManifestError, PluginCapability, PluginManifest, ToolManifest};
pub use serve::{MAX_LINE_BYTES, MCP_VERSION, McpHandler, read_line, serve, serve_stdio};
pub use tool::{
    Effect, Plugin, PluginError, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput,
};
