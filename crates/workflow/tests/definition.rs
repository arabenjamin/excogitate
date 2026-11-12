//! What a definition has to get right before it can run.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::needless_pass_by_value, reason = "a test: it reads better")]

use std::collections::{BTreeMap, BTreeSet};

use plugin_api::{Effect, ToolDefinition};
use schema::Validator;
use serde_json::{Value, json};
use workflow::{Agent, Workflow, check_agent, check_workflow};

fn tools() -> BTreeMap<String, ToolDefinition> {
    let tool = ToolDefinition::new(
        "fetch".to_owned(),
        "fetches".to_owned(),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url"],
            "properties": {
                "url": { "type": "string", "maxLength": 100 },
                "limit": { "type": "integer" }
            }
        }),
        json!({ "type": "object", "additionalProperties": false }),
        Effect::Read,
    );
    BTreeMap::from([("fetch".to_owned(), tool)])
}

fn agents() -> BTreeSet<String> {
    BTreeSet::from(["helper".to_owned()])
}

fn inputs() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["url"],
        "properties": { "url": { "type": "string", "maxLength": 100 } }
    })
}

fn workflow(steps: Value) -> Workflow {
    serde_json::from_value(json!({ "inputs": inputs(), "steps": steps })).unwrap()
}

fn fetch(id: &str) -> Value {
    json!({ "type": "tool", "id": id, "tool": "fetch", "arguments": { "url": "{{inputs.url}}" } })
}

fn problem(steps: Value) -> String {
    check_workflow(&workflow(steps), &tools(), &agents()).unwrap_err()
}

fn assert_refused(steps: Value, expected: &str) {
    let found = problem(steps);
    assert!(found.contains(expected), "{found}");
}

#[test]
fn a_sound_workflow_passes() {
    let steps = json!([
        fetch("first"),
        {
            "type": "agent", "id": "second", "agent": "helper",
            "message": "About {{inputs.url}}", "context": "{{steps.first.result.body}}"
        },
        { "type": "tool", "id": "third", "tool": "fetch",
          "arguments": { "url": "{{steps.second.answer}}" } }
    ]);
    check_workflow(&workflow(steps), &tools(), &agents()).unwrap();
}

#[test]
fn unknown_keys_are_refused() {
    let validator = Validator::for_type::<Workflow>().unwrap();
    let extra_on_workflow = json!({ "inputs": inputs(), "steps": [fetch("a")], "retries": 3 });
    assert!(validator.check(&extra_on_workflow).is_err());
    let mut step = fetch("a");
    step["on_error"] = json!("continue");
    let extra_on_step = json!({ "inputs": inputs(), "steps": [step] });
    assert!(validator.check(&extra_on_step).is_err());
    assert!(serde_json::from_value::<Workflow>(extra_on_step).is_err());
}

#[test]
fn a_step_must_say_what_it_is() {
    let validator = Validator::for_type::<Workflow>().unwrap();
    let untyped =
        json!({ "inputs": inputs(), "steps": [{ "id": "a", "tool": "fetch", "arguments": {} }] });
    assert!(validator.check(&untyped).is_err());
}

#[test]
fn tools_and_agents_must_exist() {
    assert_refused(
        json!([{ "type": "tool", "id": "a", "tool": "nope", "arguments": {} }]),
        "no tool nope",
    );
    assert_refused(
        json!([{ "type": "agent", "id": "a", "agent": "nope", "message": "hi" }]),
        "no agent nope",
    );
}

#[test]
fn a_problem_names_the_step() {
    assert_refused(
        json!([fetch("first"), { "type": "tool", "id": "second", "tool": "nope", "arguments": {} }]),
        "step second",
    );
}

#[test]
fn step_names_are_unique() {
    assert_refused(json!([fetch("a"), fetch("a")]), "two steps are named a");
}

#[test]
fn a_reference_points_backwards() {
    let forward = json!([
        { "type": "tool", "id": "a", "tool": "fetch", "arguments": { "url": "{{steps.b.result}}" } },
        fetch("b")
    ]);
    assert_refused(forward, "hasn't run yet");
    let itself = json!([
        { "type": "tool", "id": "a", "tool": "fetch", "arguments": { "url": "{{steps.a.result}}" } }
    ]);
    assert_refused(itself, "hasn't run yet");
}

#[test]
fn a_reference_must_be_to_something_declared() {
    assert_refused(
        json!([{ "type": "tool", "id": "a", "tool": "fetch", "arguments": { "url": "{{inputs.nope}}" } }]),
        "no input nope",
    );
    assert_refused(
        json!([{ "type": "tool", "id": "a", "tool": "fetch", "arguments": { "url": "{{goal}}" } }]),
        "must start with",
    );
}

#[test]
fn a_reference_uses_the_field_that_step_has() {
    assert_refused(
        json!([fetch("a"), { "type": "tool", "id": "b", "tool": "fetch",
                             "arguments": { "url": "{{steps.a.answer}}" } }]),
        "under .result",
    );
    assert_refused(
        json!([
            { "type": "agent", "id": "a", "agent": "helper", "message": "hi" },
            { "type": "tool", "id": "b", "tool": "fetch", "arguments": { "url": "{{steps.a.result}}" } }
        ]),
        "under .answer",
    );
}

#[test]
fn what_a_step_found_goes_in_context_not_the_question() {
    assert_refused(
        json!([fetch("a"), { "type": "agent", "id": "b", "agent": "helper",
                             "message": "Summarize {{steps.a.result.body}}" }]),
        "put that in context",
    );
}

#[test]
fn context_references_are_checked() {
    assert_refused(
        json!([{ "type": "agent", "id": "a", "agent": "helper", "message": "hi",
                 "context": "{{steps.later.result}}" }]),
        "hasn't run yet",
    );
}

#[test]
fn arguments_are_checked_against_the_tool() {
    let wrong_type = json!([{ "type": "tool", "id": "a", "tool": "fetch",
                              "arguments": { "url": "http://a/", "limit": "many" } }]);
    assert_refused(wrong_type, "/limit");
    let missing = json!([{ "type": "tool", "id": "a", "tool": "fetch", "arguments": {} }]);
    assert_refused(missing, "url");
    let unknown = json!([{ "type": "tool", "id": "a", "tool": "fetch",
                           "arguments": { "url": "http://a/", "extra": 1 } }]);
    assert_refused(unknown, "extra");
}

#[test]
fn a_value_filled_in_later_is_judged_when_it_runs() {
    let steps = json!([fetch("a"), { "type": "tool", "id": "b", "tool": "fetch",
        "arguments": { "url": "http://a/", "limit": "{{steps.a.result.count}}" } }]);
    check_workflow(&workflow(steps), &tools(), &agents()).unwrap();
}

#[test]
fn a_loose_inputs_schema_is_refused() {
    let loose = json!({ "inputs": { "type": "object" }, "steps": [fetch("a")] });
    let loose: Workflow = serde_json::from_value(loose).unwrap();
    let found = check_workflow(&loose, &tools(), &agents()).unwrap_err();
    assert!(found.contains("isn't strict"), "{found}");
}

#[test]
fn the_output_can_only_use_what_ran() {
    let mut bad = workflow(json!([fetch("a")]));
    bad.output = Some("{{steps.b.result}}".to_owned());
    let found = check_workflow(&bad, &tools(), &agents()).unwrap_err();
    assert!(found.starts_with("its output"), "{found}");
    bad.output = Some("{{steps.a.result.body}}".to_owned());
    check_workflow(&bad, &tools(), &agents()).unwrap();
}

#[test]
fn an_agents_tools_must_exist() {
    let agent = |tools: Value| -> Agent {
        serde_json::from_value(json!({ "system_prompt": "x", "tools": tools, "budget": null, "max_output_tokens": null })).unwrap()
    };
    check_agent(&agent(json!(["fetch"])), &tools()).unwrap();
    let found = check_agent(&agent(json!(["fetch", "nope"])), &tools()).unwrap_err();
    assert!(found.contains("nope"), "{found}");
}

fn each(items: &str, max_items: u32) -> Value {
    json!({ "items": items, "max_items": max_items })
}

fn fetch_each(id: &str, items: &str, url: &str) -> Value {
    json!({ "type": "tool", "id": id, "tool": "fetch", "arguments": { "url": url },
            "for_each": each(items, 10) })
}

#[test]
fn a_step_can_run_once_per_item() {
    let steps = json!([
        fetch("find"),
        fetch_each("read", "{{steps.find.result.links}}", "{{item.url}}"),
        { "type": "agent", "id": "sum", "agent": "helper", "message": "Sum them up",
          "context": "{{steps.read.result}} ({{steps.read.fan_out.ran}} read)" },
        { "type": "agent", "id": "each", "agent": "helper", "message": "Say one thing",
          "context": "{{item}}", "for_each": each("{{steps.read.result}}", 5) }
    ]);
    check_workflow(&workflow(steps), &tools(), &agents()).unwrap();
}

#[test]
fn only_a_step_with_for_each_has_an_item() {
    let steps = json!([{ "type": "tool", "id": "a", "tool": "fetch",
                         "arguments": { "url": "{{item}}" } }]);
    assert_refused(steps, "only a step with for_each has an item");
    let mut later = workflow(json!([fetch_each("a", "{{inputs.url}}", "{{item}}")]));
    later.output = Some("{{item}}".to_owned());
    let found = check_workflow(&later, &tools(), &agents()).unwrap_err();
    assert!(found.contains("only a step with for_each"), "{found}");
    assert_refused(
        json!([fetch("a"), { "type": "tool", "id": "b", "tool": "fetch",
              "arguments": { "url": "{{steps.a.fan_out}}" } }]),
        "output is under .result",
    );
}

#[test]
fn the_items_are_one_reference_to_something_earlier() {
    assert_refused(
        json!([fetch_each("a", "{{inputs.url}} and more", "{{item}}")]),
        "nothing else",
    );
    assert_refused(
        json!([fetch_each("a", "{{inputs.url}}{{inputs.url}}", "{{item}}")]),
        "one reference",
    );
    assert_refused(
        json!([fetch_each("a", "{{steps.a.result}}", "{{item}}")]),
        "hasn't run yet",
    );
    assert_refused(
        json!([fetch_each("a", "{{item.list}}", "{{item}}")]),
        "only a step with for_each",
    );
}

#[test]
fn an_agent_takes_its_item_in_context_only() {
    let steps = json!([{ "type": "agent", "id": "a", "agent": "helper",
        "message": "About {{item}}", "for_each": each("{{inputs.url}}", 3) }]);
    assert_refused(steps, "can't use the item");
}

#[test]
fn the_number_of_items_is_bounded() {
    let zero = json!([{ "type": "tool", "id": "a", "tool": "fetch", "arguments": { "url": "{{item}}" },
                        "for_each": each("{{inputs.url}}", 0) }]);
    assert_refused(zero, "max_items must be 1 to 50");
    let many = json!([{ "type": "tool", "id": "a", "tool": "fetch", "arguments": { "url": "{{item}}" },
                        "for_each": each("{{inputs.url}}", 51) }]);
    assert_refused(many.clone(), "max_items must be 1 to 50");
    let validator = Validator::for_type::<Workflow>().unwrap();
    assert!(
        validator
            .check(&json!({ "inputs": inputs(), "steps": many }))
            .is_err()
    );
}
