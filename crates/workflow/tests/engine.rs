//! Running workflows with real dispatch, a scripted model and recording tools.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chaos_model::{ChaosServer, ChaosStep};
use chrono::Utc;
use dispatch::{Budget, ToolRegistry};
use events::MemorySink;
use model::{Catalog, Gateway, MemoryLedger, OpenAiClient};
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::Trust;
use serde_json::{Value, json};
use store::{Definition, DefinitionKey, DefinitionKind, Store, UnitOfWork, WriteOp};
use store_memory::MemoryStore;
use workflow::{Engine, EngineLimits, RunOutcome, StepStatus, WorkflowError};

/// Records what it was given and what was left to spend.
#[derive(Debug, Default)]
struct Recorder {
    calls: Mutex<Vec<(Value, u64)>>,
}

#[derive(Debug)]
struct Fake {
    name: &'static str,
    effect: Effect,
    reply: Result<Value, &'static str>,
    cost: u64,
    delay: Duration,
    log: Arc<Recorder>,
}

fn open_object() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "text": { "type": "string", "maxLength": 2_000_000 },
            "count": { "type": "integer" }
        }
    })
}

#[async_trait]
impl Tool for Fake {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.to_owned(),
            "a fake".to_owned(),
            open_object(),
            open_object(),
            self.effect,
        )
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        tokio::time::sleep(self.delay).await;
        self.log
            .calls
            .lock()
            .unwrap()
            .push((arguments, context.budget_usd_micros));
        match &self.reply {
            Ok(value) => Ok(ToolOutput::new(value.clone(), Trust::Untrusted).with_cost(self.cost)),
            Err(why) => Err(ToolFailure::new((*why).to_owned())),
        }
    }
}

const BUDGET: Budget = Budget {
    max_rounds: 4,
    max_tool_calls: 4,
    max_writes: 1,
    max_total_tokens: 100_000,
    max_usd_micros: 0,
    max_request_bytes: 600_000,
    max_duration_ms: 20_000,
};

struct Harness {
    engine: Engine,
    server: ChaosServer,
    log: Arc<Recorder>,
}

fn tool(
    name: &'static str,
    reply: Result<Value, &'static str>,
    cost: u64,
    log: &Arc<Recorder>,
) -> Arc<Fake> {
    Arc::new(Fake {
        name,
        effect: Effect::Write,
        reply,
        cost,
        delay: Duration::ZERO,
        log: Arc::clone(log),
    })
}

async fn harness(script: Vec<ChaosStep>, extra: Vec<Arc<Fake>>, log: Arc<Recorder>) -> Harness {
    let server = ChaosServer::start(script).await.unwrap();
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 1000000, max_output_tokens: 100}}\n",
        server.base_url
    );
    let gateway = Gateway::new(
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        Arc::new(OpenAiClient::new().unwrap()),
        Arc::new(MemoryLedger::default()),
    );
    let mut registry = ToolRegistry::new();
    for fake in extra {
        registry.register(fake).unwrap();
    }
    let engine = Engine {
        registry: Arc::new(registry),
        gateway: Arc::new(gateway),
        sink: Arc::new(MemorySink::new()),
        store: Arc::new(MemoryStore::new(3)),
        limits: EngineLimits {
            agent_budget: BUDGET,
            agent_result_chars: 1000,
            agent_output_tokens: 50,
            time_limit: Duration::from_secs(20),
        },
        stop: dispatch::StopSignal::new(),
        sources: workflow::SourceChoices::default(),
    };
    Harness {
        engine,
        server,
        log,
    }
}

async fn put(store: &Arc<dyn Store>, kind: DefinitionKind, name: &str, body: Value) {
    let definition = Definition {
        key: DefinitionKey {
            kind,
            name: name.to_owned(),
        },
        body,
        source: "test".to_owned(),
        updated_at: Utc::now(),
    };
    store
        .apply(UnitOfWork::new().with(WriteOp::PutDefinition(definition)))
        .await
        .unwrap();
}

fn inputs() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["subject"],
        "properties": { "subject": { "type": "string", "maxLength": 100 } }
    })
}

fn agent() -> Value {
    json!({ "system_prompt": "You help.", "tools": [], "budget": null, "max_output_tokens": null })
}

fn agent_step() -> Value {
    json!({
        "type": "agent", "id": "think", "agent": "helper",
        "message": "About {{inputs.subject}}", "context": "{{steps.fetch.result.text}}"
    })
}

fn tool_step(id: &str, tool: &str, arguments: &Value) -> Value {
    json!({ "type": "tool", "id": id, "tool": tool, "arguments": arguments })
}

async fn define(harness: &Harness, steps: Value, extra: Value) {
    let mut workflow = json!({ "inputs": inputs(), "steps": steps });
    for (key, value) in extra.as_object().unwrap() {
        workflow[key] = value.clone();
    }
    let store = &harness.engine.store;
    put(store, DefinitionKind::Workflow, "flow", workflow).await;
    put(store, DefinitionKind::Agent, "helper", agent()).await;
}

fn subject() -> Value {
    json!({ "subject": "otters" })
}

#[tokio::test]
async fn steps_feed_each_other_in_order() {
    let log = Arc::new(Recorder::default());
    let fake = tool(
        "fake",
        Ok(json!({ "text": "fetched", "count": 7 })),
        0,
        &log,
    );
    let harness = harness(vec![ChaosStep::Text("thought".to_owned())], vec![fake], log).await;
    let steps = json!([
        tool_step("fetch", "fake", &json!({ "text": "{{inputs.subject}}" })),
        agent_step(),
        tool_step(
            "keep",
            "fake",
            &json!({
                "text": "{{steps.think.answer}}: {{steps.fetch.result.text}}",
                "count": "{{steps.fetch.result.count}}"
            })
        ),
    ]);
    define(
        &harness,
        steps,
        json!({ "output": "{{steps.think.answer}}" }),
    )
    .await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    assert_eq!(run.outcome, RunOutcome::Completed, "{run:?}");
    assert_eq!(run.output, Some(json!("thought")));
    assert_eq!(harness.server.served().len(), 1);
    let calls = harness.log.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, json!({ "text": "otters" }));
    assert_eq!(
        calls[1].0,
        json!({ "text": "thought: fetched", "count": 7 })
    );
    assert!(run.steps.iter().all(|step| step.status == StepStatus::Ran));
    assert!(run.steps.iter().all(|step| step.receipt.is_some()));
    let by_harness = &run.steps[0].receipt.as_ref().unwrap().tool_calls[0];
    assert!(by_harness.by_harness, "the model didn't ask for this call");
}

#[tokio::test]
async fn a_failed_step_stops_the_run_and_says_which() {
    let log = Arc::new(Recorder::default());
    let bad = tool("bad", Err("it broke"), 0, &log);
    let good = tool("good", Ok(json!({})), 0, &log);
    let harness = harness(vec![], vec![bad, good], log).await;
    let steps = json!([
        tool_step("first", "good", &json!({})),
        tool_step("second", "bad", &json!({})),
        tool_step("third", "good", &json!({})),
    ]);
    define(&harness, steps, json!({ "output": "done" })).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    assert_eq!(run.outcome, RunOutcome::Failed);
    let failure = run.failure.as_deref().unwrap();
    assert!(failure.starts_with("step second:"), "{failure}");
    assert!(failure.contains("it broke"), "{failure}");
    let statuses: Vec<_> = run.steps.iter().map(|step| step.status).collect();
    assert_eq!(
        statuses,
        [StepStatus::Ran, StepStatus::Failed, StepStatus::Skipped]
    );
    assert!(run.steps[1].receipt.is_some(), "the failure is on record");
    assert_eq!(run.output, None);
    assert_eq!(
        harness.log.calls.lock().unwrap().len(),
        2,
        "the third never ran"
    );
}

#[tokio::test]
async fn a_value_that_turns_out_wrong_is_refused_when_the_step_runs() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({ "text": "not a number" })), 0, &log);
    let harness = harness(vec![], vec![fake], log).await;
    let steps = json!([
        tool_step("fetch", "fake", &json!({})),
        tool_step(
            "next",
            "fake",
            &json!({ "count": "{{steps.fetch.result.text}}" })
        ),
    ]);
    define(&harness, steps, json!({})).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    assert_eq!(run.outcome, RunOutcome::Failed);
    assert_eq!(run.steps[1].status, StepStatus::Failed);
    assert_eq!(
        harness.log.calls.lock().unwrap().len(),
        1,
        "it was refused, not run"
    );
}

#[tokio::test]
async fn a_value_that_turns_out_missing_is_named() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({})), 0, &log);
    let harness = harness(vec![], vec![fake], log).await;
    let steps = json!([
        tool_step("fetch", "fake", &json!({})),
        tool_step(
            "next",
            "fake",
            &json!({ "text": "{{steps.fetch.result.text}}" })
        ),
    ]);
    define(&harness, steps, json!({})).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    let failure = run.failure.as_deref().unwrap();
    assert!(
        failure.contains("steps.fetch.result.text is missing"),
        "{failure}"
    );
}

#[tokio::test]
async fn a_result_too_long_to_use_whole_fails_its_step() {
    let log = Arc::new(Recorder::default());
    let huge = "x".repeat(600_000);
    let fake = tool("fake", Ok(json!({ "text": huge })), 0, &log);
    let harness = harness(vec![], vec![fake], log).await;
    define(
        &harness,
        json!([tool_step("fetch", "fake", &json!({}))]),
        json!({}),
    )
    .await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    let failure = run.failure.as_deref().unwrap();
    assert!(failure.contains("longer than"), "{failure}");
}

#[tokio::test]
async fn an_agent_that_doesnt_answer_fails_its_step() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({ "text": "x" })), 0, &log);
    let harness = harness(vec![ChaosStep::Status(500)], vec![fake], log).await;
    let steps = json!([tool_step("fetch", "fake", &json!({})), agent_step()]);
    define(&harness, steps, json!({})).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    assert_eq!(run.outcome, RunOutcome::Failed);
    assert!(run.failure.as_deref().unwrap().starts_with("step think:"));
    assert!(run.steps[1].receipt.is_some(), "what it tried is on record");
}

#[tokio::test]
async fn what_a_step_spends_comes_out_of_what_the_next_can() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({})), 30, &log);
    let harness = harness(vec![], vec![fake], log).await;
    let steps = json!([
        tool_step("one", "fake", &json!({})),
        tool_step("two", "fake", &json!({})),
        tool_step("three", "fake", &json!({})),
    ]);
    define(&harness, steps, json!({ "max_usd_micros": 100 })).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    let left: Vec<u64> = harness
        .log
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| call.1)
        .collect();
    assert_eq!(left, [100, 70, 40]);
    assert_eq!(run.cost_usd_micros, 90);
}

#[tokio::test]
async fn an_answer_cut_off_is_not_passed_on_as_whole() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({ "text": "x" })), 0, &log);
    let script = vec![ChaosStep::CutOff("half a tho".to_owned())];
    let harness = harness(script, vec![fake], log).await;
    let steps = json!([tool_step("fetch", "fake", &json!({})), agent_step()]);
    define(&harness, steps, json!({})).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    let failure = run.failure.as_deref().unwrap();
    assert!(failure.contains("cut off"), "{failure}");
}

#[tokio::test]
async fn the_time_a_step_takes_comes_out_of_the_next() {
    let log = Arc::new(Recorder::default());
    let slow = Arc::new(Fake {
        name: "slow",
        effect: Effect::Read,
        reply: Ok(json!({})),
        cost: 0,
        delay: Duration::from_millis(1400),
        log: Arc::clone(&log),
    });
    let mut harness = harness(vec![], vec![slow], log).await;
    harness.engine.limits.time_limit = Duration::from_millis(2000);
    let steps = json!([
        tool_step("one", "slow", &json!({})),
        tool_step("two", "slow", &json!({})),
    ]);
    define(&harness, steps, json!({})).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    assert!(
        run.failure.as_deref().unwrap().starts_with("step two:"),
        "{run:?}"
    );
}

/// Runs one agent on a priced model, with the agent free to spend, under
/// what the workflow allows.
async fn agent_on_a_priced_model(workflow_money: u64) -> (workflow::WorkflowRun, usize) {
    let log = Arc::new(Recorder::default());
    let script = vec![ChaosStep::Text("paid for".to_owned())];
    let mut harness = harness(script, vec![], log).await;
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10, \
         spend_cap_usd_micros_per_day: 1000000000, paid_for: [workflow]}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], \
         context_tokens: 1000000, max_output_tokens: 100, \
         price: {{input_usd_micros_per_mtok: 1000000, output_usd_micros_per_mtok: 1000000}}}}\n",
        harness.server.base_url
    );
    harness.engine.gateway = Arc::new(Gateway::new(
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        Arc::new(OpenAiClient::new().unwrap()),
        Arc::new(MemoryLedger::default()),
    ));
    harness.engine.limits.agent_budget.max_usd_micros = 1_000_000;
    let step = json!({ "type": "agent", "id": "think", "agent": "helper", "message": "hi" });
    define(
        &harness,
        json!([step]),
        json!({ "max_usd_micros": workflow_money }),
    )
    .await;
    let run = harness.engine.run("flow", subject()).await.unwrap();
    (run, harness.server.served().len())
}

#[tokio::test]
async fn an_agent_cant_spend_what_the_workflow_doesnt_allow() {
    let (allowed, calls) = agent_on_a_priced_model(1_000_000).await;
    assert_eq!(allowed.outcome, RunOutcome::Completed, "{allowed:?}");
    assert_eq!(calls, 1);
    assert!(allowed.cost_usd_micros > 0);

    let (refused, calls) = agent_on_a_priced_model(0).await;
    assert_eq!(refused.outcome, RunOutcome::Failed, "{refused:?}");
    assert_eq!(calls, 0, "the priced model was never called");
}

#[tokio::test]
async fn inputs_that_dont_fit_never_start_a_run() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({})), 0, &log);
    let harness = harness(vec![], vec![fake], log).await;
    define(
        &harness,
        json!([tool_step("fetch", "fake", &json!({}))]),
        json!({}),
    )
    .await;

    let error = harness
        .engine
        .run("flow", json!({ "other": 1 }))
        .await
        .unwrap_err();

    assert!(matches!(error, WorkflowError::Invalid(_)), "{error}");
    assert!(harness.log.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unknown_workflow_is_an_error() {
    let harness = harness(vec![], vec![], Arc::new(Recorder::default())).await;
    let error = harness.engine.run("nope", json!({})).await.unwrap_err();
    assert!(matches!(error, WorkflowError::Unknown(_)), "{error}");
}

#[tokio::test]
async fn an_agent_removed_since_loading_fails_its_step() {
    let harness = harness(vec![], vec![], Arc::new(Recorder::default())).await;
    let workflow = json!({
        "inputs": inputs(),
        "steps": [{ "type": "agent", "id": "think", "agent": "gone", "message": "hi" }]
    });
    put(
        &harness.engine.store,
        DefinitionKind::Workflow,
        "flow",
        workflow,
    )
    .await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    assert!(run.failure.as_deref().unwrap().contains("no agent gone"));
}

// Probes.

fn read_tool(name: &'static str, log: &Arc<Recorder>) -> Arc<Fake> {
    Arc::new(Fake {
        name,
        effect: Effect::Read,
        reply: Ok(json!({ "text": "seen" })),
        cost: 0,
        delay: Duration::ZERO,
        log: Arc::clone(log),
    })
}

fn candidate(tools: &[&str]) -> workflow::Agent {
    workflow::Agent {
        description: String::new(),
        system_prompt: "You help, carefully.".to_owned(),
        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        budget: None,
        max_output_tokens: None,
        memory: None,
        skills: Vec::new(),
        guardrails: Vec::new(),
        needs: Vec::new(),
    }
}

#[tokio::test]
async fn a_probe_runs_only_the_reads_it_was_allowed() {
    let log = Arc::new(Recorder::default());
    let script = vec![
        ChaosStep::ToolCalls(vec![
            ("save".to_owned(), r#"{"text":"x"}"#.to_owned()),
            ("look".to_owned(), "{}".to_owned()),
            ("peek".to_owned(), "{}".to_owned()),
        ]),
        ChaosStep::Text("done".to_owned()),
    ];
    let tools = vec![
        tool("save", Ok(json!({})), 0, &log),
        read_tool("look", &log),
        read_tool("peek", &log),
    ];
    let harness = harness(script, tools, Arc::clone(&log)).await;
    let agent = candidate(&["save", "look", "peek"]);
    let reads = ["look".to_owned()].into_iter().collect();
    let probed = harness
        .engine
        .probe_agent("helper", &agent, "Find it.", reads)
        .await
        .unwrap();

    assert_eq!(probed.answer.as_deref(), Some("done"));
    // Only the allowed read reached a tool.
    assert_eq!(log.calls.lock().unwrap().len(), 1);
    let statuses: Vec<_> = probed
        .receipt
        .tool_calls
        .iter()
        .map(|call| (call.tool.as_str(), call.status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("save", dispatch::CallStatus::Stubbed),
            ("look", dispatch::CallStatus::Ran),
            ("peek", dispatch::CallStatus::Stubbed),
        ]
    );
    // A stubbed write still counts, so a probe meets the limits a real run would.
    assert_eq!(probed.receipt.totals.writes, 1);
    assert_eq!(probed.harness, harness.engine.agent_harness(&agent));

    let trajectory = dispatch::read_trajectory(
        harness.engine.sink.as_ref(),
        probed.run.parse().unwrap(),
        100,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(trajectory.agent.as_deref(), Some("probe:helper"));
    assert_eq!(
        trajectory.calls,
        "save stubbed → look ran → peek stubbed → answered"
    );
}

#[tokio::test]
async fn a_candidate_is_checked_before_it_runs() {
    let log = Arc::new(Recorder::default());
    let harness = harness(vec![], vec![], Arc::clone(&log)).await;
    let unknown = harness
        .engine
        .probe_agent(
            "helper",
            &candidate(&["ghost"]),
            "Go.",
            std::collections::BTreeSet::new(),
        )
        .await
        .unwrap_err();
    assert!(unknown.contains("ghost"), "{unknown}");
    let mut blank = candidate(&[]);
    blank.system_prompt = String::new();
    let refused = harness
        .engine
        .probe_agent("helper", &blank, "Go.", std::collections::BTreeSet::new())
        .await
        .unwrap_err();
    assert!(refused.starts_with("the candidate:"), "{refused}");
}

#[tokio::test]
async fn a_harness_id_follows_the_agent_not_the_run() {
    let log = Arc::new(Recorder::default());
    let harness = harness(vec![], vec![], log).await;
    let engine = &harness.engine;
    let base = candidate(&[]);
    let mut reworded = base.clone();
    reworded.system_prompt.push_str(" Cite sources.");
    let mut described = base.clone();
    described.description = "What it's for.".to_owned();
    assert_ne!(engine.agent_harness(&base), engine.agent_harness(&reworded));
    // A description is for people; it doesn't change how the agent behaves.
    assert_eq!(
        engine.agent_harness(&base),
        engine.agent_harness(&described)
    ); // An agent without skills or guardrails keeps the id it had before
    // agents could have them.
    let before = dispatch::harness_id(&json!({
        "system_prompt": base.system_prompt,
        "tools": base.tools,
        "budget": engine.limits.agent_budget,
        "max_output_tokens": engine.limits.agent_output_tokens,
        "result_chars": engine.limits.agent_result_chars,
    }));
    assert_eq!(engine.agent_harness(&base), before);
    let mut skilled = base.clone();
    skilled.skills.push(skill());
    assert_ne!(engine.agent_harness(&base), engine.agent_harness(&skilled));
}

fn skill() -> workflow::Skill {
    serde_json::from_value(json!({
        "name": "finding", "when": "^find", "unless": "nothing",
        "instructions": "Look in two places."
    }))
    .unwrap()
}

#[tokio::test]
async fn an_agent_gets_the_skills_its_task_matches_and_is_held_to_its_guardrails() {
    let log = Arc::new(Recorder::default());
    let script = vec![
        ChaosStep::ToolCalls(vec![
            ("look".to_owned(), r#"{"text":"secret"}"#.to_owned()),
            ("look".to_owned(), r#"{"text":"open"}"#.to_owned()),
        ]),
        ChaosStep::Text("done".to_owned()),
    ];
    let harness = harness(script, vec![read_tool("look", &log)], Arc::clone(&log)).await;
    let mut agent = candidate(&["look"]);
    agent.skills.push(skill());
    agent.guardrails.push(
        serde_json::from_value(json!({
            "name": "no_secrets", "tool": "look", "because": "Secrets stay put.",
            "deny": { "argument": "text", "matches": "secret" }
        }))
        .unwrap(),
    );
    let reads = ["look".to_owned()].into_iter().collect();
    let probed = harness
        .engine
        .probe_agent("helper", &agent, "Find it.", reads)
        .await
        .unwrap();

    assert_eq!(probed.answer.as_deref(), Some("done"));
    let guarded: Vec<_> = probed
        .receipt
        .tool_calls
        .iter()
        .map(|call| (call.status, call.guardrail.as_deref()))
        .collect();
    assert_eq!(
        guarded,
        [
            (dispatch::CallStatus::Refused, Some("no_secrets")),
            (dispatch::CallStatus::Ran, None),
        ]
    );
    assert_eq!(log.calls.lock().unwrap().len(), 1);
    let trajectory = dispatch::read_trajectory(
        harness.engine.sink.as_ref(),
        probed.run.parse().unwrap(),
        100,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        &trajectory.steps[0].step,
        dispatch::Step::Started { skills, .. } if skills == &["finding"]
    ));
    assert!(matches!(
        &trajectory.steps[1].step,
        dispatch::Step::Message { content, .. }
            if content.ends_with("Skill finding:\nLook in two places.")
    ));
}

#[tokio::test]
async fn a_skill_the_task_doesnt_match_is_left_out() {
    let log = Arc::new(Recorder::default());
    let script = vec![ChaosStep::Text("done".to_owned())];
    let harness = harness(script, vec![], log).await;
    let mut agent = candidate(&[]);
    agent.skills.push(skill());
    let probed = harness
        .engine
        .probe_agent(
            "helper",
            &agent,
            "Find nothing.",
            std::collections::BTreeSet::new(),
        )
        .await
        .unwrap();
    let trajectory = dispatch::read_trajectory(
        harness.engine.sink.as_ref(),
        probed.run.parse().unwrap(),
        100,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        &trajectory.steps[0].step,
        dispatch::Step::Started { skills, .. } if skills.is_empty()
    ));
    assert!(matches!(
        &trajectory.steps[1].step,
        dispatch::Step::Message { content, .. } if content == "You help, carefully."
    ));
}

/// Fails on the text "bad", and echoes anything else.
#[derive(Debug)]
struct Picky;

#[async_trait]
impl Tool for Picky {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "picky".to_owned(),
            "fails on bad".to_owned(),
            open_object(),
            open_object(),
            Effect::Read,
        )
    }

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let text = arguments["text"].as_str().unwrap_or_default().to_owned();
        if text == "bad" {
            return Err(ToolFailure::new("that one was bad".to_owned()));
        }
        Ok(ToolOutput::new(json!({ "text": text }), Trust::Untrusted).with_cost(5))
    }
}

fn list_inputs() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["links"],
        "properties": { "links": { "type": "array", "maxItems": 10,
                                   "items": { "type": "string", "maxLength": 10 } } }
    })
}

fn per_link(tool: &str, max_items: u32, continue_on_failure: bool) -> Value {
    json!({
        "type": "tool", "id": "read", "tool": tool, "arguments": { "text": "{{item}}" },
        "for_each": { "items": "{{inputs.links}}", "max_items": max_items,
                      "continue_on_failure": continue_on_failure }
    })
}

async fn picky_harness() -> Harness {
    let log = Arc::new(Recorder::default());
    let mut harness = harness(vec![], vec![], log).await;
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Picky)).unwrap();
    harness.engine.registry = Arc::new(registry);
    harness
}

#[tokio::test]
async fn a_step_runs_once_per_item_and_says_where_it_cut() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({ "text": "read" })), 10, &log);
    let harness = harness(vec![], vec![fake], log).await;
    let steps = json!([
        per_link("fake", 2, false),
        tool_step(
            "count",
            "fake",
            &json!({ "count": "{{steps.read.fan_out.ran}}" })
        ),
    ]);
    let output = "{{steps.read.result}} cut={{steps.read.fan_out.cut}}";
    define(
        &harness,
        steps,
        json!({ "inputs": list_inputs(), "max_usd_micros": 100, "output": output }),
    )
    .await;

    let run = harness
        .engine
        .run("flow", json!({ "links": ["a", "b", "c"] }))
        .await
        .unwrap();

    assert_eq!(run.outcome, RunOutcome::Completed, "{run:?}");
    let calls = harness.log.calls.lock().unwrap().clone();
    let given: Vec<&Value> = calls.iter().map(|call| &call.0).collect();
    assert_eq!(
        given,
        [
            &json!({ "text": "a" }),
            &json!({ "text": "b" }),
            &json!({ "count": 2 })
        ]
    );
    let left: Vec<u64> = calls.iter().map(|call| call.1).collect();
    assert_eq!(left, [100, 90, 80], "each item spends from what's left");
    assert_eq!(run.cost_usd_micros, 30);
    let read = &run.steps[0];
    assert_eq!(read.receipt, None, "each item has its own");
    assert_eq!(read.items.len(), 2);
    assert!(read.items.iter().all(|item| item.receipt.is_some()));
    assert_eq!(
        run.output,
        Some(json!(r#"[{"text":"read"},{"text":"read"}] cut=true"#))
    );
}

#[tokio::test]
async fn a_failing_item_stops_the_step_unless_told_otherwise() {
    let harness = picky_harness().await;
    define(
        &harness,
        json!([per_link("picky", 10, false)]),
        json!({ "inputs": list_inputs(), "max_usd_micros": 100 }),
    )
    .await;
    let links = json!({ "links": ["a", "bad", "c"] });

    let stopped = harness.engine.run("flow", links.clone()).await.unwrap();

    assert_eq!(stopped.outcome, RunOutcome::Failed);
    let failure = stopped.failure.as_deref().unwrap();
    assert!(failure.starts_with("step read: item 1:"), "{failure}");
    assert!(failure.contains("that one was bad"), "{failure}");
    let statuses: Vec<_> = stopped.steps[0]
        .items
        .iter()
        .map(|item| item.status)
        .collect();
    assert_eq!(statuses, [StepStatus::Ran, StepStatus::Failed]);
    assert_eq!(stopped.cost_usd_micros, 5);

    define(
        &harness,
        json!([per_link("picky", 10, true)]),
        json!({
            "inputs": list_inputs(), "max_usd_micros": 100,
            "output": "{{steps.read.result}} failed={{steps.read.fan_out.failed}}"
        }),
    )
    .await;

    let went_on = harness.engine.run("flow", links).await.unwrap();

    assert_eq!(went_on.outcome, RunOutcome::Completed, "{went_on:?}");
    assert_eq!(went_on.steps[0].items.len(), 3);
    assert_eq!(went_on.cost_usd_micros, 10);
    assert_eq!(
        went_on.output,
        Some(json!(r#"[{"text":"a"},null,{"text":"c"}] failed=1"#))
    );
}

#[tokio::test]
async fn items_that_arent_a_list_fail_the_step() {
    let log = Arc::new(Recorder::default());
    let fake = tool("fake", Ok(json!({ "text": "one" })), 0, &log);
    let harness = harness(vec![], vec![fake], log).await;
    let steps = json!([
        tool_step("find", "fake", &json!({})),
        {
            "type": "tool", "id": "read", "tool": "fake", "arguments": { "text": "{{item}}" },
            "for_each": { "items": "{{steps.find.result.text}}", "max_items": 5 }
        }
    ]);
    define(&harness, steps, json!({})).await;

    let run = harness.engine.run("flow", subject()).await.unwrap();

    let failure = run.failure.as_deref().unwrap();
    assert!(failure.contains("a single value, not a list"), "{failure}");
    assert_eq!(harness.log.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_agent_gets_each_item_as_context() {
    let log = Arc::new(Recorder::default());
    let script = vec![
        ChaosStep::Text("first".to_owned()),
        ChaosStep::Text("second".to_owned()),
    ];
    let harness = harness(script, vec![], log).await;
    let step = json!({
        "type": "agent", "id": "each", "agent": "helper", "message": "Say one thing",
        "context": "{{item}}", "for_each": { "items": "{{inputs.links}}", "max_items": 5 }
    });
    define(
        &harness,
        json!([step]),
        json!({ "inputs": list_inputs(), "output": "{{steps.each.answer}}" }),
    )
    .await;

    let run = harness
        .engine
        .run("flow", json!({ "links": ["x", "y"] }))
        .await
        .unwrap();

    assert_eq!(run.outcome, RunOutcome::Completed, "{run:?}");
    assert_eq!(harness.server.served().len(), 2);
    assert_eq!(run.output, Some(json!(["first", "second"])));
}

// Probes of a whole workflow.

#[tokio::test]
async fn a_workflow_probe_stubs_writes_runs_allowed_reads_and_probes_its_agents() {
    let log = Arc::new(Recorder::default());
    let script = vec![ChaosStep::Text("summed".to_owned())];
    let tools = vec![
        read_tool("look", &log),
        tool("save", Ok(json!({})), 0, &log),
    ];
    let harness = harness(script, tools, Arc::clone(&log)).await;
    // `define` stores the agent the steps use.
    define(&harness, json!([agent_step()]), json!({})).await;
    let candidate: workflow::Workflow = serde_json::from_value(json!({
        "inputs": { "type": "object", "additionalProperties": false, "properties": {} },
        "steps": [
            tool_step("find", "look", &json!({})),
            tool_step("keep", "save", &json!({ "text": "x" })),
            { "type": "agent", "id": "sum", "agent": "helper", "message": "Sum it up.",
              "context": "{{steps.keep.result}}" }
        ],
        "output": "{{steps.sum.answer}}"
    }))
    .unwrap();
    let reads = ["look".to_owned()].into_iter().collect();
    let probed = harness
        .engine
        .probe_workflow("flow", &candidate, &json!({}), reads)
        .await
        .unwrap();

    let run = &probed.run;
    assert_eq!(run.outcome, RunOutcome::Completed, "{run:?}");
    assert_eq!(run.output, Some(json!("summed")));
    // Only the allowed read reached a tool; the write was stubbed, and its
    // output is the stub.
    assert_eq!(log.calls.lock().unwrap().len(), 1);
    let keep = run.steps[1].receipt.as_ref().unwrap();
    assert_eq!(keep.tool_calls[0].status, dispatch::CallStatus::Stubbed);
    assert_eq!(probed.harness, harness.engine.workflow_harness(&candidate));
    let mut subjects = Vec::new();
    for step in &run.steps {
        let receipt = step.receipt.as_ref().unwrap();
        let trajectory = dispatch::read_trajectory(
            harness.engine.sink.as_ref(),
            receipt.run.parse().unwrap(),
            100,
        )
        .await
        .unwrap()
        .unwrap();
        subjects.push(trajectory.agent.unwrap());
    }
    assert_eq!(
        subjects,
        ["probe:tool:look", "probe:tool:save", "probe:helper"]
    );
}

#[tokio::test]
async fn a_workflow_probe_checks_the_candidate_and_its_inputs() {
    let log = Arc::new(Recorder::default());
    let harness = harness(vec![], vec![], log).await;
    define(&harness, json!([agent_step()]), json!({})).await;
    let asking = |agent: &str| -> workflow::Workflow {
        serde_json::from_value(json!({
            "inputs": { "type": "object", "additionalProperties": false, "properties": {} },
            "steps": [{ "type": "agent", "id": "a", "agent": agent, "message": "Hi." }]
        }))
        .unwrap()
    };
    let none = std::collections::BTreeSet::new();
    let unknown = harness
        .engine
        .probe_workflow("flow", &asking("nobody"), &json!({}), none.clone())
        .await
        .unwrap_err();
    assert!(unknown.contains("nobody"), "{unknown}");
    let wrong = harness
        .engine
        .probe_workflow("flow", &asking("helper"), &json!({ "extra": 1 }), none)
        .await
        .unwrap_err();
    assert!(wrong.starts_with("the inputs"), "{wrong}");
}

/// Says how many tool calls its run has left: what a plugin's capability
/// calls are held to.
#[derive(Debug)]
struct CallsLeft;

#[async_trait]
impl Tool for CallsLeft {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "calls_left".to_owned(),
            "says how many calls are left".to_owned(),
            open_object(),
            open_object(),
            Effect::Read,
        )
    }

    async fn run(&self, _: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        Ok(ToolOutput::new(
            json!({ "count": context.tool_calls_left }),
            Trust::Untrusted,
        ))
    }
}

#[tokio::test]
async fn a_tool_step_allows_its_tool_the_capability_calls_it_says() {
    let log = Arc::new(Recorder::default());
    let mut harness = harness(vec![], vec![], log).await;
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(CallsLeft)).unwrap();
    harness.engine.registry = Arc::new(registry);
    let steps = json!([
        tool_step("plain", "calls_left", &json!({})),
        { "type": "tool", "id": "many", "tool": "calls_left", "arguments": {},
          "capability_calls": 40 },
        { "type": "tool", "id": "each", "tool": "calls_left", "arguments": {},
          "capability_calls": 3,
          "for_each": { "items": "{{inputs.links}}", "max_items": 2 } },
    ]);
    let output = "{{steps.plain.result.count}} {{steps.many.result.count}} {{steps.each.result}}";
    define(
        &harness,
        steps,
        json!({ "inputs": list_inputs(), "output": output }),
    )
    .await;

    let run = harness
        .engine
        .run("flow", json!({ "links": ["a", "b"] }))
        .await
        .unwrap();

    assert_eq!(run.outcome, RunOutcome::Completed, "{run:?}");
    // The call itself is one; each item of a step gets the step's allowance.
    assert_eq!(run.output, Some(json!(r#"1 40 [{"count":3},{"count":3}]"#)));
}

#[tokio::test]
async fn a_tool_steps_capability_calls_are_bounded() {
    for calls in [0, 201] {
        let steps = json!([{ "type": "tool", "id": "a", "tool": "fake", "arguments": {},
                             "capability_calls": calls }]);
        let definition = json!({ "inputs": inputs(), "steps": steps });
        let checked = schema::Validator::for_type::<workflow::Workflow>()
            .unwrap()
            .parse::<workflow::Workflow>(definition);
        assert!(checked.is_err(), "{calls} is out of bounds");
    }
}
