//! Installs plugins. A plugin gets only what its manifest declared and the
//! deployment granted, and its tools must be exactly the ones it declared.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod config;
mod host;
mod records;
mod service;

pub use config::{PluginConfig, PluginsConfig};
pub use host::{HostError, PluginHost};
pub use records::ScopedRecords;
pub use service::{ServiceError, ServiceSpec};
