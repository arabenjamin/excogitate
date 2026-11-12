//! The loop end to end, with one scripted model playing the agent and the
//! judges, real dispatch, and a read tool that records when it runs.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chaos_model::{ChaosServer, ChaosStep};
use chrono::Utc;
use dispatch::{Budget, ToolRegistry};
use events::MemorySink;
use harness::{Attempt, Judges, Loop, Request, Settings, Status};
use model::{Catalog, Gateway, MemoryLedger, OpenAiClient};
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::Trust;
use serde_json::{Value, json};
use store::{Definition, DefinitionKey, DefinitionKind, Store, UnitOfWork, WriteOp};
use store_memory::MemoryStore;
use workflow::{Engine, EngineLimits};

const TASK: &str = "What is the answer to the question we discussed?";

#[derive(Debug, Default)]
struct Look(AtomicUsize);

#[async_trait]
impl Tool for Look {
    fn definition(&self) -> ToolDefinition {
        let object = json!({ "type": "object", "additionalProperties": false, "properties": {} });
        ToolDefinition::new(
            "look".to_owned(),
            "Looks it up.".to_owned(),
            object.clone(),
            json!({
                "type": "object", "additionalProperties": false,
                "properties": { "found": { "type": "string", "maxLength": 100 } }
            }),
            Effect::Read,
        )
    }

    async fn run(&self, _: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::new(json!({ "found": "42" }), Trust::Untrusted))
    }
}

const BUDGET: Budget = Budget {
    max_rounds: 4,
    max_tool_calls: 4,
    max_writes: 0,
    max_total_tokens: 100_000,
    max_usd_micros: 0,
    max_request_bytes: 600_000,
    max_duration_ms: 20_000,
};

struct Setup {
    engine: Engine,
    _server: ChaosServer,
    look: Arc<Look>,
}

async fn setup(script: Vec<ChaosStep>) -> Setup {
    setup_agent(
        json!({ "system_prompt": "You help.", "tools": ["look"] }),
        script,
    )
    .await
}

async fn setup_agent(body: Value, script: Vec<ChaosStep>) -> Setup {
    let server = ChaosServer::start(script).await.unwrap();
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools, investigative], context_tokens: 1000000, max_output_tokens: 4000}}\n",
        server.base_url
    );
    let gateway = Gateway::new(
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        Arc::new(OpenAiClient::new().unwrap()),
        Arc::new(MemoryLedger::default()),
    );
    let look = Arc::new(Look::default());
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::clone(&look) as Arc<dyn Tool>)
        .unwrap();
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new(3));
    let agent = Definition {
        key: DefinitionKey {
            kind: DefinitionKind::Agent,
            name: "helper".to_owned(),
        },
        body,
        source: "test".to_owned(),
        updated_at: Utc::now(),
    };
    store
        .apply(UnitOfWork::new().with(WriteOp::PutDefinition(agent)))
        .await
        .unwrap();
    let engine = Engine {
        registry: Arc::new(registry),
        gateway: Arc::new(gateway),
        sink: Arc::new(MemorySink::new()),
        store,
        limits: EngineLimits {
            agent_budget: BUDGET,
            agent_result_chars: 1000,
            agent_output_tokens: 200,
            time_limit: Duration::from_secs(20),
        },
        stop: dispatch::StopSignal::new(),
        sources: workflow::SourceChoices::default(),
    };
    Setup {
        engine,
        _server: server,
        look,
    }
}

async fn attempt(setup: &Setup) -> Attempt {
    let settings = Settings {
        reads: ["look".to_owned()].into_iter().collect(),
        judge_usd_micros: 0,
    };
    let looped = Loop {
        engine: &setup.engine,
        judges: Judges {
            gateway: &setup.engine.gateway,
            max_output_tokens: 4000,
        },
        settings: &settings,
    };
    looped
        .improve(Request {
            agent: "helper".to_owned(),
            target: harness::Target::Agent,
            task: TASK.to_owned(),
            inputs: None,
            why: Some("It gave up without looking.".to_owned()),
        })
        .await
}

fn text(value: &Value) -> ChaosStep {
    ChaosStep::Text(value.to_string())
}

fn say(words: &str) -> ChaosStep {
    ChaosStep::Text(words.to_owned())
}

fn look() -> ChaosStep {
    ChaosStep::ToolCalls(vec![("look".to_owned(), "{}".to_owned())])
}

const LOOK_FIRST: &str = "You help. Look things up before you answer.";

/// The reference gives up: steps R0 started, R1 instructions, R2 task, R3 the
/// model's reply, R4 the end.
fn cite_reference() -> Value {
    json!([{ "run": "R", "step": 3, "quote": "I don't know" }])
}

fn editor(invariant: &str, cites: &Value, domain: &str) -> ChaosStep {
    text(&json!({
        "invariant": { "system_prompt": invariant, "change": "Look first.", "cites": cites },
        "domain": { "system_prompt": domain, "change": "Nothing.", "cites": [] }
    }))
}

/// A candidate that looks first has steps X0 to X3 as given, X3 the call, X4
/// the look, X5 the answer, X6 the end.
fn composer(verdicts: &Value, decision: &str, choose: Option<&str>) -> ChaosStep {
    text(&json!({
        "verdicts": verdicts, "decision": decision, "choose": choose,
        "merged": null, "reason": "Run A looked it up and answered."
    }))
}

fn mitigated(run: &str) -> Value {
    json!({ "candidate": run, "effect": "mitigated",
            "cite": { "run": run, "step": 5, "quote": "The answer is 42." } })
}

#[tokio::test]
async fn an_edit_the_runs_support_is_supported() {
    let setup = setup(vec![
        say("I don't know."),
        editor(LOOK_FIRST, &cite_reference(), "You help."),
        look(),
        say("The answer is 42."),
        composer(&json!([mitigated("A")]), "adopt", Some("A")),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Supported, "{attempt:#?}");
    assert_eq!(attempt.chosen.as_deref(), Some("A"));
    assert_eq!(attempt.supported().unwrap().system_prompt, LOOK_FIRST);
    // The domain candidate changed nothing, so it wasn't run.
    assert_eq!(
        attempt.candidates[1].refused.as_deref(),
        Some("it changes nothing")
    );
    assert_eq!(attempt.reference.as_ref().unwrap().calls, "answered");
    assert_eq!(
        attempt.candidates[0].probe.as_ref().unwrap().calls,
        "look ran → answered"
    );
    assert_eq!(setup.look.0.load(Ordering::SeqCst), 1);
    assert_ne!(
        attempt.candidates[0].probe.as_ref().unwrap().harness,
        attempt.base_harness.clone().unwrap()
    );
    let schema = schema::schema_of::<Attempt>();
    assert_eq!(schema::check_strict(&schema), vec![]);
    schema::Validator::from_schema(&schema)
        .unwrap()
        .check(&serde_json::to_value(&attempt).unwrap())
        .unwrap();
}

#[tokio::test]
async fn a_quote_that_isnt_in_the_run_counts_for_nothing() {
    let made_up = json!([{ "candidate": "A", "effect": "mitigated",
        "cite": { "run": "A", "step": 5, "quote": "It checked three sources." } }]);
    let setup = setup(vec![
        say("I don't know."),
        editor(LOOK_FIRST, &cite_reference(), "You help."),
        look(),
        say("The answer is 42."),
        composer(&made_up, "adopt", Some("A")),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Kept);
    assert!(
        attempt.verdicts[0]
            .refused
            .as_deref()
            .unwrap()
            .contains("doesn't say that")
    );
}

#[tokio::test]
async fn a_run_that_ends_worse_is_a_regression_whatever_the_judge_says() {
    let setup = setup(vec![
        say("I don't know."),
        editor(LOOK_FIRST, &cite_reference(), "You help."),
        ChaosStep::Status(500),
        composer(
            &json!([{ "candidate": "A", "effect": "mitigated",
                      "cite": { "run": "A", "step": 3, "quote": "model call failed" } }]),
            "adopt",
            Some("A"),
        ),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Kept, "{attempt:#?}");
    assert_eq!(
        attempt.verdicts[0].overridden,
        Some(harness::Effect::Regressed)
    );
}

#[tokio::test]
async fn a_candidate_without_evidence_from_the_reference_is_never_run() {
    let instructions = json!([{ "run": "R", "step": 1, "quote": "instructions: You help." }]);
    let setup = setup(vec![
        say("I don't know."),
        editor(LOOK_FIRST, &instructions, "You help. Be thorough."),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Kept);
    assert!(
        attempt.candidates[0]
            .refused
            .as_deref()
            .unwrap()
            .contains("not what it did"),
        "{attempt:#?}"
    );
    assert_eq!(
        attempt.candidates[1].refused.as_deref(),
        Some("it cites no step of run R")
    );
    assert!(attempt.candidates.iter().all(|c| c.probe.is_none()));
}

#[tokio::test]
async fn the_editor_pasting_the_task_in_is_refused() {
    let pasted = format!("You help.\n{TASK}");
    let setup = setup(vec![
        say("I don't know."),
        editor(&pasted, &cite_reference(), "You help."),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(
        attempt.candidates[0].refused.as_deref(),
        Some("the instructions contain the task they were tested on")
    );
}

#[tokio::test]
async fn a_merge_is_run_and_judged_on_its_own() {
    let both = text(&json!({
        "invariant": { "system_prompt": LOOK_FIRST, "change": "Look first.", "cites": cite_reference() },
        "domain": { "system_prompt": "You help. The answers are in look.", "change": "Where answers are.", "cites": cite_reference() }
    }));
    let merged = "You help. The answers are in look, so look before you answer.";
    let setup = setup(vec![
        say("I don't know."),
        both,
        look(),
        say("The answer is 42."),
        look(),
        say("The answer is 42."),
        text(&json!({
            "verdicts": [mitigated("A"), mitigated("B")], "decision": "merge", "choose": null,
            "merged": merged, "reason": "Each found the answer."
        })),
        look(),
        say("The answer is 42."),
        composer(&json!([mitigated("M")]), "adopt", Some("M")),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Supported, "{attempt:#?}");
    assert_eq!(attempt.supported().unwrap().system_prompt, merged);
    assert_eq!(attempt.candidates.len(), 3);
}

#[tokio::test]
async fn an_agent_that_doesnt_exist_fails_and_says_so() {
    let setup = setup(vec![]).await;
    let settings = Settings {
        reads: std::collections::BTreeSet::new(),
        judge_usd_micros: 0,
    };
    let looped = Loop {
        engine: &setup.engine,
        judges: Judges {
            gateway: &setup.engine.gateway,
            max_output_tokens: 4000,
        },
        settings: &settings,
    };
    let attempt = looped
        .improve(Request {
            agent: "ghost".to_owned(),
            target: harness::Target::Agent,
            task: TASK.to_owned(),
            inputs: None,
            why: None,
        })
        .await;
    assert_eq!(attempt.status, Status::Failed);
    assert_eq!(attempt.problems, ["there is no agent ghost"]);
}

#[tokio::test]
async fn the_record_type_registers_and_an_attempt_meets_it() {
    let setup = setup(vec![]).await;
    setup
        .engine
        .store
        .register_records(&harness::declarations())
        .await
        .unwrap();
    let settings = Settings {
        reads: std::collections::BTreeSet::new(),
        judge_usd_micros: 0,
    };
    let looped = Loop {
        engine: &setup.engine,
        judges: Judges {
            gateway: &setup.engine.gateway,
            max_output_tokens: 4000,
        },
        settings: &settings,
    };
    let attempt = looped
        .improve(Request {
            agent: "ghost".to_owned(),
            target: harness::Target::Agent,
            task: TASK.to_owned(),
            inputs: None,
            why: None,
        })
        .await;
    let id = store::RecordId::generate();
    let record = harness::record_of(id, &attempt).unwrap();
    setup
        .engine
        .store
        .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
        .await
        .unwrap();
    let kept = setup
        .engine
        .store
        .get_record(harness::ATTEMPT_TYPE, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.data["status"], "failed");
}

#[tokio::test]
async fn a_merge_of_a_candidate_that_mitigated_nothing_is_refused() {
    let both = text(&json!({
        "invariant": { "system_prompt": LOOK_FIRST, "change": "Look first.", "cites": cite_reference() },
        "domain": { "system_prompt": "You help. Be polite.", "change": "Manners.", "cites": cite_reference() }
    }));
    let unchanged = json!({ "candidate": "B", "effect": "unchanged",
        "cite": { "run": "B", "step": 3, "quote": "I don't know" } });
    let setup = setup(vec![
        say("I don't know."),
        both,
        look(),
        say("The answer is 42."),
        say("I don't know."),
        text(&json!({
            "verdicts": [mitigated("A"), unchanged], "decision": "merge", "choose": null,
            "merged": "You help. Be polite, and look before you answer.", "reason": "Both."
        })),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Kept, "{attempt:#?}");
    assert!(
        attempt
            .problems
            .iter()
            .any(|p| p.contains("a merge needs two"))
    );
    // The merge never ran.
    assert_eq!(attempt.candidates.len(), 2);
}

// Calibration and the adoption rules.

fn verdict(effect: &str, step: u32, quote: &str) -> Value {
    json!([{ "candidate": "A", "effect": effect,
             "cite": { "run": "A", "step": step, "quote": quote } }])
}

fn calibration_script(broke_effect: &str) -> Vec<ChaosStep> {
    vec![
        composer(
            &verdict("mitigated", 3, "Six times seven is 42."),
            "adopt",
            Some("A"),
        ),
        composer(
            &verdict(broke_effect, 3, "the model's reply was empty"),
            "keep",
            None,
        ),
        composer(
            &verdict("unchanged", 3, "Six times seven is 42."),
            "keep",
            None,
        ),
    ]
}

async fn calibrate_on(setup: &Setup, model: &str) -> harness::Calibration {
    Judges {
        gateway: &setup.engine.gateway,
        max_output_tokens: 4000,
    }
    .calibrate(model, 0)
    .await
}

async fn calibrate(setup: &Setup) -> harness::Calibration {
    calibrate_on(setup, "c").await
}

#[tokio::test]
async fn a_judge_that_gets_the_known_answers_right_passes() {
    let setup = setup(calibration_script("regressed")).await;
    let calibration = calibrate(&setup).await;
    assert!(calibration.passed, "{calibration:#?}");
    assert_eq!(calibration.model, "c");
    assert_eq!(calibration.cases.len(), 3);
}

#[tokio::test]
async fn the_test_is_taken_by_the_composers_model_or_not_at_all() {
    // Pinned to a model that can't be reached, no other model stands in:
    // every case fails to route, and nothing passes for the model.
    let setup = setup(calibration_script("regressed")).await;
    let calibration = calibrate_on(&setup, "elsewhere").await;
    assert!(!calibration.passed);
    assert_eq!(calibration.model, "");
    assert!(
        calibration.cases.iter().all(|case| case
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("pinned to another model"))),
        "{calibration:#?}"
    );
}

#[tokio::test]
async fn a_judge_that_calls_a_broken_run_better_fails() {
    let setup = setup(calibration_script("mitigated")).await;
    let calibration = calibrate(&setup).await;
    assert!(!calibration.passed);
    let broke = &calibration.cases[1];
    assert_eq!(broke.case, "broke");
    assert!(
        broke.detail.as_deref().unwrap().contains("Regressed"),
        "{broke:?}"
    );
}

async fn supported() -> Attempt {
    let setup = setup(vec![
        say("I don't know."),
        editor(LOOK_FIRST, &cite_reference(), "You help."),
        look(),
        say("The answer is 42."),
        composer(&json!([mitigated("A")]), "adopt", Some("A")),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Supported);
    attempt
}

#[tokio::test]
async fn only_a_supported_edit_that_meets_every_rule_is_adopted() {
    let attempt = supported().await;
    let now = Utc::now();
    let base = attempt.base_harness.clone().unwrap();
    let candidate = attempt.supported().unwrap().probe.clone().unwrap().harness;
    let none = std::collections::BTreeSet::new();
    let facts = harness::Facts {
        attempt: &attempt,
        current_harness: &base,
        hold: None,
        reverted: &none,
        last_change: None,
        calibrated_at: Some(now),
        now,
    };
    assert_eq!(harness::check_adoption(&facts), Ok(()));

    let hold = harness::Hold {
        agent: "helper".to_owned(),
        target: harness::Target::Agent,
        since: now,
        reason: "the owner reverted a change".to_owned(),
    };
    let reverted: std::collections::BTreeSet<String> = [candidate].into_iter().collect();
    let refused = [
        (
            harness::Facts {
                hold: Some(&hold),
                ..facts.clone()
            },
            "held",
        ),
        (
            harness::Facts {
                current_harness: "edited",
                ..facts.clone()
            },
            "changed while",
        ),
        (
            harness::Facts {
                reverted: &reverted,
                ..facts.clone()
            },
            "reverted this harness",
        ),
        (
            harness::Facts {
                last_change: Some(now - chrono::Duration::hours(2)),
                ..facts.clone()
            },
            "less than 24 hours",
        ),
        (
            harness::Facts {
                calibrated_at: None,
                ..facts.clone()
            },
            "known-answer",
        ),
        (
            harness::Facts {
                calibrated_at: Some(now - chrono::Duration::days(8)),
                ..facts.clone()
            },
            "known-answer",
        ),
    ];
    for (facts, why) in refused {
        let error = harness::check_adoption(&facts).unwrap_err();
        assert!(error.contains(why), "{error}");
    }
    let mut kept = attempt.clone();
    kept.status = Status::Kept;
    let error = harness::check_adoption(&harness::Facts {
        attempt: &kept,
        ..facts
    })
    .unwrap_err();
    assert!(error.contains("supports no change"), "{error}");
}

#[tokio::test]
async fn a_citation_that_doesnt_hold_is_dropped_and_the_rest_still_count() {
    let mixed = json!([
        { "run": "R", "step": 2, "quote": "What is the answer" },
        { "run": "R3", "step": 3, "quote": "I don't know" }
    ]);
    let setup = setup(vec![
        say("I don't know."),
        editor(LOOK_FIRST, &mixed, "You help."),
        look(),
        say("The answer is 42."),
        composer(&json!([mitigated("A")]), "adopt", Some("A")),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Supported, "{attempt:#?}");
    let tried = &attempt.candidates[0];
    assert_eq!(tried.cites.len(), 1);
    assert_eq!(tried.dropped.len(), 1);
    assert!(tried.dropped[0].contains("not what it did"), "{tried:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_editor_is_shown_the_active_lessons_that_fit() {
    let setup = setup(vec![
        say("I don't know."),
        editor(LOOK_FIRST, &cite_reference(), "You help."),
        look(),
        say("The answer is 42."),
        composer(&json!([mitigated("A")]), "adopt", Some("A")),
    ])
    .await;
    let store = setup.engine.store.as_ref();
    store
        .register_records(&harness::declarations())
        .await
        .unwrap();
    let lesson = |agent: &str, change: &str, situation: &str, status: harness::LessonStatus| {
        harness::Lesson {
            agent: agent.to_owned(),
            target: harness::Target::Agent,
            change_id: change.to_owned(),
            at: Utc::now(),
            situation: situation.to_owned(),
            lesson: "Look before answering a question".to_owned(),
            evidence: Vec::new(),
            status,
        }
    };
    let active = harness::LessonStatus::Active;
    for (agent, change, situation, status) in [
        ("helper", "fits", "The answer to a question", active),
        (
            "helper",
            "gone",
            "The answer to a question",
            harness::LessonStatus::Retired,
        ),
        ("other", "elsewhere", "Sum the invoices", active),
    ] {
        let record = harness::record(
            harness::LESSON_TYPE,
            store::RecordId::generate(),
            &lesson(agent, change, situation, status),
            Utc::now(),
        )
        .unwrap();
        store
            .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
            .await
            .unwrap();
    }
    let attempt = attempt(&setup).await;
    // The agent's own lesson first; another agent's that shares the task's
    // words after it; a retired one never.
    assert_eq!(attempt.lessons_shown, ["fits", "elsewhere"]);
}

fn look_first_rule() -> Value {
    json!({ "name": "look_first", "tool": "look", "because": "The answers are in look.",
            "required": true })
}

#[tokio::test]
async fn an_edit_may_add_a_guardrail_and_the_probe_runs_under_it() {
    let draft = text(&json!({
        "invariant": { "system_prompt": "You help.", "change": "Make it look first.",
                       "cites": cite_reference(), "guardrails": [look_first_rule()] },
        "domain": { "system_prompt": "You help.", "change": "Nothing.", "cites": [] }
    }));
    // A0 started, A1 instructions, A2 task, A3 the guess, A4 the reminder,
    // A5 the call, A6 the look, A7 the answer, A8 the end.
    let setup = setup(vec![
        say("I don't know."),
        draft,
        say("I don't know."),
        look(),
        say("The answer is 42."),
        composer(
            &json!([{ "candidate": "A", "effect": "mitigated",
                      "cite": { "run": "A", "step": 7, "quote": "The answer is 42." } }]),
            "adopt",
            Some("A"),
        ),
    ])
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Supported, "{attempt:#?}");
    let chosen = attempt.supported().unwrap();
    assert_eq!(chosen.guardrails.len(), 1);
    assert_eq!(chosen.skills, vec![]);
    assert_eq!(setup.look.0.load(Ordering::SeqCst), 1);
    let base = setup.engine.agent("helper").await.unwrap().unwrap();
    let edited = chosen.agent(&base);
    assert_eq!(edited.guardrails[0].name, "look_first");
    assert_ne!(
        setup.engine.agent_harness(&edited),
        setup.engine.agent_harness(&base)
    );
}

#[tokio::test]
async fn an_edit_that_drops_a_guardrail_is_never_run() {
    let body = json!({ "system_prompt": "You help.", "tools": ["look"],
                       "guardrails": [look_first_rule()] });
    let draft = text(&json!({
        "invariant": { "system_prompt": LOOK_FIRST, "change": "Prose instead.",
                       "cites": cite_reference(), "guardrails": [] },
        "domain": { "system_prompt": "You help.", "change": "Nothing.", "cites": [] }
    }));
    // The reference is reminded, looks, and answers.
    let setup = setup_agent(
        body,
        vec![
            say("I don't know."),
            look(),
            say("The answer is 42."),
            draft,
        ],
    )
    .await;
    let attempt = attempt(&setup).await;
    assert_eq!(attempt.status, Status::Kept, "{attempt:#?}");
    assert!(
        attempt.candidates[0]
            .refused
            .as_deref()
            .unwrap()
            .contains("removes or changes the guardrail look_first")
    );
    assert!(attempt.candidates.iter().all(|c| c.probe.is_none()));
}

#[tokio::test]
async fn a_skill_that_wont_compile_is_never_run() {
    let draft = text(&json!({
        "invariant": { "system_prompt": "You help.", "change": "A skill.",
                       "cites": cite_reference(),
                       "skills": [{ "name": "s", "when": "(", "instructions": "Look." }] },
        "domain": { "system_prompt": "You help.", "change": "Nothing.", "cites": [] }
    }));
    let setup = setup(vec![say("I don't know."), draft]).await;
    let attempt = attempt(&setup).await;
    assert!(
        attempt.candidates[0]
            .refused
            .as_deref()
            .unwrap()
            .contains("the skill s"),
        "{attempt:#?}"
    );
}

async fn with_workflow(setup: &Setup) {
    let body = json!({
        "inputs": { "type": "object", "additionalProperties": false,
                    "properties": { "q": { "type": "string", "maxLength": 100 } } },
        "steps": [{ "type": "agent", "id": "answer", "agent": "helper", "message": "Answer it." }]
    });
    let definition = Definition {
        key: DefinitionKey {
            kind: DefinitionKind::Workflow,
            name: "ask".to_owned(),
        },
        body,
        source: "test".to_owned(),
        updated_at: Utc::now(),
    };
    setup
        .engine
        .store
        .apply(UnitOfWork::new().with(WriteOp::PutDefinition(definition)))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_workflows_merge_combines_the_step_edits_and_is_judged_on_its_own() {
    let cite = json!([{ "run": "R", "step": 3, "quote": "I don't know" }]);
    let setup = setup(vec![
        say("I don't know."),
        text(&json!({
            "invariant": { "steps": [{ "id": "answer", "message": "Work it out, then answer." }],
                           "change": "Work it out.", "cites": cite },
            "domain": { "steps": [{ "id": "answer", "context": "Answers are numbers." }],
                        "change": "Say what answers look like.", "cites": cite }
        })),
        say("The answer is 42."),
        say("The answer is 42."),
        text(&json!({
            "verdicts": [mitigated_at("A", 3), mitigated_at("B", 3)], "decision": "merge",
            "choose": null, "merged": null, "reason": "Each found the answer."
        })),
        say("The answer is 42."),
        composer(&json!([mitigated_at("M", 3)]), "adopt", Some("M")),
    ])
    .await;
    with_workflow(&setup).await;
    let settings = Settings {
        reads: ["look".to_owned()].into_iter().collect(),
        judge_usd_micros: 0,
    };
    let looped = Loop {
        engine: &setup.engine,
        judges: Judges {
            gateway: &setup.engine.gateway,
            max_output_tokens: 4000,
        },
        settings: &settings,
    };
    let attempt = looped
        .improve(Request {
            agent: "ask".to_owned(),
            task: String::new(),
            inputs: serde_json::json!({ "q": "six times seven" })
                .as_object()
                .cloned(),
            target: harness::Target::Workflow,
            why: None,
        })
        .await;
    assert_eq!(attempt.status, Status::Supported, "{attempt:#?}");
    let merged = attempt.supported().unwrap();
    assert_eq!(merged.label, "M");
    let edits: Vec<_> = merged
        .steps
        .iter()
        .map(|edit| (edit.message.as_deref(), edit.context.as_deref()))
        .collect();
    // Both edited the one step, each a different part, so the merge has both.
    assert_eq!(
        edits,
        [(
            Some("Work it out, then answer."),
            Some("Answers are numbers.")
        )]
    );
    assert_eq!(
        attempt.reference.as_ref().unwrap().outcome.as_deref(),
        Some("completed")
    );
    assert_eq!(attempt.reference.as_ref().unwrap().runs.len(), 1);
    assert_ne!(
        merged.probe.as_ref().unwrap().harness,
        attempt.base_harness.clone().unwrap()
    );
}

fn mitigated_at(run: &str, step: u32) -> Value {
    json!({ "candidate": run, "effect": "mitigated",
            "cite": { "run": run, "step": step, "quote": "The answer is 42." } })
}

#[tokio::test]
async fn a_fan_out_steps_items_are_read_as_one_run() {
    // The reference probe answers twice, once per item; nothing scripted
    // follows, so the attempt ends there, with the reference read.
    let setup = setup(vec![say("Otters."), say("Badgers.")]).await;
    let body = json!({
        "inputs": { "type": "object", "additionalProperties": false, "required": ["animals"],
                    "properties": { "animals": { "type": "array", "maxItems": 5,
                                     "items": { "type": "string", "maxLength": 20 } } } },
        "steps": [{ "type": "agent", "id": "each", "agent": "helper", "message": "Describe it.",
                    "context": "{{item}}",
                    "for_each": { "items": "{{inputs.animals}}", "max_items": 5 } }]
    });
    let definition = Definition {
        key: DefinitionKey {
            kind: DefinitionKind::Workflow,
            name: "animals".to_owned(),
        },
        body,
        source: "test".to_owned(),
        updated_at: Utc::now(),
    };
    setup
        .engine
        .store
        .apply(UnitOfWork::new().with(WriteOp::PutDefinition(definition)))
        .await
        .unwrap();
    let settings = Settings {
        reads: std::collections::BTreeSet::new(),
        judge_usd_micros: 0,
    };
    let looped = Loop {
        engine: &setup.engine,
        judges: Judges {
            gateway: &setup.engine.gateway,
            max_output_tokens: 4000,
        },
        settings: &settings,
    };
    let attempt = looped
        .improve(Request {
            agent: "animals".to_owned(),
            task: String::new(),
            inputs: json!({ "animals": ["otter", "badger"] })
                .as_object()
                .cloned(),
            target: harness::Target::Workflow,
            why: None,
        })
        .await;
    let reference = attempt
        .reference
        .as_ref()
        .unwrap_or_else(|| panic!("{attempt:#?}"));
    assert_eq!(reference.runs.len(), 2, "{attempt:#?}");
    assert_eq!(reference.outcome.as_deref(), Some("completed"));
}
