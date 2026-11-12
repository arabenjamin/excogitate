use serde_json::json;

use super::*;

fn object(properties: &Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn breaks(old: &Value, new: &Value, role: Role) -> Vec<String> {
    breaking_changes(old, new, role)
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn name(max: u64) -> Value {
    json!({"type": "string", "maxLength": max})
}

#[test]
fn the_same_schema_breaks_nothing() {
    let schema = object(&json!({"name": name(64)}), &["name"]);
    assert_eq!(breaks(&schema, &schema, Role::Input), Vec::<String>::new());
    assert_eq!(breaks(&schema, &schema, Role::Output), Vec::<String>::new());
}

#[test]
fn an_input_may_widen_but_not_narrow() {
    let old = object(
        &json!({"name": name(64), "kind": {"type": "string", "enum": ["a", "b"]}}),
        &["name"],
    );
    let wider = object(
        &json!({
            "name": name(128),
            "kind": {"type": "string", "enum": ["a", "b", "c"]},
            "note": name(10),
        }),
        &["name"],
    );
    assert_eq!(breaks(&old, &wider, Role::Input), Vec::<String>::new());

    let narrower = object(
        &json!({"name": name(32), "kind": {"type": "string", "enum": ["a"]}, "note": name(10)}),
        &["name", "note"],
    );
    assert_eq!(
        breaks(&old, &narrower, Role::Input),
        [
            "kind: value \"b\" removed",
            "name: maxLength 64 became 32",
            "note: now required",
        ]
    );
    let dropped = object(&json!({"name": name(64)}), &["name"]);
    assert_eq!(
        breaks(&old, &dropped, Role::Input),
        ["kind: property removed"]
    );
}

#[test]
fn an_output_may_add_a_property_but_not_loosen_one() {
    let old = object(
        &json!({"outcome": {"type": "string", "enum": ["done"]}, "answer": name(100)}),
        &["outcome", "answer"],
    );
    let added = object(
        &json!({
            "outcome": {"type": "string", "enum": ["done"]},
            "answer": name(100),
            "extra": name(5),
        }),
        &["outcome", "answer"],
    );
    assert_eq!(breaks(&old, &added, Role::Output), Vec::<String>::new());

    let looser = object(
        &json!({"outcome": {"type": "string", "enum": ["done", "stopped"]}, "answer": name(200)}),
        &["outcome"],
    );
    assert_eq!(
        breaks(&old, &looser, Role::Output),
        [
            "answer: maxLength 100 became 200",
            "outcome: value \"stopped\" added",
            "answer: no longer always present",
        ]
    );
}

#[test]
fn a_type_that_no_longer_takes_null_is_a_break() {
    let old = json!({"type": ["string", "null"], "maxLength": 9});
    let new = json!({"type": "string", "maxLength": 9});
    assert_eq!(
        breaks(&old, &new, Role::Input),
        ["(top): type null|string became string"]
    );
    assert_eq!(breaks(&old, &new, Role::Output), Vec::<String>::new());
}

#[test]
fn integers_fit_where_numbers_do() {
    let number = json!({"type": "number"});
    let integer = json!({"type": "integer"});
    assert_eq!(breaks(&integer, &number, Role::Input), Vec::<String>::new());
    assert_eq!(
        breaks(&number, &integer, Role::Input),
        ["(top): type number became integer"]
    );
}

#[test]
fn references_are_followed_and_choices_matched() {
    let old = json!({
        "$defs": {"Kind": {"oneOf": [{"const": "a"}, {"const": "b"}]}},
        "type": "object",
        "properties": {"kind": {"$ref": "#/$defs/Kind"}},
        "additionalProperties": false,
    });
    let same_inline = json!({
        "type": "object",
        "properties": {"kind": {"oneOf": [{"const": "b"}, {"const": "a"}]}},
        "additionalProperties": false,
    });
    assert_eq!(
        breaks(&old, &same_inline, Role::Input),
        Vec::<String>::new()
    );
    let fewer = json!({
        "type": "object",
        "properties": {"kind": {"oneOf": [{"const": "a"}]}},
        "additionalProperties": false,
    });
    assert_eq!(
        breaks(&old, &fewer, Role::Input),
        ["kind|1: value \"b\" removed"]
    );
}

#[test]
fn a_recursive_type_ends() {
    let tree = json!({
        "$defs": {"Node": {
            "type": "object",
            "properties": {"children": {"type": "array", "maxItems": 4, "items": {"$ref": "#/$defs/Node"}}},
            "additionalProperties": false,
        }},
        "$ref": "#/$defs/Node",
    });
    assert_eq!(breaks(&tree, &tree, Role::Input), Vec::<String>::new());
}

#[test]
fn an_open_place_takes_anything() {
    let old = json!({"x-extension": "a plugin's own arguments"});
    let new = json!({"type": "object"});
    assert_eq!(breaks(&new, &old, Role::Input), Vec::<String>::new());
    assert_eq!(breaks(&json!({}), &new, Role::Output), Vec::<String>::new());
}

#[test]
fn a_changed_condition_is_reported() {
    let old = json!({"if": {"required": ["a"]}, "then": {"required": ["b"]}});
    let new = json!({"if": {"required": ["a"]}, "then": {"required": ["c"]}});
    assert_eq!(breaks(&old, &new, Role::Input), ["(top): `then` changed"]);
}

#[test]
fn a_new_kind_of_output_matches_no_old_choice() {
    let step = |kind: &str, field: &str| {
        object(
            &json!({"type": {"const": kind}, field: {"type": "integer"}}),
            &["type", field],
        )
    };
    let old = json!({"oneOf": [step("start", "at"), step("end", "cost")]});
    let new = json!({"oneOf": [step("start", "at"), step("end", "cost"), step("call", "index")]});
    assert_eq!(
        breaks(&old, &new, Role::Output),
        ["(top)|2: a new value matches none of the old choices"]
    );
}
