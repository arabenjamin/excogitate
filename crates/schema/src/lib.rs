//! JSON Schema at every boundary: generate a schema from a Rust type, check
//! that it is strict, and validate untrusted values against it.
//!
//! Strict means unknown keys are refused and every string and array has a
//! maximum. A place that really is open says so with [`EXTENSION_KEY`], and
//! the check then stops at that place.
//!
//! [`breaking_changes`] says whether a changed schema breaks what was built
//! against the old one, which CI checks against the last release.

mod compat;
mod strict;
mod validate;
mod yaml;

pub use compat::{Break, Role, breaking_changes};
pub use strict::{EXTENSION_KEY, Violation, check_strict, for_model};
pub use validate::{InvalidSchema, Problem, Refusal, Validator, bounded};
pub use yaml::{YamlError, parse_yaml};

use schemars::JsonSchema;
use serde_json::Value;

/// The JSON Schema (2020-12) of `T`.
#[must_use]
pub fn schema_of<T: JsonSchema>() -> Value {
    schemars::generate::SchemaGenerator::default()
        .into_root_schema_for::<T>()
        .to_value()
}
