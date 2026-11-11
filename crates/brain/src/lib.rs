//! Where the parts become one brain: configuration, assembly, and the tools
//! it offers over MCP. The binary only calls [`run_stdio`].

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod assemble;
mod config;
mod mcp;
mod run;

pub use assemble::{Brain, BrainError};
pub use config::{BrainConfig, ConfigError};
pub use mcp::{ChatArgs, ChatReply, ToolEnvelope};
pub use run::{RunError, run_stdio};
