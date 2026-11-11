use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use events::{EventError, EventPage, MemorySink};
use model::{
    Catalog, ChatResponse, FinishReason, MemoryLedger, Message, Model, ModelClient, ModelError,
    Usage,
};
use proptest::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::*;
use crate::{Effect, RegistryError, Tool, ToolDefinition, ToolFailure, ToolOutput};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Input {
    #[schemars(length(max = 8))]
    text: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Output {
    #[schemars(length(max = 100_000))]
    echoed: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Behavior {
    Good,
    Fail,
    Panic,
    Hang,
    BadOutput,
    Long,
}

#[derive(Debug)]
struct Fixture {
    name: &'static str,
    effect: Effect,
    behavior: Behavior,
    trust: Trust,
    runs: Mutex<Vec<Value>>,
}

impl Fixture {
    fn new(name: &'static str, effect: Effect, behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            name,
            effect,
            behavior,
            trust: Trust::Trusted,
            runs: Mutex::default(),
        })
    }

    fn untrusted(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            effect: Effect::Read,
            behavior: Behavior::Good,
            trust: Trust::Untrusted,
            runs: Mutex::default(),
        })
    }

    fn run_count(&self) -> usize {
        self.runs.lock().unwrap().len()
    }
}

#[async_trait]
impl Tool for Fixture {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::for_types::<Input, Output>(self.name, "a fixture", self.effect)
    }

    async fn run(
        &self,
        arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolFailure> {
        self.runs.lock().unwrap().push(arguments);
        let value = match self.behavior {
            Behavior::Good => json!({ "echoed": "hello" }),
            Behavior::Fail => return Err(ToolFailure("it broke".to_owned())),
            Behavior::Panic => panic!("tool bug"),
            Behavior::Hang => {
                tokio::time::sleep(Duration::from_secs(60)).await;
                json!({ "echoed": "late" })
            }
            Behavior::BadOutput => json!({ "wrong": true }),
            Behavior::Long => json!({ "echoed": "é".repeat(5000) }),
        };
        Ok(ToolOutput {
            value,
            trust: self.trust,
            cost_usd_micros: 0,
        })
    }
}

fn budget() -> Budget {
    Budget {
        max_rounds: 3,
        max_tool_calls: 4,
        max_writes: 1,
        max_total_tokens: 10_000,
        max_usd_micros: 1000,
        max_request_bytes: 10_000,
        max_duration_ms: 10_000,
    }
}

fn config(offered: &[&str], budget: Budget) -> SessionConfig {
    SessionConfig {
        run: RunId::generate(),
        offered: offered.iter().map(|s| (*s).to_owned()).collect(),
        budget,
        data_class: DataClass::Private,
        purpose: "work".to_owned(),
        max_result_chars: 1000,
    }
}

fn session_with(tools: &[Arc<Fixture>], offered: &[&str], budget: Budget) -> Session {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry
            .register(Arc::clone(tool) as Arc<dyn Tool>)
            .unwrap();
    }
    Session::new(
        Arc::new(registry),
        Arc::new(MemorySink::new()),
        config(offered, budget),
    )
    .unwrap()
}

fn call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: "c1".to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

const GOOD_ARGS: &str = r#"{"text":"hi"}"#;

fn parsed(reply: &ToolReply) -> Value {
    serde_json::from_str(&reply.content).unwrap()
}

#[tokio::test]
async fn a_good_call_runs_and_is_recorded() {
    let tool = Fixture::new("reader", Effect::Read, Behavior::Good);
    let mut session = session_with(std::slice::from_ref(&tool), &["reader"], budget());
    let reply = session.dispatch(&call("reader", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::Ran);
    assert_eq!(parsed(&reply)["result"]["echoed"], "hello");
    assert_eq!(tool.run_count(), 1);
    let receipt = session.finish(Outcome::Refused).await;
    assert_eq!(receipt.tool_calls[0].status, CallStatus::Ran);
    assert_eq!(receipt.outcome, Outcome::Refused);
}

#[tokio::test]
async fn a_registered_tool_not_offered_never_runs() {
    let offered = Fixture::new("reader", Effect::Read, Behavior::Good);
    let hidden = Fixture::new("hidden", Effect::Write, Behavior::Good);
    let mut session = session_with(&[offered, Arc::clone(&hidden)], &["reader"], budget());
    assert_eq!(session.tool_specs().len(), 1);
    let reply = session.dispatch(&call("hidden", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::Refused);
    let body = parsed(&reply);
    assert_eq!(body["kind"], "not_offered");
    assert_eq!(body["info"]["available"][0], "reader");
    assert_eq!(hidden.run_count(), 0);
}

#[tokio::test]
async fn a_made_up_tool_is_refused_and_recorded_at_a_bounded_length() {
    let mut session = session_with(&[], &[], budget());
    let reply = session.dispatch(&call(&"x".repeat(100_000), "{}")).await;
    assert_eq!(reply.status, CallStatus::Refused);
    let receipt = session.finish(Outcome::Refused).await;
    assert_eq!(receipt.tool_calls[0].tool.chars().count(), 64);
}

async fn refusal_of(arguments: &str) -> (Value, usize) {
    let tool = Fixture::new("reader", Effect::Read, Behavior::Good);
    let mut session = session_with(std::slice::from_ref(&tool), &["reader"], budget());
    let reply = session.dispatch(&call("reader", arguments)).await;
    assert_eq!(reply.status, CallStatus::Refused);
    (parsed(&reply), tool.run_count())
}

#[tokio::test]
async fn bad_arguments_are_refused_before_the_tool_runs() {
    for (arguments, kind) in [
        ("{not json", "arguments_not_json"),
        (r#"{"text":7}"#, "arguments_invalid"),
        (r#"{"text":"ok","extra":1}"#, "arguments_invalid"),
        (r#"{"text":"much too long"}"#, "arguments_invalid"),
        ("[1,2]", "arguments_invalid"),
        ("", "arguments_invalid"),
    ] {
        let (body, runs) = refusal_of(arguments).await;
        assert_eq!(body["kind"], kind, "{arguments}");
        assert_eq!(runs, 0);
    }
}

#[tokio::test]
async fn a_refusal_says_what_to_fix() {
    let (body, _) = refusal_of(r#"{"text":7}"#).await;
    assert_eq!(body["info"]["problems"][0]["path"], "/text");
}

#[tokio::test]
async fn huge_arguments_are_refused_without_being_parsed() {
    let huge = format!(r#"{{"text":"{}"}}"#, "x".repeat(MAX_ARGUMENT_BYTES));
    let (body, runs) = refusal_of(&huge).await;
    assert_eq!(body["kind"], "arguments_too_large");
    assert_eq!(runs, 0);
}

#[tokio::test]
async fn a_failing_tool_is_reported_as_failed() {
    let tool = Fixture::new("bad", Effect::Read, Behavior::Fail);
    let mut session = session_with(&[tool], &["bad"], budget());
    let reply = session.dispatch(&call("bad", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::Failed);
    assert_eq!(parsed(&reply)["detail"], "it broke");
}

#[tokio::test]
async fn a_panicking_tool_does_not_take_the_run_with_it() {
    let tool = Fixture::new("bad", Effect::Read, Behavior::Panic);
    let mut session = session_with(&[tool], &["bad"], budget());
    let reply = session.dispatch(&call("bad", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::Failed);
    assert_eq!(parsed(&reply)["detail"], "the tool crashed");
}

#[tokio::test]
async fn output_failing_its_schema_is_a_failure_not_a_result() {
    let tool = Fixture::new("bad", Effect::Read, Behavior::BadOutput);
    let mut session = session_with(&[tool], &["bad"], budget());
    let reply = session.dispatch(&call("bad", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::Failed);
    assert!(parsed(&reply).get("result").is_none());
}

#[tokio::test]
async fn a_long_result_is_cut_and_says_so() {
    let tool = Fixture::new("long", Effect::Read, Behavior::Long);
    let mut session = session_with(&[tool], &["long"], budget());
    let reply = session.dispatch(&call("long", GOOD_ARGS)).await;
    let body = parsed(&reply);
    assert_eq!(body["truncated"], true);
    assert_eq!(body["shown_chars"], 1000);
    assert!(body["total_chars"].as_u64().unwrap() > 5000);
    let receipt = session.finish(Outcome::Answered).await;
    assert!(receipt.tool_calls[0].truncated);
}

#[tokio::test]
async fn fetched_content_goes_to_the_model_labelled() {
    let tool = Fixture::untrusted("fetch");
    let mut session = session_with(&[tool], &["fetch"], budget());
    let reply = session.dispatch(&call("fetch", GOOD_ARGS)).await;
    assert_eq!(parsed(&reply)["untrusted"], true);
    assert!(session.finish(Outcome::Answered).await.tool_calls[0].untrusted);
}

#[tokio::test]
async fn the_write_budget_stops_a_second_write() {
    let tool = Fixture::new("writer", Effect::Write, Behavior::Good);
    let mut session = session_with(std::slice::from_ref(&tool), &["writer"], budget());
    assert_eq!(
        session.dispatch(&call("writer", GOOD_ARGS)).await.status,
        CallStatus::Ran
    );
    let reply = session.dispatch(&call("writer", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::Refused);
    assert_eq!(parsed(&reply)["kind"], "budget");
    assert_eq!(tool.run_count(), 1);
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.outcome, Outcome::BudgetHit);
    assert_eq!(receipt.budget_hit, Some(BudgetKind::Writes));
    assert_eq!(receipt.totals.writes, 1);
}

/// Spends money on each call, and counts as a read when its text says so.
#[derive(Debug, Default)]
struct Spender {
    budgets_seen: Mutex<Vec<u64>>,
}

#[async_trait]
impl Tool for Spender {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::for_types::<Input, Output>("spender", "a fixture", Effect::Write)
    }

    fn effect_of(&self, arguments: &Value) -> Effect {
        if arguments["text"] == "read" {
            Effect::Read
        } else {
            Effect::Write
        }
    }

    async fn run(&self, _: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        self.budgets_seen
            .lock()
            .unwrap()
            .push(context.budget_usd_micros);
        Ok(ToolOutput {
            value: json!({ "echoed": "hello" }),
            trust: Trust::Trusted,
            cost_usd_micros: 300,
        })
    }
}

fn spender_session(tool: &Arc<Spender>) -> Session {
    let mut registry = ToolRegistry::new();
    registry.register(tool.clone()).unwrap();
    Session::new(
        Arc::new(registry),
        Arc::new(MemorySink::new()),
        config(&["spender"], budget()),
    )
    .unwrap()
}

#[tokio::test]
async fn what_a_tool_spends_reaches_the_budget_and_the_receipt() {
    let tool = Arc::new(Spender::default());
    let mut session = spender_session(&tool);
    let read = r#"{"text":"read"}"#;
    session.dispatch(&call("spender", read)).await;
    session.dispatch(&call("spender", read)).await;
    assert_eq!(*tool.budgets_seen.lock().unwrap(), vec![1000, 700]);
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.totals.cost_usd_micros, 600);
    assert_eq!(receipt.tool_calls[0].cost_usd_micros, 300);
}

#[tokio::test]
async fn the_write_budget_goes_by_what_this_call_does() {
    let tool = Arc::new(Spender::default());
    let mut session = spender_session(&tool);
    let read = r#"{"text":"read"}"#;
    let write = r#"{"text":"write"}"#;
    assert_eq!(
        session.dispatch(&call("spender", read)).await.status,
        CallStatus::Ran
    );
    assert_eq!(
        session.dispatch(&call("spender", read)).await.status,
        CallStatus::Ran
    );
    assert_eq!(
        session.dispatch(&call("spender", write)).await.status,
        CallStatus::Ran
    );
    assert_eq!(
        session.dispatch(&call("spender", write)).await.status,
        CallStatus::Refused
    );
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.totals.writes, 1);
    assert_eq!(receipt.tool_calls[0].effect, Some(Effect::Read));
    assert_eq!(receipt.tool_calls[2].effect, Some(Effect::Write));
}

#[tokio::test]
async fn refused_calls_count_against_the_budget_and_the_receipt_stays_bounded() {
    let mut session = session_with(&[], &[], budget());
    for _ in 0..1000 {
        session.dispatch(&call("made_up", "{}")).await;
    }
    assert_eq!(session.budget_hit(), Some(BudgetKind::ToolCalls));
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.tool_calls.len(), 4);
    assert_eq!(receipt.calls_not_recorded, 996);
}

#[tokio::test]
async fn the_time_budget_stops_a_hung_tool() {
    let tool = Fixture::new("hang", Effect::Read, Behavior::Hang);
    let mut limits = budget();
    limits.max_duration_ms = 30;
    let mut session = session_with(&[tool], &["hang"], limits);
    let reply = session.dispatch(&call("hang", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::TimedOut);
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.outcome, Outcome::BudgetHit);
    assert_eq!(receipt.budget_hit, Some(BudgetKind::Time));
    assert!(receipt.tool_calls[0].status.started());
    assert!(receipt.tool_calls[0].duration_ms < 5000);
}

// ---- model calls ----

#[derive(Debug)]
struct Canned {
    usage: Option<Usage>,
    delay: Duration,
    seen: Mutex<Vec<ChatRequest>>,
    outputs: Mutex<Vec<u32>>,
    fail: bool,
}

impl Canned {
    fn new(usage: Option<Usage>) -> Arc<Self> {
        Arc::new(Self {
            usage,
            delay: Duration::ZERO,
            seen: Mutex::default(),
            outputs: Mutex::default(),
            fail: false,
        })
    }
}

#[async_trait]
impl ModelClient for Canned {
    async fn chat(
        &self,
        _model: &Model,
        request: &ChatRequest,
        output_tokens: u32,
    ) -> Result<ChatResponse, ModelError> {
        self.seen.lock().unwrap().push(request.clone());
        self.outputs.lock().unwrap().push(output_tokens);
        tokio::time::sleep(self.delay).await;
        if self.fail {
            return Err(ModelError::Timeout);
        }
        Ok(ChatResponse {
            content: "fine".to_owned(),
            tool_calls: vec![],
            finish: FinishReason::Stop,
            usage: self.usage,
        })
    }

    async fn embed(&self, _: &Model, _: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        Err(ModelError::Timeout)
    }
}

/// Plays back replies, one per call.
#[derive(Debug, Default)]
struct Scripted {
    replies: Mutex<VecDeque<ChatResponse>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn new(replies: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            seen: Mutex::default(),
        })
    }
}

#[async_trait]
impl ModelClient for Scripted {
    async fn chat(
        &self,
        _model: &Model,
        request: &ChatRequest,
        _output_tokens: u32,
    ) -> Result<ChatResponse, ModelError> {
        self.seen.lock().unwrap().push(request.clone());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ModelError::Timeout)
    }

    async fn embed(&self, _: &Model, _: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        Err(ModelError::Timeout)
    }
}

fn says(content: &str, finish: FinishReason) -> ChatResponse {
    ChatResponse {
        content: content.to_owned(),
        tool_calls: vec![],
        finish,
        usage: USED,
    }
}

fn calls(name: &str) -> ChatResponse {
    ChatResponse {
        content: "looking".to_owned(),
        tool_calls: vec![call(name, GOOD_ARGS)],
        finish: FinishReason::ToolCalls,
        usage: USED,
    }
}

async fn converse_with(
    yaml: &str,
    client: Arc<Scripted>,
    tools: &[Arc<Fixture>],
    offered: &[&str],
    budget: Budget,
) -> ChatResult {
    let catalog = Catalog::from_yaml(yaml, |_| None).unwrap();
    let gateway = Gateway::new(catalog, client, Arc::new(MemoryLedger::new()));
    session_with(tools, offered, budget)
        .converse(
            &gateway,
            Conversation {
                messages: vec![Message::user("hello")],
                max_output_tokens: 400,
                temperature: None,
            },
        )
        .await
}

const FREE: &str = "
version: 1
providers:
  - {name: p, base_url: 'http://a/v1', no_log: true, timeout_secs: 5}
models:
  - {name: m, provider: p, wire_model: a, capabilities: [chat, tools],
     context_tokens: 100000, max_output_tokens: 500}
";

const PRICED: &str = "
version: 1
providers:
  - {name: p, base_url: 'http://a/v1', no_log: true, timeout_secs: 5,
     spend_cap_usd_micros_per_day: 100000000, paid_for: [work]}
models:
  - {name: m, provider: p, wire_model: a, capabilities: [chat, tools],
     context_tokens: 100000, max_output_tokens: 500,
     price: {input_usd_micros_per_mtok: 1000000, output_usd_micros_per_mtok: 1000000}}
";

fn gateway(yaml: &str, client: Arc<Canned>) -> Gateway {
    let catalog = Catalog::from_yaml(yaml, |_| None).unwrap();
    Gateway::new(catalog, client, Arc::new(MemoryLedger::new()))
}

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user("hello")],
        tools: vec![],
        max_output_tokens: 400,
        temperature: None,
        response_schema: None,
    }
}

const USED: Option<Usage> = Some(Usage {
    input_tokens: 10,
    output_tokens: 20,
});

#[tokio::test]
async fn the_model_is_shown_only_the_offered_tools() {
    let client = Canned::new(USED);
    let gateway = gateway(FREE, Arc::clone(&client));
    let reader = Fixture::new("reader", Effect::Read, Behavior::Good);
    let hidden = Fixture::new("hidden", Effect::Read, Behavior::Good);
    let mut session = session_with(&[reader, hidden], &["reader"], budget());
    let mut asked = request();
    asked.tools.push(ToolSpec {
        name: "hidden".to_owned(),
        description: String::new(),
        parameters: json!({}),
    });
    session.call_model(&gateway, &[], asked).await.unwrap();
    let seen = client.seen.lock().unwrap();
    let names: Vec<_> = seen[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["reader"]);
}

#[tokio::test]
async fn the_round_budget_stops_the_fourth_call() {
    let client = Canned::new(USED);
    let gateway = gateway(FREE, Arc::clone(&client));
    let mut session = session_with(&[], &[], budget());
    for _ in 0..3 {
        session.call_model(&gateway, &[], request()).await.unwrap();
    }
    let stop = session
        .call_model(&gateway, &[], request())
        .await
        .unwrap_err();
    assert_eq!(stop, ModelStop::Budget(BudgetKind::Rounds));
    assert_eq!(client.seen.lock().unwrap().len(), 3);
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(
        (receipt.outcome, receipt.totals.rounds),
        (Outcome::BudgetHit, 3)
    );
    assert_eq!(receipt.totals.tokens, 90);
}

#[tokio::test]
async fn a_big_request_never_leaves() {
    let client = Canned::new(USED);
    let gateway = gateway(FREE, Arc::clone(&client));
    let mut limits = budget();
    limits.max_request_bytes = 3;
    let mut session = session_with(&[], &[], limits);
    let stop = session
        .call_model(&gateway, &[], request())
        .await
        .unwrap_err();
    assert_eq!(stop, ModelStop::Budget(BudgetKind::RequestSize));
    assert!(client.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn output_is_limited_by_the_tokens_left() {
    let client = Canned::new(USED);
    let gateway = gateway(FREE, Arc::clone(&client));
    let mut limits = budget();
    limits.max_total_tokens = 110;
    let mut session = session_with(&[], &[], limits);
    session.call_model(&gateway, &[], request()).await.unwrap();
    let output = client.outputs.lock().unwrap()[0];
    assert!(output < 110 && output > 50, "{output}");
}

#[tokio::test]
async fn the_token_budget_stops_the_run() {
    let client = Canned::new(Some(Usage {
        input_tokens: 5000,
        output_tokens: 5000,
    }));
    let gateway = gateway(FREE, client);
    let mut session = session_with(&[], &[], budget());
    session.call_model(&gateway, &[], request()).await.unwrap();
    let stop = session
        .call_model(&gateway, &[], request())
        .await
        .unwrap_err();
    assert_eq!(stop, ModelStop::Budget(BudgetKind::Tokens));
}

#[tokio::test]
async fn unreported_usage_is_counted_from_the_estimate() {
    let client = Canned::new(None);
    let gateway = gateway(FREE, client);
    let mut session = session_with(&[], &[], budget());
    session.call_model(&gateway, &[], request()).await.unwrap();
    let receipt = session.finish(Outcome::Answered).await;
    assert!(receipt.model_calls[0].tokens_estimated);
    assert!(receipt.totals.tokens >= 400);
}

#[tokio::test]
async fn the_spend_budget_is_a_budget_outcome_not_a_model_failure() {
    let client = Canned::new(USED);
    let gateway = gateway(PRICED, Arc::clone(&client));
    let mut limits = budget();
    limits.max_usd_micros = 5;
    let mut session = session_with(&[], &[], limits);
    let stop = session
        .call_model(&gateway, &[], request())
        .await
        .unwrap_err();
    assert_eq!(stop, ModelStop::Budget(BudgetKind::Spend));
    assert!(client.seen.lock().unwrap().is_empty());
    assert_eq!(
        session.finish(Outcome::ModelFailed).await.outcome,
        Outcome::BudgetHit
    );
}

#[tokio::test]
async fn the_time_budget_stops_a_slow_model() {
    let client = Arc::new(Canned {
        usage: USED,
        delay: Duration::from_secs(60),
        seen: Mutex::default(),
        outputs: Mutex::default(),
        fail: false,
    });
    let gateway = gateway(FREE, client);
    let mut limits = budget();
    limits.max_duration_ms = 30;
    let mut session = session_with(&[], &[], limits);
    let stop = session
        .call_model(&gateway, &[], request())
        .await
        .unwrap_err();
    assert_eq!(stop, ModelStop::Budget(BudgetKind::Time));
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.model_calls.len(), 1);
    assert_eq!(receipt.model_calls[0].model, None);
}

#[tokio::test]
async fn a_failed_model_is_recorded_and_cannot_be_claimed_as_answered() {
    let client = Arc::new(Canned {
        usage: USED,
        delay: Duration::ZERO,
        seen: Mutex::default(),
        outputs: Mutex::default(),
        fail: true,
    });
    let gateway = gateway(FREE, client);
    let mut session = session_with(&[], &[], budget());
    assert!(matches!(
        session.call_model(&gateway, &[], request()).await,
        Err(ModelStop::Failed(_))
    ));
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.outcome, Outcome::ModelFailed);
    assert_eq!(receipt.model_calls[0].failed_attempts, 1);
}

#[tokio::test]
async fn a_budget_outcome_cannot_be_claimed_without_one() {
    let gateway = gateway(FREE, Canned::new(USED));
    let mut session = session_with(&[], &[], budget());
    session.call_model(&gateway, &[], request()).await.unwrap();
    assert_eq!(
        session.finish(Outcome::BudgetHit).await.outcome,
        Outcome::Answered
    );
}

// ---- setup ----

#[test]
fn an_offer_of_an_unregistered_tool_is_refused() {
    let result = Session::new(
        Arc::new(ToolRegistry::new()),
        Arc::new(MemorySink::new()),
        config(&["ghost"], budget()),
    );
    assert!(matches!(result, Err(SessionError::InvalidOffer(_))));
}

#[test]
fn a_budget_outside_its_schema_is_refused() {
    let mut limits = budget();
    limits.max_tool_calls = 513;
    let result = Session::new(
        Arc::new(ToolRegistry::new()),
        Arc::new(MemorySink::new()),
        config(&[], limits),
    );
    assert!(matches!(result, Err(SessionError::InvalidBudget(_))));
}

#[test]
fn a_duplicate_tool_name_is_refused() {
    let mut registry = ToolRegistry::new();
    let tool = || Fixture::new("same", Effect::Read, Behavior::Good) as Arc<dyn Tool>;
    registry.register(tool()).unwrap();
    assert_eq!(
        registry.register(tool()),
        Err(RegistryError::Duplicate("same".to_owned()))
    );
}

#[derive(Debug)]
struct Declared(ToolDefinition);

#[async_trait]
impl Tool for Declared {
    fn definition(&self) -> ToolDefinition {
        self.0.clone()
    }

    async fn run(&self, _: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        Err(ToolFailure("unused".to_owned()))
    }
}

#[test]
fn a_loose_or_misnamed_tool_is_refused() {
    let good = || ToolDefinition::for_types::<Input, Output>("fine", "d", Effect::Read);
    let mut loose = good();
    loose.input_schema = json!({ "type": "object", "properties": {} });
    let mut misnamed = good();
    misnamed.name = "Not-Valid".to_owned();
    let mut wordy = good();
    wordy.description = "x".repeat(MAX_RESULT_TEST);
    for bad in [loose, misnamed, wordy] {
        let result = ToolRegistry::new().register(Arc::new(Declared(bad)));
        assert!(matches!(result, Err(RegistryError::Invalid { .. })));
    }
}

const MAX_RESULT_TEST: usize = 3000;

// ---- event sink ----

#[derive(Debug)]
struct BrokenSink;

#[async_trait]
impl EventSink for BrokenSink {
    async fn append(&self, _: Vec<Event>) -> Result<(), EventError> {
        Err(EventError("disk full".to_owned()))
    }

    async fn read_run(&self, _: RunId, _: usize, _: usize) -> Result<EventPage, EventError> {
        Err(EventError("disk full".to_owned()))
    }
}

#[tokio::test]
async fn a_lost_event_shows_on_the_receipt_and_does_not_stop_the_run() {
    let mut registry = ToolRegistry::new();
    registry
        .register(Fixture::new("reader", Effect::Read, Behavior::Good) as Arc<dyn Tool>)
        .unwrap();
    let mut session = Session::new(
        Arc::new(registry),
        Arc::new(BrokenSink),
        config(&["reader"], budget()),
    )
    .unwrap();
    let reply = session.dispatch(&call("reader", GOOD_ARGS)).await;
    assert_eq!(reply.status, CallStatus::Ran);
    let receipt = session.finish(Outcome::Answered).await;
    assert_eq!(receipt.events_lost, 2);
}

#[tokio::test]
async fn every_call_reaches_the_sink() {
    let sink = Arc::new(MemorySink::new());
    let mut registry = ToolRegistry::new();
    registry
        .register(Fixture::new("reader", Effect::Read, Behavior::Good) as Arc<dyn Tool>)
        .unwrap();
    let run_config = config(&["reader"], budget());
    let run = run_config.run;
    let mut session = Session::new(
        Arc::new(registry),
        Arc::clone(&sink) as Arc<dyn EventSink>,
        run_config,
    )
    .unwrap();
    session.dispatch(&call("reader", GOOD_ARGS)).await;
    session.dispatch(&call("ghost", "{}")).await;
    session.finish(Outcome::Answered).await;
    let page = sink.read_run(run, 0, 10).await.unwrap();
    let kinds: Vec<_> = page.events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::ToolCall,
            EventKind::ToolCall,
            EventKind::RunEnded
        ]
    );
    assert_eq!(page.events[1].body["refusal"], "not_offered");
}

#[test]
fn the_receipt_type_is_strict() {
    assert_eq!(
        schema::check_strict(&schema::schema_of::<Receipt>()),
        vec![]
    );
}

// ---- properties ----

fn arbitrary_call() -> impl Strategy<Value = ToolCall> {
    let name = prop_oneof![
        Just("reader".to_owned()),
        Just("hidden".to_owned()),
        "\\PC{0,80}"
    ];
    let arguments = prop_oneof![
        Just(GOOD_ARGS.to_owned()),
        "\\PC{0,200}",
        any::<u32>().prop_map(|n| format!(r#"{{"text":{n}}}"#)),
        "[a-z]{0,20}".prop_map(|s| format!(r#"{{"text":"{s}","x":1}}"#)),
    ];
    (name, arguments).prop_map(|(name, arguments)| ToolCall {
        id: "c".to_owned(),
        name,
        arguments,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn no_arguments_panic_the_parser(raw in "\\PC*") {
        let _ = parse_arguments(&raw);
    }

    #[test]
    fn the_receipt_matches_what_ran(calls in proptest::collection::vec(arbitrary_call(), 0..12)) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let reader = Fixture::new("reader", Effect::Read, Behavior::Good);
            let hidden = Fixture::new("hidden", Effect::Write, Behavior::Good);
            let mut session = session_with(
                &[Arc::clone(&reader), Arc::clone(&hidden)],
                &["reader"],
                budget(),
            );
            for call in &calls {
                let reply = session.dispatch(call).await;
                prop_assert!(serde_json::from_str::<Value>(&reply.content).is_ok());
            }
            let receipt = session.finish(Outcome::Answered).await;
            let started = receipt.tool_calls.iter().filter(|e| e.status.started()).count();
            prop_assert_eq!(started, reader.run_count());
            prop_assert_eq!(hidden.run_count(), 0);
            prop_assert!(receipt.tool_calls.len() <= 4);
            let total = receipt.tool_calls.len() as u64 + receipt.calls_not_recorded;
            prop_assert_eq!(total, calls.len() as u64);
            let raw = serde_json::to_value(&receipt).unwrap();
            prop_assert!(Validator::for_type::<Receipt>().unwrap().check(&raw).is_ok());
            Ok(())
        })?;
    }
}

const CHAT_ONLY: &str = "
version: 1
providers:
  - {name: p, base_url: 'http://a/v1', no_log: true, timeout_secs: 5}
models:
  - {name: m, provider: p, wire_model: a, capabilities: [chat],
     context_tokens: 100000, max_output_tokens: 500}
";

#[tokio::test]
async fn a_conversation_runs_tools_and_ends_with_the_answer() {
    let client = Scripted::new(vec![calls("reader"), says("done", FinishReason::Stop)]);
    let reader = Fixture::new("reader", Effect::Read, Behavior::Good);
    let result = converse_with(
        FREE,
        Arc::clone(&client),
        std::slice::from_ref(&reader),
        &["reader"],
        budget(),
    )
    .await;
    assert_eq!(result.answer.as_deref(), Some("done"));
    assert_eq!(result.stopped_because, None);
    assert_eq!(result.receipt.outcome, Outcome::Answered);
    assert!(!result.receipt.answer_cut_off);
    assert_eq!(result.receipt.tool_calls.len(), 1);
    assert_eq!(reader.run_count(), 1);

    let seen = client.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let second = &seen[1].messages;
    assert_eq!(second.len(), 3, "the question, the call and its result");
    assert_eq!(second[1].tool_calls.len(), 1);
    assert_eq!(second[1].content, "looking");
    assert_eq!(second[2].tool_call_id.as_deref(), Some("c1"));
    assert!(second[2].content.contains("hello"), "{}", second[2].content);
}

#[tokio::test]
async fn an_empty_reply_is_not_an_answer() {
    let client = Scripted::new(vec![says("  \n", FinishReason::Stop)]);
    let result = converse_with(FREE, client, &[], &[], budget()).await;
    assert_eq!(result.answer, None);
    assert_eq!(result.receipt.outcome, Outcome::ModelFailed);
    assert!(result.stopped_because.unwrap().contains("empty"));
    assert!(
        result.receipt.model_calls[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("empty")
    );
}

#[tokio::test]
async fn an_answer_cut_off_says_so() {
    let client = Scripted::new(vec![says("half an ans", FinishReason::Length)]);
    let result = converse_with(FREE, client, &[], &[], budget()).await;
    assert_eq!(result.answer.as_deref(), Some("half an ans"));
    assert!(result.receipt.answer_cut_off);
}

#[tokio::test]
async fn a_model_that_never_stops_is_stopped_by_the_rounds_budget() {
    let client = Scripted::new(vec![calls("reader"); 20]);
    let reader = Fixture::new("reader", Effect::Read, Behavior::Good);
    let result = converse_with(FREE, Arc::clone(&client), &[reader], &["reader"], budget()).await;
    assert_eq!(result.answer, None);
    assert_eq!(result.receipt.outcome, Outcome::BudgetHit);
    assert_eq!(result.receipt.budget_hit, Some(BudgetKind::Rounds));
    assert!(result.stopped_because.unwrap().contains("rounds"));
    assert_eq!(client.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn running_out_during_a_round_ends_the_run_at_once() {
    let client = Scripted::new(vec![
        calls("writer"),
        calls("writer"),
        says("late", FinishReason::Stop),
    ]);
    let writer = Fixture::new("writer", Effect::Write, Behavior::Good);
    let result = converse_with(
        FREE,
        Arc::clone(&client),
        std::slice::from_ref(&writer),
        &["writer"],
        budget(),
    )
    .await;
    assert_eq!(result.receipt.budget_hit, Some(BudgetKind::Writes));
    assert_eq!(result.answer, None);
    assert_eq!(writer.run_count(), 1);
    assert_eq!(
        client.seen.lock().unwrap().len(),
        2,
        "no round after the limit"
    );
}

#[tokio::test]
async fn a_failing_tool_goes_back_to_the_model() {
    let client = Scripted::new(vec![calls("broken"), says("it broke", FinishReason::Stop)]);
    let broken = Fixture::new("broken", Effect::Read, Behavior::Fail);
    let result = converse_with(FREE, client, &[broken], &["broken"], budget()).await;
    assert_eq!(result.answer.as_deref(), Some("it broke"));
    assert_eq!(result.receipt.tool_calls[0].status, CallStatus::Failed);
}

#[tokio::test]
async fn when_no_model_answers_the_run_says_why() {
    let result = converse_with(FREE, Scripted::new(vec![]), &[], &[], budget()).await;
    assert_eq!(result.answer, None);
    assert_eq!(result.receipt.outcome, Outcome::ModelFailed);
    assert_ne!(result.stopped_because.unwrap(), "");
}

#[tokio::test]
async fn tools_need_a_model_that_can_use_them() {
    let client = Scripted::new(vec![says("hi", FinishReason::Stop)]);
    let reader = Fixture::new("reader", Effect::Read, Behavior::Good);
    let refused = converse_with(
        CHAT_ONLY,
        Arc::clone(&client),
        &[reader],
        &["reader"],
        budget(),
    )
    .await;
    assert_eq!(refused.receipt.outcome, Outcome::ModelFailed);
    assert_eq!(client.seen.lock().unwrap().len(), 0);

    let answered = converse_with(CHAT_ONLY, client, &[], &[], budget()).await;
    assert_eq!(answered.answer.as_deref(), Some("hi"));
}
