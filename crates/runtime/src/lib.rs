//! Where the parts become one runtime: configuration, assembly, and the tools
//! it offers over MCP. A binary only calls [`main`] with its [`Program`]:
//! its name, its version and the plugins compiled into it.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod assemble;
mod capability;
mod cli;
mod config;
mod connect;
mod definitions;
mod delegate;
mod harness;
mod http;
mod jobs;
mod ledger;
mod mcp;
mod run;
mod schemas;

pub use assemble::{Plugins, Runtime, RuntimeError};
pub use cli::{Program, main};
pub use config::{Config, ConfigError, HarnessConfig, ServerConfig};
pub use connect::relay;
pub use harness::{PickArgs, Picked};
pub use http::{
    ApiError, AttemptList, AttemptView, ChangeList, ChangeView, DefinitionList, DefinitionView,
    HoldList, HoldRequest, HttpError, JobList, JobStatusView, JobView, LessonList, OpenAiRequest,
    RunList, RunRequest, RunStarted, RunView,
};
pub use jobs::WorkflowArgs;
pub use mcp::{ChatArgs, ChatReply, ToolEnvelope};
pub use plugin_host::PluginsConfig;
pub use run::{RunError, run_connect, run_server, run_stdio};
pub use schemas::bundled_tool_schemas;
