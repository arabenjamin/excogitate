//! A remote tool's schemas. Every schema the runtime registers is strict,
//! and a remote server's rarely are, so its input schema is tightened: an
//! object with named properties is closed, an array and a string get a
//! maximum, and a place that accepts anything, such as an open map, is
//! marked as the server's own extension point. The server checks its own
//! arguments too; this bounds what a model can send it. What can't be made
//! strict is refused, naming where.

use schema::{EXTENSION_KEY, check_strict};
use serde_json::{Map, Value, json};

/// The longest string a model may send a remote tool, where the server
/// sets none.
pub const MAX_STRING_CHARS: u64 = 20_000;
/// The most items in an array a model may send, where the server sets none.
pub const MAX_ITEMS: u64 = 1_000;

const EXTENSION: &str = "the remote server's own schema";
const BOUNDED_FORMATS: [&str; 3] = ["uuid", "date-time", "date"];
const SINGLE_KEYS: [&str; 2] = ["items", "not"];
const LIST_KEYS: [&str; 4] = ["oneOf", "anyOf", "allOf", "prefixItems"];
const MAP_KEYS: [&str; 3] = ["properties", "$defs", "definitions"];

fn has_type(schema: &Map<String, Value>, wanted: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(name)) => name == wanted,
        Some(Value::Array(names)) => names.iter().any(|name| name == wanted),
        _ => false,
    }
}

fn has_shape(schema: &Map<String, Value>) -> bool {
    ["type", "enum", "const", "$ref"]
        .iter()
        .any(|key| schema.contains_key(*key))
        || LIST_KEYS.iter().any(|key| {
            schema
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|list| !list.is_empty())
        })
}

fn open(schema: &mut Map<String, Value>) {
    schema.insert(
        EXTENSION_KEY.to_owned(),
        Value::String(EXTENSION.to_owned()),
    );
}

fn tighten_node(node: &mut Value) {
    if *node == Value::Bool(true) {
        *node = json!({ EXTENSION_KEY: EXTENSION });
        return;
    }
    let Value::Object(schema) = node else {
        return;
    };
    if schema.contains_key(EXTENSION_KEY) {
        return;
    }
    if !has_shape(schema) {
        open(schema);
        return;
    }
    if has_type(schema, "object") && schema.get("additionalProperties") != Some(&Value::Bool(false))
    {
        let named = schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| !properties.is_empty());
        if named {
            schema.insert("additionalProperties".to_owned(), Value::Bool(false));
        } else {
            // A map the server fills as it likes: its own extension point.
            open(schema);
            return;
        }
    }
    if has_type(schema, "array") && !schema.contains_key("maxItems") {
        schema.insert("maxItems".to_owned(), json!(MAX_ITEMS));
    }
    let closed = schema.contains_key("enum") || schema.contains_key("const");
    let bounded_format = schema
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|format| BOUNDED_FORMATS.contains(&format));
    if has_type(schema, "string") && !closed && !bounded_format && !schema.contains_key("maxLength")
    {
        schema.insert("maxLength".to_owned(), json!(MAX_STRING_CHARS));
    }
    for key in SINGLE_KEYS {
        if let Some(child) = schema.get_mut(key) {
            tighten_node(child);
        }
    }
    for key in LIST_KEYS {
        if let Some(Value::Array(children)) = schema.get_mut(key) {
            children.iter_mut().for_each(tighten_node);
        }
    }
    for key in MAP_KEYS {
        if let Some(Value::Object(children)) = schema.get_mut(key) {
            children.values_mut().for_each(tighten_node);
        }
    }
}

/// A remote tool's input schema, made strict.
///
/// # Errors
///
/// Why it can't be: it isn't an object's, or a place in it stays open.
pub fn tighten(remote: &Value) -> Result<Value, String> {
    let mut schema = remote.clone();
    let Value::Object(root) = &mut schema else {
        return Err("its input schema isn't an object".to_owned());
    };
    // A meta-schema address would be fetched by some validators; the
    // runtime's checks fix the draft.
    root.remove("$schema");
    root.remove("$id");
    if !has_type(root, "object") {
        return Err("its input schema isn't for an object".to_owned());
    }
    if !root.contains_key("properties") {
        root.insert("properties".to_owned(), json!({}));
    }
    // The root is closed even with no properties: a tool's arguments are
    // named.
    root.insert("additionalProperties".to_owned(), Value::Bool(false));
    tighten_node(&mut schema);
    if let Some(violation) = check_strict(&schema).first() {
        return Err(format!(
            "its input schema can't be made strict at {violation}"
        ));
    }
    Ok(schema)
}

/// What every remote tool returns: the text it answered with, its
/// structured result as it sent it, and whether either was cut.
#[must_use]
pub fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["text", "truncated"],
        "properties": {
            "text": {
                "type": "string",
                "maxLength": crate::config::MAX_RESULT_CHARS,
                "description": "The server's text content, joined; anything else it sent is named in brackets."
            },
            "structured": {
                EXTENSION_KEY: "the server's structured result, as it sent it",
                "description": "The server's structured result, when it sent one that fits."
            },
            "truncated": {
                "type": "boolean",
                "description": "True if the text was cut, or a structured result was left out for its size."
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

    use super::*;

    #[test]
    fn a_loose_schema_is_tightened() {
        let remote = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to look for"},
                "skills": {"type": "array", "items": {"type": "string"}},
                "level": {"type": "string", "enum": ["junior", "senior"]},
                "since": {"type": "string", "format": "date"},
                "filters": {"type": "object", "properties": {"remote": {"type": "boolean"}}},
                "extra": {"type": "object"},
                "anything": {},
                "limit": {"type": "integer"}
            },
            "required": ["query"]
        });
        let strict = tighten(&remote).unwrap();
        assert_eq!(check_strict(&strict), vec![]);
        assert_eq!(strict.get("$schema"), None);
        assert_eq!(strict["additionalProperties"], false);
        let properties = &strict["properties"];
        assert_eq!(properties["query"]["maxLength"], MAX_STRING_CHARS);
        assert_eq!(properties["query"]["description"], "What to look for");
        assert_eq!(properties["skills"]["maxItems"], MAX_ITEMS);
        assert_eq!(properties["skills"]["items"]["maxLength"], MAX_STRING_CHARS);
        assert_eq!(properties["level"].get("maxLength"), None);
        assert_eq!(properties["since"].get("maxLength"), None);
        assert_eq!(properties["filters"]["additionalProperties"], false);
        assert_eq!(properties["extra"][EXTENSION_KEY], EXTENSION);
        assert_eq!(properties["anything"][EXTENSION_KEY], EXTENSION);
        assert_eq!(strict["required"], json!(["query"]));
    }

    #[test]
    fn a_tool_with_no_arguments_takes_none() {
        let strict = tighten(&json!({"type": "object"})).unwrap();
        assert_eq!(strict["additionalProperties"], false);
        assert_eq!(strict["properties"], json!({}));
    }

    #[test]
    fn what_cant_be_an_arguments_schema_is_refused() {
        assert!(tighten(&json!("object")).is_err());
        assert!(
            tighten(&json!({"type": "string"}))
                .unwrap_err()
                .contains("isn't for an object")
        );
    }

    #[test]
    fn the_output_schema_is_strict() {
        assert_eq!(check_strict(&output_schema()), vec![]);
    }
}
