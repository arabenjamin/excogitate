//! The routed front door: a line of workers chooses a profile, its tools and
//! their arguments one at a time, held to closed sets and checked in code,
//! and a writer answers from what the tools found.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use events::{EventSink, MemorySink};
use model::Catalog;
use runtime::{Config, FrontDoor, OnlyWhenAsked, Route, Runtime};
use serde_json::{Value, json};
use store_memory::MemoryStore;

const NOTES: &str = "description: What's stored in memory.\nsystem_prompt: You look things up in memory.\ntools: [memory]\n";
const JOBS: &str = "description: What the job queue ran.\nsystem_prompt: You read the queue.\ntools: [job_history]\n";
const WRITER: &str =
    "description: Writes the answer.\nsystem_prompt: You write the answer.\ntools: []\n";

#[derive(Clone, Default)]
struct Recorded {
    replies: Arc<Mutex<VecDeque<Value>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn text(content: &str) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }] })
}

fn json_reply(value: &Value) -> Value {
    text(&value.to_string())
}

fn choice(name: &str) -> Value {
    json_reply(&json!({ "choice": name }))
}

/// A model that answers from a queue and keeps every request it was sent.
async fn model(replies: Vec<Value>) -> (String, Recorded) {
    let recorded = Recorded {
        replies: Arc::new(Mutex::new(replies.into())),
        seen: Arc::default(),
    };
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(recorded): State<Recorded>, body: axum::Json<Value>| async move {
                    recorded.seen.lock().unwrap().push(body.0);
                    let next = recorded.replies.lock().unwrap().pop_front();
                    axum::Json(next.unwrap_or_else(|| text("out of replies")))
                },
            ),
        )
        .with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/v1"), recorded)
}

fn definitions(name: &str, agents: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("route-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for directory in ["agents", "workflows", "schedules"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    for (agent, body) in agents {
        std::fs::write(root.join("agents").join(format!("{agent}.yaml")), body).unwrap();
    }
    root
}

fn route(profiles: &[&str]) -> Route {
    Route {
        profiles: profiles.iter().map(|name| (*name).to_owned()).collect(),
        writer: "writer".to_owned(),
        small_talk: true,
        max_calls: 3,
        max_parts: 1,
        choice_role: None,
        arguments_role: None,
        only_when_asked: Vec::new(),
    }
}

struct Fixture {
    runtime: Runtime,
    sink: Arc<MemorySink>,
    recorded: Recorded,
}

async fn fixture(name: &str, route: Route, replies: Vec<Value>) -> Fixture {
    let (url, recorded) = model(replies).await;
    fixture_with(name, route, url, recorded, &[]).await
}

/// A fixture whose definitions add `extra` agents to the usual ones.
async fn fixture_with(
    name: &str,
    route: Route,
    url: String,
    recorded: Recorded,
    extra: &[(&str, &str)],
) -> Fixture {
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{url}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n\
         roles:\n  - {{name: picks, models: [c]}}\n"
    );
    let sink = Arc::new(MemorySink::new());
    let runtime = Runtime::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::clone(&sink) as Arc<dyn EventSink>,
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        Config {
            front_door: Some(FrontDoor::Route(route)),
            ..Config::default()
        },
        &|_| None,
    )
    .unwrap();
    let mut agents = vec![("notes", NOTES), ("jobs", JOBS), ("writer", WRITER)];
    agents.extend_from_slice(extra);
    let root = definitions(name, &agents);
    runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap();
    Fixture {
        runtime,
        sink,
        recorded,
    }
}

impl Fixture {
    async fn chat(&self, message: &str) -> Value {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "chat", "arguments": { "message": message } }
        });
        let input = format!("{request}\n");
        let mut output = Vec::new();
        plugin_api::serve(
            "test",
            "1",
            self.runtime.mcp_tools(),
            input.as_bytes(),
            &mut output,
        )
        .await
        .unwrap();
        let line = String::from_utf8(output).unwrap();
        serde_json::from_str::<Value>(line.lines().next().unwrap()).unwrap()["result"]
            ["structuredContent"]
            .take()
    }

    fn seen(&self) -> Vec<Value> {
        self.recorded.seen.lock().unwrap().clone()
    }

    /// The run's decisions, as their event bodies.
    async fn decisions(&self, reply: &Value) -> Vec<Value> {
        let run = reply["receipt"]["run"].as_str().unwrap().parse().unwrap();
        self.sink
            .read_run(run, 0, 1000)
            .await
            .unwrap()
            .events
            .into_iter()
            .filter(|event| event.kind == events::EventKind::Decision)
            .map(|event| event.body)
            .collect()
    }
}

fn user_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_calls(reply: &Value) -> Vec<String> {
    reply["receipt"]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["tool"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_chooses_a_profile_fills_its_call_and_the_writer_answers_from_the_results() {
    let fixture = fixture(
        "happy",
        route(&["notes", "jobs"]),
        vec![
            choice("notes"),
            json_reply(&json!({ "operation": "search", "query": "otters" })),
            choice("done"),
            text("They hold hands."),
        ],
    )
    .await;
    let reply = fixture.chat("What do otters do?").await;
    assert_eq!(reply["answer"], "They hold hands.", "{reply}");
    assert_eq!(tool_calls(&reply), ["memory"], "{reply}");

    let seen = fixture.seen();
    assert_eq!(seen.len(), 4, "{seen:?}");
    // No step is offered tools: each is a choice or a schema to fill.
    assert!(seen.iter().all(|request| {
        request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_none_or(Vec::is_empty))
    }));
    let options = user_text(&seen[0]);
    assert!(options.contains("What's stored in memory."), "{options}");
    assert!(options.contains("small_talk"), "{options}");
    assert!(user_text(&seen[1]).contains("Fill in the arguments for one call to memory"));
    assert_eq!(
        seen[1]["messages"][0]["content"],
        "You look things up in memory."
    );
    // The writer reads the question and the results, under its own prompt.
    assert_eq!(seen[3]["messages"][0]["content"], "You write the answer.");
    let asked = user_text(&seen[3]);
    assert!(asked.contains("Question: What do otters do?"), "{asked}");
    assert!(asked.contains("== memory("), "{asked}");

    let decisions = fixture.decisions(&reply).await;
    assert_eq!(decisions[0]["picked"], "notes", "{decisions:?}");
    assert_eq!(decisions[0]["stage"], "profile", "{decisions:?}");
    assert_eq!(decisions[1]["tool"], "memory", "{decisions:?}");
    assert_eq!(decisions[2]["picked"], "done", "{decisions:?}");
    assert_eq!(decisions[2]["profile"], "notes", "{decisions:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_choice_that_names_no_option_looks_nothing_up_and_says_so() {
    let fixture = fixture(
        "invalid",
        route(&["notes"]),
        vec![choice("weather"), text("Hello.")],
    )
    .await;
    let reply = fixture.chat("Hi there").await;
    assert_eq!(reply["answer"], "Hello.", "{reply}");
    assert!(tool_calls(&reply).is_empty(), "{reply}");
    assert!(user_text(&fixture.seen()[1]).contains("Nothing was looked up"));
    let decisions = fixture.decisions(&reply).await;
    assert_eq!(decisions[0]["picked"], "invalid", "{decisions:?}");
    assert_eq!(decisions[1]["rule"], "invalid_choice", "{decisions:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invented_argument_is_dropped_and_the_same_call_twice_ends_the_profile() {
    let same = json!({ "operation": "search", "query": "otters" });
    let fixture = fixture(
        "repeat",
        route(&["notes"]),
        vec![
            choice("notes"),
            json_reply(&json!({ "operation": "search", "query": "otters", "kind": "invented" })),
            choice("memory"),
            json_reply(&same),
            json_reply(&same),
            text("Nothing more found."),
        ],
    )
    .await;
    let reply = fixture.chat("What do otters do?").await;
    assert_eq!(reply["answer"], "Nothing more found.", "{reply}");
    assert_eq!(tool_calls(&reply), ["memory"], "{reply}");
    let seen = fixture.seen();
    // The refill was told what already ran.
    assert!(
        user_text(&seen[4]).contains("already ran with"),
        "{:?}",
        seen[4]
    );

    let decisions = fixture.decisions(&reply).await;
    let first = &decisions[1];
    assert_eq!(first["kept"], same.to_string(), "{decisions:?}");
    assert!(first["dropped"].as_str().unwrap().contains("invented"));
    assert!(decisions.iter().any(|step| step["refilled"] == true));
    assert!(
        decisions.iter().any(|step| step["rule"] == "no_repeat"),
        "{decisions:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_makes_at_most_max_calls() {
    let mut once = route(&["notes"]);
    once.max_calls = 1;
    let fixture = fixture(
        "max-calls",
        once,
        vec![
            choice("notes"),
            json_reply(&json!({ "operation": "search", "query": "otters" })),
            text("One look."),
        ],
    )
    .await;
    let reply = fixture.chat("What do otters do?").await;
    assert_eq!(reply["answer"], "One look.", "{reply}");
    // No next-call choice: the writer came straight after the one call.
    assert_eq!(fixture.seen().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_taken_only_when_asked_goes_elsewhere_when_it_isnt() {
    let mut stored_first = route(&["notes", "jobs"]);
    stored_first.only_when_asked = vec![OnlyWhenAsked {
        profile: "jobs".to_owned(),
        asked: r"\b(job|queue)s?\b".to_owned(),
        otherwise: "notes".to_owned(),
    }];
    let fixture = fixture(
        "stored-first",
        stored_first,
        vec![
            choice("jobs"),
            json_reply(&json!({ "operation": "search", "query": "otters" })),
            choice("done"),
            text("From memory."),
        ],
    )
    .await;
    let reply = fixture.chat("What do otters do?").await;
    assert_eq!(tool_calls(&reply), ["memory"], "{reply}");
    let decisions = fixture.decisions(&reply).await;
    assert_eq!(decisions[1]["rule"], "stored_first", "{decisions:?}");
}

async fn refused(name: &str, route: Route, agents: &[(&str, &str)], expected: &str) {
    let (url, _) = model(Vec::new()).await;
    let yaml = format!(
        "version: 1\nproviders:\n  - {{name: c, base_url: '{url}', no_log: true, timeout_secs: 10}}\n\
         models:\n  - {{name: c, provider: c, wire_model: x, capabilities: [chat, tools], context_tokens: 100000, max_output_tokens: 100}}\n"
    );
    let runtime = Runtime::assemble(
        Arc::new(MemoryStore::new(3)),
        Arc::new(MemorySink::new()),
        Catalog::from_yaml(&yaml, |_| None).unwrap(),
        Config {
            front_door: Some(FrontDoor::Route(route)),
            ..Config::default()
        },
        &|_| None,
    )
    .unwrap();
    let root = definitions(name, agents);
    let error = runtime
        .load_definitions(&root, chrono::Utc::now())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(expected), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_its_agents_cant_serve_is_refused_when_loaded() {
    let base = [("notes", NOTES), ("writer", WRITER)];
    refused("no-profile", route(&["nobody"]), &base, "the agent nobody").await;
    refused(
        "writer-tools",
        route(&["notes"]),
        &[("notes", NOTES), ("writer", NOTES)],
        "the writer writer has tools",
    )
    .await;
    let delegating = "description: Hands on.\nsystem_prompt: You hand on.\ntools: [delegate]\n";
    refused(
        "delegates",
        route(&["door"]),
        &[("door", delegating), ("writer", WRITER)],
        "delegates",
    )
    .await;
    let mut role = route(&["notes"]);
    role.choice_role = Some("nobody".to_owned());
    refused("role", role, &base, "choice_role names the role nobody").await;
    let mut asked = route(&["notes"]);
    asked.only_when_asked = vec![OnlyWhenAsked {
        profile: "web".to_owned(),
        asked: "web".to_owned(),
        otherwise: "notes".to_owned(),
    }];
    refused("asked", asked, &base, "only_when_asked names web").await;
    let mut calls = route(&["notes"]);
    calls.max_calls = 9;
    refused("calls", calls, &base, "max_calls").await;
    let mut parts = route(&["notes"]);
    parts.max_parts = 5;
    refused("parts", parts, &base, "max_parts").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_would_write_isnt_run() {
    let mut once = route(&["notes"]);
    once.max_calls = 1;
    let fixture = fixture(
        "reads-only",
        once,
        vec![
            choice("notes"),
            // An id the question gave, so grounding keeps it.
            json_reply(
                &json!({ "operation": "delete", "id": "0192f0d0-0000-7000-8000-000000000001" }),
            ),
            text("I can't change notes from here."),
        ],
    )
    .await;
    let reply = fixture
        .chat("Delete note 0192f0d0-0000-7000-8000-000000000001")
        .await;
    assert_eq!(
        reply["answer"], "I can't change notes from here.",
        "{reply}"
    );
    assert!(tool_calls(&reply).is_empty(), "{reply}");
    assert!(user_text(&fixture.seen()[2]).contains("only reads"));
    let decisions = fixture.decisions(&reply).await;
    assert!(
        decisions.iter().any(|step| step["rule"] == "reads_only"),
        "{decisions:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_query_in_other_words_is_kept_since_a_search_needs_one() {
    let mut once = route(&["notes"]);
    once.max_calls = 1;
    let fixture = fixture(
        "rephrased",
        once,
        vec![
            choice("notes"),
            // Not words of the question: memory requires a query only for a
            // search, through its schema's allOf, which the model isn't shown.
            json_reply(&json!({ "operation": "search", "query": "otter behaviour" })),
            text("Nothing on otters."),
        ],
    )
    .await;
    let reply = fixture.chat("What do otters do?").await;
    let call = &reply["receipt"]["tool_calls"][0];
    assert_eq!(call["tool"], "memory", "{reply}");
    assert_eq!(call["status"], "ran", "{reply}");
    let decisions = fixture.decisions(&reply).await;
    assert_eq!(
        decisions[1]["kept"],
        json!({ "operation": "search", "query": "otter behaviour" }).to_string(),
        "{decisions:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_about_two_subjects_is_split_and_the_writer_answers_both() {
    let mut split = route(&["notes", "jobs"]);
    split.max_parts = 2;
    split.small_talk = false;
    let fixture = fixture(
        "split",
        split,
        vec![
            json_reply(&json!({ "parts": ["What do otters do?", "What jobs ran lately?"] })),
            // The first part: notes, a search, then done.
            choice("notes"),
            json_reply(&json!({ "operation": "search", "query": "otters" })),
            choice("done"),
            // The second: jobs, its one tool, then done.
            choice("jobs"),
            json_reply(&json!({})),
            choice("done"),
            text("Otters hold hands; nothing ran."),
        ],
    )
    .await;
    let asked = "What do otters do, and what jobs ran lately?";
    let reply = fixture.chat(asked).await;
    assert_eq!(
        reply["answer"], "Otters hold hands; nothing ran.",
        "{reply}"
    );
    assert_eq!(tool_calls(&reply), ["memory", "job_history"], "{reply}");
    let seen = fixture.seen();
    // Each part's choice sees that part, not the whole turn.
    assert!(user_text(&seen[1]).contains("User: What do otters do?"));
    assert!(user_text(&seen[4]).contains("User: What jobs ran lately?"));
    // The writer answers the owner's words, from both parts' results.
    let written = user_text(&seen[7]);
    assert!(written.contains(&format!("Question: {asked}")), "{written}");
    assert!(written.contains("== memory(") && written.contains("== job_history("));
    let decisions = fixture.decisions(&reply).await;
    let rule = decisions
        .iter()
        .find(|step| step["rule"] == "split")
        .unwrap();
    assert!(
        rule["detail"].as_str().unwrap().starts_with("2 parts:"),
        "{rule}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_about_one_subject_runs_on_the_owners_own_words() {
    let mut split = route(&["notes"]);
    split.max_parts = 2;
    split.max_calls = 1;
    let fixture = fixture(
        "one-part",
        split,
        vec![
            // A rewording, which the line doesn't use for one part.
            json_reply(&json!({ "parts": ["Tell me about otter behaviour"] })),
            choice("notes"),
            json_reply(&json!({ "operation": "search", "query": "otters" })),
            text("They hold hands."),
        ],
    )
    .await;
    let reply = fixture.chat("What do otters do?").await;
    assert_eq!(reply["answer"], "They hold hands.", "{reply}");
    assert!(user_text(&fixture.seen()[1]).contains("User: What do otters do?"));
    let decisions = fixture.decisions(&reply).await;
    assert!(
        decisions.iter().all(|step| step["rule"] != "split"),
        "{decisions:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn calculate_isnt_a_first_call_but_is_offered_once_a_result_is_in() {
    let mut counted = route(&["sums"]);
    counted.max_calls = 2;
    let (url, recorded) = model(vec![
        choice("sums"),
        // No tool choice: with calculate set aside, memory is the only one.
        json_reply(&json!({ "operation": "search", "query": "otters" })),
        choice("calculate"),
        json_reply(&json!({ "expression": "2 + 2" })),
        text("Four."),
    ])
    .await;
    let fixture = fixture_with(
        "calc-first",
        counted,
        url,
        recorded,
        &[(
            "sums",
            "description: Sums of what's stored.\nsystem_prompt: You add.\ntools: [memory, calculate]\n",
        )],
    )
    .await;
    let reply = fixture.chat("What do otters do, in sums?").await;
    assert_eq!(tool_calls(&reply), ["memory", "calculate"], "{reply}");
    let seen = fixture.seen();
    assert!(user_text(&seen[1]).contains("Fill in the arguments for one call to memory"));
    let next = user_text(&seen[2]);
    assert!(next.contains("calculate"), "offered after a result: {next}");
}
