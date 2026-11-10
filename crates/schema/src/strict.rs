//! The strictness check. It runs on every generated schema in CI, and on
//! every schema a plugin declares when the plugin registers it.

use serde_json::{Map, Value};

/// Marks a declared extension point in a schema. Its value says why the
/// place is open, and the check doesn't look inside it.
pub const EXTENSION_KEY: &str = "x-extension";

/// String formats whose values have a fixed shape, so need no `maxLength`.
const BOUNDED_FORMATS: [&str; 2] = ["uuid", "date-time"];

/// Keys holding one sub-schema.
const SINGLE_KEYS: [&str; 3] = ["items", "not", "additionalProperties"];
/// Keys holding a list of sub-schemas.
const LIST_KEYS: [&str; 4] = ["oneOf", "anyOf", "allOf", "prefixItems"];
/// Keys holding sub-schemas by name.
const MAP_KEYS: [&str; 2] = ["properties", "$defs"];

/// One place where a schema is not strict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Where in the schema, as a path of keys.
    pub path: String,
    /// What is wrong there.
    pub rule: &'static str,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.rule)
    }
}

/// Every place where `schema` is not strict. Empty means it passes.
#[must_use]
pub fn check_strict(schema: &Value) -> Vec<Violation> {
    let mut found = Vec::new();
    walk(schema, "#", &mut found);
    found
}

fn walk(node: &Value, path: &str, found: &mut Vec<Violation>) {
    let Value::Object(schema) = node else {
        if *node != Value::Bool(false) {
            found.push(Violation {
                path: path.to_owned(),
                rule: "accepts anything and doesn't declare an extension point",
            });
        }
        return;
    };
    if schema.contains_key(EXTENSION_KEY) {
        return;
    }
    check_node(schema, path, found);
    for key in SINGLE_KEYS {
        match schema.get(key) {
            // `additionalProperties: false` is the rule being met, not a schema.
            Some(Value::Bool(false)) | None => {}
            Some(child) => walk(child, &format!("{path}/{key}"), found),
        }
    }
    for key in LIST_KEYS {
        if let Some(Value::Array(children)) = schema.get(key) {
            for (index, child) in children.iter().enumerate() {
                walk(child, &format!("{path}/{key}/{index}"), found);
            }
        }
    }
    for key in MAP_KEYS {
        if let Some(Value::Object(children)) = schema.get(key) {
            for (name, child) in children {
                walk(child, &format!("{path}/{key}/{name}"), found);
            }
        }
    }
}

fn check_node(schema: &Map<String, Value>, path: &str, found: &mut Vec<Violation>) {
    let mut flag = |rule| {
        found.push(Violation {
            path: path.to_owned(),
            rule,
        });
    };
    let closed_values = schema.contains_key("enum") || schema.contains_key("const");
    let has_shape = closed_values
        || schema.contains_key("type")
        || schema.contains_key("$ref")
        || LIST_KEYS.iter().any(|key| schema.contains_key(*key));
    if !has_shape {
        flag("accepts anything and doesn't declare an extension point");
    }
    if has_type(schema, "object") && schema.get("additionalProperties") != Some(&Value::Bool(false))
    {
        flag("object must set additionalProperties: false");
    }
    if has_type(schema, "array") && !schema.contains_key("maxItems") {
        flag("array has no maxItems");
    }
    if has_type(schema, "string")
        && !closed_values
        && !schema.contains_key("maxLength")
        && !has_bounded_format(schema)
    {
        flag("string has no maxLength");
    }
}

fn has_type(schema: &Map<String, Value>, wanted: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(name)) => name == wanted,
        Some(Value::Array(names)) => names.iter().any(|name| name == wanted),
        _ => false,
    }
}

fn has_bounded_format(schema: &Map<String, Value>) -> bool {
    schema
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|format| BOUNDED_FORMATS.contains(&format))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(schema: &Value) -> Vec<&'static str> {
        check_strict(schema).into_iter().map(|v| v.rule).collect()
    }

    #[test]
    fn a_strict_object_passes() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "maxLength": 10 },
                "when": { "type": "string", "format": "date-time" },
                "mode": { "type": "string", "enum": ["a", "b"] },
                "tags": { "type": "array", "maxItems": 3,
                          "items": { "type": "string", "maxLength": 5 } },
                "note": { "type": ["string", "null"], "maxLength": 9 },
                "other": { "$ref": "#/$defs/x" }
            },
            "$defs": { "x": { "type": "integer" } }
        });
        assert_eq!(check_strict(&schema), vec![]);
    }

    #[test]
    fn an_open_object_fails() {
        let schema = json!({ "type": "object", "properties": {} });
        assert_eq!(
            rules(&schema),
            vec!["object must set additionalProperties: false"]
        );
    }

    #[test]
    fn a_nullable_unbounded_string_fails() {
        let schema = json!({ "type": ["string", "null"] });
        assert_eq!(rules(&schema), vec!["string has no maxLength"]);
    }

    #[test]
    fn an_unbounded_array_fails() {
        let schema = json!({ "type": "array", "items": { "type": "integer" } });
        assert_eq!(rules(&schema), vec!["array has no maxItems"]);
    }

    #[test]
    fn a_problem_deep_in_defs_is_found_with_its_path() {
        let schema = json!({
            "$ref": "#/$defs/a",
            "$defs": { "a": { "anyOf": [{ "type": "string" }, { "type": "null" }] } }
        });
        let found = check_strict(&schema);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "#/$defs/a/anyOf/0");
    }

    #[test]
    fn a_schema_that_accepts_anything_fails() {
        assert_eq!(rules(&json!(true)).len(), 1);
        assert_eq!(rules(&json!({ "description": "anything" })).len(), 1);
    }

    #[test]
    fn a_declared_extension_point_passes() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "open": { "x-extension": "the plugin's own data" } }
        });
        assert_eq!(check_strict(&schema), vec![]);
    }
}
