//! The `http_request` tool: how a model reaches the outside world.
//!
//! It defends against the model that calls it and the sites it fetches. The
//! address of every hop is checked, including after DNS, so a name can't
//! rebind to something internal. Keys are attached by the runtime and never
//! reach the model. Whatever comes back is labelled untrusted.

mod config;
mod guard;
mod text;
mod tool;

pub use config::{Credential, HttpConfig, HttpConfigError, MAX_BODY_CHARS, Method};
pub use guard::AddressGuard;
pub use text::readable_text;
pub use tool::HttpRequestTool;
