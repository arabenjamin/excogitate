//! The two tools the brain ships with: `memory`, so a model can keep what it
//! learns, and `http_request`, so it has something to learn from. Everything
//! else comes from plugins.
//!
//! Both treat the model as an adversary. Its arguments say what it wants;
//! where data may go, who may see it and how far it can be trusted are set by
//! the brain.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod clamp;
mod http;
mod memory_tool;

pub use http::{
    Credential, HttpConfig, HttpConfigError, HttpRequestTool, MAX_BODY_CHARS, Method, readable_text,
};
pub use memory_tool::MemoryTool;
