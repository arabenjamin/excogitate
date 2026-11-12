//! The plugin contract. It's opinionated about how a plugin is built and has
//! no opinion on what a plugin does.
//!
//! A plugin has a manifest ([`PluginManifest`]) that declares its tools with
//! their schemas and names the narrow [capabilities](Capabilities) it needs.
//! Nothing here lets a plugin call another tool. It can run compiled in, as a
//! [`Plugin`] behind a cargo feature, or as its own process through
//! [`serve_plugin_stdio`], with the same capabilities reaching back to the host.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod capability;
mod manifest;
mod process;
#[doc(hidden)]
pub mod rpc;
mod serve;
mod serve_sources;
mod source;
mod tool;
#[doc(hidden)]
pub mod wire;

pub use capability::{
    Capabilities, CapabilityError, HttpCapability, HttpRequest, HttpResponse, JobPhase, JobStarted,
    JobState, JobsCapability, MAX_BATCH, MAX_IDEMPOTENCY_KEY, MAX_NOTE_TAG, MAX_NOTE_TAGS,
    MAX_NOTE_TEXT, MAX_NOTE_TITLE, MAX_NOTE_URL, MAX_OUTSTANDING_JOBS, MemoryCapability,
    ModelCapability, ModelReply, NoteStored, NoteToStore, RecordLink, RecordWrite,
    RecordsCapability, RunWorkflow, check_batch,
};
pub use manifest::{
    API_VERSION, ManifestError, PluginCapability, PluginManifest, RESERVED_NAMES, SourceManifest,
    SourceVectors, ToolManifest,
};
pub use process::{serve_plugin, serve_plugin_stdio};
pub use source::{
    MAX_HIT_TEXT, MAX_ITEM_ID, MAX_ITEM_TEXT, MAX_ITEM_TITLE, MAX_ITEM_URL, MAX_SOURCE_HITS,
    Source, SourceFound, SourceItem, SourceQuery,
};
// The runtime's side of MCP. It isn't part of the plugin contract and can
// change in any release.
#[doc(hidden)]
pub use serve::{MAX_LINE_BYTES, MCP_VERSION, McpHandler, read_line, serve, serve_stdio};
pub use tool::{
    CapabilityUse, Effect, Plugin, PluginError, Tool, ToolContext, ToolDefinition, ToolFailure,
    ToolOutput,
};

/// The store's vocabulary a plugin works in, so a plugin needs only this
/// crate.
pub use protocol::{DataClass, RunId, Trust};
pub use store::{
    Direction, FieldFilter, FieldOp, FieldValue, Page, PageRequest, Provenance, Record,
    RecordDecls, RecordId, RecordQuery,
};
