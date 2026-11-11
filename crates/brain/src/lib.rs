//! Where the parts become one brain: configuration, assembly, and the tools
//! it offers over MCP. The binary only calls [`run_stdio`] and [`run_server`].

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod assemble;
mod config;
mod connect;
mod definitions;
mod http;
mod jobs;
mod ledger;
mod mcp;
mod run;

pub use assemble::{Brain, BrainError};
pub use config::{BrainConfig, ConfigError, ServerConfig};
pub use connect::relay;
pub use http::{
    ApiError, DefinitionList, DefinitionView, HttpError, JobList, JobStatusView, JobView,
    OpenAiRequest, RunList, RunRequest, RunStarted, RunView,
};
pub use jobs::WorkflowArgs;
pub use mcp::{ChatArgs, ChatReply, ToolEnvelope};
pub use run::{RunError, run_connect, run_server, run_stdio};
