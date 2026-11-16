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

/// The JSON Schema (2020-12) of `T`, made [`portable`].
#[must_use]
pub fn schema_of<T: JsonSchema>() -> Value {
    let mut schema = schemars::generate::SchemaGenerator::default()
        .into_root_schema_for::<T>()
        .to_value();
    portable(&mut schema);
    schema
}

/// The formats JSON Schema 2020-12 defines.
const STANDARD_FORMATS: &[&str] = &[
    "date-time",
    "date",
    "time",
    "duration",
    "email",
    "idn-email",
    "hostname",
    "idn-hostname",
    "ipv4",
    "ipv6",
    "uri",
    "uri-reference",
    "iri",
    "iri-reference",
    "uuid",
    "uri-template",
    "json-pointer",
    "relative-json-pointer",
    "regex",
];

/// Keywords whose values are data, not schemas, so nothing in them is a
/// keyword.
const DATA_KEYWORDS: &[&str] = &["enum", "const", "default", "examples"];

/// Drops every `format` JSON Schema doesn't define, such as the `uint64`
/// and `double` a Rust type's schema carries. Validators that check formats,
/// such as Ajv in MCP clients, warn on each one; an integer's `minimum` says
/// what its `uint` did, and no validator checks the width.
pub fn portable(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            if map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|format| !STANDARD_FORMATS.contains(&format))
            {
                map.remove("format");
            }
            for (key, value) in map.iter_mut() {
                if !DATA_KEYWORDS.contains(&key.as_str()) {
                    portable(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(portable),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_portable_schema_keeps_only_the_formats_json_schema_defines() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer", "format": "uint64", "minimum": 0 },
                "share": { "type": "number", "format": "double" },
                "at": { "type": "string", "format": "date-time" },
                // A property that happens to be named format is kept.
                "format": { "type": "string", "enum": ["a"] },
                "kind": { "type": "string", "default": { "format": "uint8" } }
            },
            "$defs": { "Id": { "type": "string", "format": "uuid" } },
            "anyOf": [{ "type": "integer", "format": "int32" }]
        });
        portable(&mut schema);
        assert_eq!(
            schema["properties"]["count"],
            json!({ "type": "integer", "minimum": 0 })
        );
        assert_eq!(schema["properties"]["share"], json!({ "type": "number" }));
        assert_eq!(schema["properties"]["at"]["format"], "date-time");
        assert_eq!(schema["properties"]["format"]["type"], "string");
        assert_eq!(schema["properties"]["kind"]["default"]["format"], "uint8");
        assert_eq!(schema["$defs"]["Id"]["format"], "uuid");
        assert_eq!(schema["anyOf"][0], json!({ "type": "integer" }));
    }

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct Counted {
        count: u64,
        share: f64,
    }

    #[test]
    fn a_rust_types_schema_is_portable() {
        let schema = schema_of::<Counted>();
        assert_eq!(schema["properties"]["count"]["minimum"], 0);
        assert!(schema["properties"]["count"].get("format").is_none());
        assert!(schema["properties"]["share"].get("format").is_none());
    }
}
