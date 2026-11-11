//! One run: the only path from a model's request to what actually happens.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use events::{Event, EventSink};
use model::{
    Capability, ChatRequest, Completion, Gateway, GatewayError, Rejection, RoutePolicy, ToolCall,
    ToolSpec, estimate_tokens,
};
use protocol::{DataClass, RunId, Trust};
use schema::{Refusal, Validator, bounded};
use serde_json::{Value, json};
use tokio::time::timeout;

mod converse;

pub use converse::{ChatResult, Conversation};

use crate::tool::Registered;
use crate::trajectory::{RunSubject, Step};
use crate::{
    Budget, BudgetKind, CallStatus, ModelEntry, Outcome, Receipt, RefusalKind, ToolContext,
    ToolEntry, ToolRegistry, Totals,
};

/// The largest arguments any tool may be sent.
const MAX_ARGUMENT_BYTES: usize = 256 * 1024;
/// The most tools one run may be offered.
const MAX_OFFERED: usize = 64;
/// How much of a tool name goes in a receipt.
const MAX_RECORDED_NAME_CHARS: usize = 64;
/// How much of arguments or a reply goes in a trajectory event.
const MAX_EVENT_CHARS: usize = 16_000;

/// How one run is set up. All of it comes from the brain, none from a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// The run.
    pub run: RunId,
    /// The tools this run may call.
    pub offered: Vec<String>,
    /// Its limits.
    pub budget: Budget,
    /// The privacy class of its data.
    pub data_class: DataClass,
    /// What the work is for, which decides whether it may pay.
    pub purpose: String,
    /// How much of a result goes back to the model, in characters.
    pub max_result_chars: usize,
    /// What's running and under which harness, for its trajectory.
    pub subject: RunSubject,
}

/// A session could not be set up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The budget fails its schema.
    #[error("invalid budget: {0}")]
    InvalidBudget(String),
    /// A tool offered isn't registered, or too many were offered.
    #[error("invalid offer: {0}")]
    InvalidOffer(String),
}

/// Why a model call didn't produce an answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelStop {
    /// A budget ran out.
    #[error("budget hit: {0:?}")]
    Budget(BudgetKind),
    /// The model couldn't be reached or refused the call.
    #[error(transparent)]
    Failed(GatewayError),
}

/// What goes back to the model for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReply {
    /// The call it answers.
    pub call_id: String,
    /// The result or the refusal, as JSON text the model can act on.
    pub content: String,
    /// What became of the call.
    pub status: CallStatus,
}

/// Reads a model's tool arguments. An empty string means no arguments, as
/// some servers send for a tool with none.
///
/// # Errors
///
/// The parser's message, if it isn't a single JSON value.
pub fn parse_arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|error| bounded(&error.to_string()))
}

fn recorded_name(name: &str) -> String {
    name.chars().take(MAX_RECORDED_NAME_CHARS).collect()
}

fn event_text(text: &str) -> String {
    let mut cut: String = text.chars().take(MAX_EVENT_CHARS).collect();
    if text.chars().nth(MAX_EVENT_CHARS).is_some() {
        cut.push_str("… (cut)");
    }
    cut
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn problems_json(refusal: &Refusal) -> Value {
    let problems: Vec<Value> = refusal
        .problems
        .iter()
        .map(|problem| json!({ "path": problem.path, "message": problem.message }))
        .collect();
    json!({ "problems": problems, "more_problems": refusal.truncated })
}

/// One run.
#[derive(Debug)]
pub struct Session {
    registry: Arc<ToolRegistry>,
    sink: Arc<dyn EventSink>,
    offered: BTreeSet<String>,
    config: SessionConfig,
    started: Instant,
    hit: Option<BudgetKind>,
    totals: Totals,
    model_calls: Vec<ModelEntry>,
    tool_calls: Vec<ToolEntry>,
    calls_not_recorded: u64,
    events_lost: u32,
    answer_cut_off: bool,
    by_harness: bool,
    start_recorded: bool,
}

/// A refusal on its way back to the model.
struct Refused {
    kind: RefusalKind,
    detail: String,
    extra: Value,
}

impl Session {
    /// Starts a run.
    ///
    /// # Errors
    ///
    /// [`SessionError`] if the budget fails its schema, or a tool offered
    /// isn't registered.
    pub fn new(
        registry: Arc<ToolRegistry>,
        sink: Arc<dyn EventSink>,
        config: SessionConfig,
    ) -> Result<Self, SessionError> {
        let invalid = |error: String| SessionError::InvalidBudget(error);
        let raw = serde_json::to_value(config.budget).map_err(|e| invalid(e.to_string()))?;
        Validator::for_type::<Budget>()
            .map_err(|e| invalid(e.to_string()))?
            .check(&raw)
            .map_err(|e| invalid(e.to_string()))?;
        if config.offered.len() > MAX_OFFERED {
            return Err(SessionError::InvalidOffer(format!(
                "at most {MAX_OFFERED} tools can be offered"
            )));
        }
        if let Some(missing) = config
            .offered
            .iter()
            .find(|name| registry.get(name).is_none())
        {
            return Err(SessionError::InvalidOffer(format!(
                "{missing} isn't registered"
            )));
        }
        Ok(Self {
            offered: config.offered.iter().cloned().collect(),
            registry,
            sink,
            config,
            started: Instant::now(),
            hit: None,
            totals: Totals::default(),
            model_calls: Vec::new(),
            tool_calls: Vec::new(),
            calls_not_recorded: 0,
            events_lost: 0,
            answer_cut_off: false,
            by_harness: false,
            start_recorded: false,
        })
    }

    /// The tools to show the model: the offered ones and no others.
    #[must_use]
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.offered
            .iter()
            .filter_map(|name| self.registry.get(name))
            .map(|registered| ToolSpec {
                name: registered.definition.name.clone(),
                description: registered.definition.description.clone(),
                parameters: schema::for_model(&registered.definition.input_schema),
            })
            .collect()
    }

    /// The limit this run hit, if any. Once hit, nothing else runs.
    #[must_use]
    pub const fn budget_hit(&self) -> Option<BudgetKind> {
        self.hit
    }

    fn time_left(&self) -> Duration {
        Duration::from_millis(self.config.budget.max_duration_ms)
            .saturating_sub(self.started.elapsed())
    }

    fn mark_hit(&mut self, kind: BudgetKind) -> BudgetKind {
        *self.hit.get_or_insert(kind)
    }

    /// The limit that stops work now, if one does.
    fn stopped(&mut self) -> Option<BudgetKind> {
        if self.hit.is_none() && self.time_left().is_zero() {
            self.mark_hit(BudgetKind::Time);
        }
        self.hit
    }

    fn event(&self, step: &Step) -> Event {
        Event {
            run: self.config.run,
            at: Utc::now(),
            kind: step.kind(),
            data_class: self.config.data_class,
            body: step.body(),
        }
    }

    /// Records a step. A run's first step is always its start, so every
    /// trajectory says what ran and under which harness.
    pub(crate) async fn emit(&mut self, step: Step) {
        let mut events = Vec::with_capacity(2);
        if !self.start_recorded {
            self.start_recorded = true;
            events.push(self.event(&Step::Started {
                agent: self.config.subject.agent.clone(),
                harness: self.config.subject.harness.clone(),
                purpose: self.config.purpose.clone(),
                offered: self.offered.iter().cloned().collect(),
                budget: self.config.budget,
            }));
        }
        let count = u32::try_from(events.len() + 1).unwrap_or(u32::MAX);
        events.push(self.event(&step));
        if self.sink.append(events).await.is_err() {
            self.events_lost = self.events_lost.saturating_add(count);
        }
    }

    /// Calls a model for the next round. The tools in `request` are replaced
    /// by this run's, and its limits are applied.
    ///
    /// # Errors
    ///
    /// [`ModelStop`] if a budget stops the call or no model answers.
    pub async fn call_model(
        &mut self,
        gateway: &Gateway,
        capabilities: &[Capability],
        mut request: ChatRequest,
    ) -> Result<Completion, ModelStop> {
        if let Some(kind) = self.stopped() {
            return Err(ModelStop::Budget(kind));
        }
        let budget = self.config.budget;
        if self.totals.rounds >= budget.max_rounds {
            return Err(ModelStop::Budget(self.mark_hit(BudgetKind::Rounds)));
        }
        request.tools = self.tool_specs();
        let request_bytes: usize = request
            .messages
            .iter()
            .map(|m| {
                m.content.len()
                    + m.tool_calls
                        .iter()
                        .map(|c| c.arguments.len())
                        .sum::<usize>()
            })
            .sum();
        if u64::try_from(request_bytes).unwrap_or(u64::MAX) > budget.max_request_bytes {
            return Err(ModelStop::Budget(self.mark_hit(BudgetKind::RequestSize)));
        }
        let estimate = u64::from(estimate_tokens(&request));
        let tokens_left = budget.max_total_tokens.saturating_sub(self.totals.tokens);
        if estimate >= tokens_left {
            return Err(ModelStop::Budget(self.mark_hit(BudgetKind::Tokens)));
        }
        let output_room = u32::try_from(tokens_left - estimate).unwrap_or(u32::MAX);
        request.max_output_tokens = request.max_output_tokens.min(output_room);
        let policy = RoutePolicy {
            capabilities: capabilities.to_vec(),
            data_class: self.config.data_class,
            purpose: self.config.purpose.clone(),
            budget_usd_micros: Some(
                budget
                    .max_usd_micros
                    .saturating_sub(self.totals.cost_usd_micros),
            ),
            embedding_dimension: None,
        };

        self.totals.rounds += 1;
        let Ok(result) = timeout(self.time_left(), gateway.chat(&policy, &request)).await else {
            self.record_model_failure("timed out against the run's time budget".to_owned(), 0);
            return Err(ModelStop::Budget(self.mark_hit(BudgetKind::Time)));
        };
        match result {
            Ok(completion) => {
                self.record_model_success(&completion, estimate, request.max_output_tokens);
                self.emit(Step::Model {
                    model: Some(completion.model.clone()),
                    reply: Some(event_text(&completion.response.content)),
                    tool_calls: u32::try_from(completion.response.tool_calls.len())
                        .unwrap_or(u32::MAX),
                    cost_usd_micros: completion.cost_usd_micros,
                    error: None,
                })
                .await;
                Ok(completion)
            }
            Err(error) => {
                let failed = match &error {
                    GatewayError::Failed(attempts) => attempts.len(),
                    _ => 0,
                };
                self.record_model_failure(bounded(&error.to_string()), failed);
                self.emit(Step::Model {
                    model: None,
                    reply: None,
                    tool_calls: 0,
                    cost_usd_micros: 0,
                    error: Some(bounded(&error.to_string())),
                })
                .await;
                if let GatewayError::NoRoute(no_route) = &error
                    && no_route
                        .rejected
                        .iter()
                        .any(|(_, why)| *why == Rejection::OverRunBudget)
                {
                    return Err(ModelStop::Budget(self.mark_hit(BudgetKind::Spend)));
                }
                Err(ModelStop::Failed(error))
            }
        }
    }

    fn record_model_success(&mut self, completion: &Completion, estimate: u64, output: u32) {
        let usage = completion.response.usage;
        let (input_tokens, output_tokens) = usage.map_or((estimate, u64::from(output)), |usage| {
            (
                u64::from(usage.input_tokens),
                u64::from(usage.output_tokens),
            )
        });
        self.totals.tokens = self
            .totals
            .tokens
            .saturating_add(input_tokens)
            .saturating_add(output_tokens);
        self.totals.cost_usd_micros = self
            .totals
            .cost_usd_micros
            .saturating_add(completion.cost_usd_micros);
        self.model_calls.push(ModelEntry {
            model: Some(completion.model.clone()),
            input_tokens,
            output_tokens,
            tokens_estimated: usage.is_none(),
            cost_usd_micros: completion.cost_usd_micros,
            failed_attempts: u32::try_from(completion.failed_attempts.len()).unwrap_or(u32::MAX),
            detail: None,
        });
    }

    fn record_model_failure(&mut self, detail: String, failed_attempts: usize) {
        self.model_calls.push(ModelEntry {
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            tokens_estimated: false,
            cost_usd_micros: 0,
            failed_attempts: u32::try_from(failed_attempts).unwrap_or(u32::MAX),
            detail: Some(detail),
        });
    }

    /// Makes a call on the harness's own initiative, with every check a
    /// model's call gets. The receipt says the model didn't ask for it.
    /// Returns `None`, and makes no call, if it would leave the model no tool
    /// calls of its own.
    pub async fn dispatch_for_harness(&mut self, call: &ToolCall) -> Option<ToolReply> {
        if self.totals.tool_calls.saturating_add(1) >= self.config.budget.max_tool_calls {
            return None;
        }
        self.by_harness = true;
        let reply = self.dispatch(call).await;
        self.by_harness = false;
        Some(reply)
    }

    /// Handles one tool call. It always returns a reply for the model: a
    /// result, or a structured refusal or failure it can act on.
    pub async fn dispatch(&mut self, call: &ToolCall) -> ToolReply {
        let name = recorded_name(&call.name);
        if let Some(kind) = self.stopped() {
            // Past a budget nothing is recorded, or a storm of calls could
            // make the receipt unbounded.
            self.calls_not_recorded = self.calls_not_recorded.saturating_add(1);
            return Self::reply(
                call,
                CallStatus::Refused,
                refusal_json(
                    RefusalKind::Budget,
                    "the run's budget is used up",
                    &json!({ "budget": kind }),
                ),
            );
        }
        if self.totals.tool_calls >= self.config.budget.max_tool_calls {
            let kind = self.mark_hit(BudgetKind::ToolCalls);
            self.calls_not_recorded = self.calls_not_recorded.saturating_add(1);
            return Self::reply(
                call,
                CallStatus::Refused,
                refusal_json(
                    RefusalKind::Budget,
                    "the run's budget is used up",
                    &json!({ "budget": kind }),
                ),
            );
        }
        self.totals.tool_calls += 1;

        let registry = Arc::clone(&self.registry);
        let checked = self.check_call(&registry, call);
        let (registered, arguments) = match checked {
            Ok(accepted) => accepted,
            Err(refused) => {
                let effect = registry.get(&call.name).map(|r| r.definition.effect);
                return self.refuse(call, name, effect, refused).await;
            }
        };
        let effect = registered.tool.effect_of(&arguments);
        if effect == crate::Effect::Write {
            if self.totals.writes >= self.config.budget.max_writes {
                let kind = self.mark_hit(BudgetKind::Writes);
                let refused = Refused {
                    kind: RefusalKind::Budget,
                    detail: "the run's write budget is used up".to_owned(),
                    extra: json!({ "budget": kind }),
                };
                return self.refuse(call, name, Some(effect), refused).await;
            }
            self.totals.writes += 1;
        }
        self.run_tool(call, name, registered, effect, arguments)
            .await
    }

    fn check_call<'r>(
        &self,
        registry: &'r ToolRegistry,
        call: &ToolCall,
    ) -> Result<(&'r Registered, Value), Refused> {
        let registered = registry
            .get(&call.name)
            .filter(|_| self.offered.contains(&call.name))
            .ok_or_else(|| Refused {
                kind: RefusalKind::NotOffered,
                detail: "that tool isn't available in this run".to_owned(),
                extra: json!({ "available": self.offered }),
            })?;
        if call.arguments.len() > MAX_ARGUMENT_BYTES {
            return Err(Refused {
                kind: RefusalKind::ArgumentsTooLarge,
                detail: format!("arguments are limited to {MAX_ARGUMENT_BYTES} bytes"),
                extra: json!({}),
            });
        }
        let arguments = parse_arguments(&call.arguments).map_err(|error| Refused {
            kind: RefusalKind::ArgumentsNotJson,
            detail: error,
            extra: json!({}),
        })?;
        registered
            .input
            .check(&arguments)
            .map_err(|refusal| Refused {
                kind: RefusalKind::ArgumentsInvalid,
                detail: "the arguments don't meet the tool's schema".to_owned(),
                extra: problems_json(&refusal),
            })?;
        Ok((registered, arguments))
    }

    async fn refuse(
        &mut self,
        call: &ToolCall,
        name: String,
        effect: Option<crate::Effect>,
        refused: Refused,
    ) -> ToolReply {
        let detail = bounded(&refused.detail);
        let index = self.call_index();
        self.tool_calls.push(ToolEntry {
            tool: name.clone(),
            status: CallStatus::Refused,
            refusal: Some(refused.kind),
            effect,
            truncated: false,
            untrusted: false,
            by_harness: self.by_harness,
            duration_ms: 0,
            cost_usd_micros: 0,
            detail: Some(detail.clone()),
        });
        self.emit(Step::Tool {
            index,
            tool: name,
            status: CallStatus::Refused,
            refusal: Some(refused.kind),
            effect,
            by_harness: self.by_harness,
            arguments: event_text(&call.arguments),
            reply: Some(detail.clone()),
            duration_ms: 0,
        })
        .await;
        Self::reply(
            call,
            CallStatus::Refused,
            refusal_json(refused.kind, &detail, &refused.extra),
        )
    }

    async fn run_tool(
        &mut self,
        call: &ToolCall,
        name: String,
        registered: &Registered,
        effect: crate::Effect,
        arguments: Value,
    ) -> ToolReply {
        let context = ToolContext {
            run: self.config.run,
            data_class: self.config.data_class,
            budget_usd_micros: self
                .config
                .budget
                .max_usd_micros
                .saturating_sub(self.totals.cost_usd_micros),
        };
        let begun = Instant::now();
        let tool = Arc::clone(&registered.tool);
        // A task of its own, so a panicking tool doesn't take the run with it.
        let task = tokio::spawn(async move { tool.run(arguments, context).await });
        let abort = task.abort_handle();
        let joined = timeout(self.time_left(), task).await;
        let duration_ms = millis(begun.elapsed());

        let finished = match joined {
            Err(_) => {
                abort.abort();
                self.mark_hit(BudgetKind::Time);
                Finished::failed(CallStatus::TimedOut, "the tool ran out of time".to_owned())
            }
            Ok(Err(_)) => Finished::failed(CallStatus::Failed, "the tool crashed".to_owned()),
            Ok(Ok(Err(failure))) => Finished::failed(CallStatus::Failed, bounded(&failure.0)),
            Ok(Ok(Ok(output))) => match registered.output.check(&output.value) {
                Ok(()) => {
                    self.totals.cost_usd_micros = self
                        .totals
                        .cost_usd_micros
                        .saturating_add(output.cost_usd_micros);
                    Finished::ran(
                        &output.value,
                        output.trust,
                        self.config.max_result_chars,
                        output.cost_usd_micros,
                    )
                }
                Err(refusal) => Finished::failed(
                    CallStatus::Failed,
                    format!(
                        "the tool's result failed its schema: {}",
                        bounded(&refusal.to_string())
                    ),
                ),
            },
        };
        let index = self.call_index();
        self.tool_calls.push(ToolEntry {
            tool: name.clone(),
            status: finished.status,
            refusal: None,
            effect: Some(effect),
            truncated: finished.truncated,
            untrusted: finished.untrusted,
            by_harness: self.by_harness,
            duration_ms,
            cost_usd_micros: finished.cost_usd_micros,
            detail: finished.detail,
        });
        self.emit(Step::Tool {
            index,
            tool: name,
            status: finished.status,
            refusal: None,
            effect: Some(effect),
            by_harness: self.by_harness,
            arguments: event_text(&call.arguments),
            reply: Some(event_text(&finished.content)),
            duration_ms,
        })
        .await;
        ToolReply {
            call_id: call.id.clone(),
            content: finished.content,
            status: finished.status,
        }
    }

    fn call_index(&self) -> u32 {
        u32::try_from(self.tool_calls.len()).unwrap_or(u32::MAX)
    }

    fn reply(call: &ToolCall, status: CallStatus, content: String) -> ToolReply {
        ToolReply {
            call_id: call.id.clone(),
            content,
            status,
        }
    }

    /// Ends the run and writes its receipt. A claim the record contradicts is
    /// corrected: a run that hit a budget says so whatever was claimed, and
    /// no run counts as answered unless a model answered.
    pub async fn finish(mut self, claimed: Outcome) -> Receipt {
        let any_answer = self.model_calls.iter().any(|entry| entry.model.is_some());
        let outcome = match (self.hit, claimed) {
            (Some(_), _) => Outcome::BudgetHit,
            (None, Outcome::BudgetHit | Outcome::Answered) if !any_answer => Outcome::ModelFailed,
            (None, Outcome::BudgetHit) => Outcome::Answered,
            (None, claimed) => claimed,
        };
        self.totals.duration_ms = millis(self.started.elapsed());
        self.emit(Step::Ended {
            outcome,
            budget_hit: self.hit,
            cost_usd_micros: self.totals.cost_usd_micros,
        })
        .await;
        Receipt {
            run: self.config.run.to_string(),
            outcome,
            budget_hit: self.hit,
            answer_cut_off: self.answer_cut_off,
            model_calls: self.model_calls,
            tool_calls: self.tool_calls,
            calls_not_recorded: self.calls_not_recorded,
            events_lost: self.events_lost,
            totals: self.totals,
        }
    }
}

fn refusal_json(kind: RefusalKind, detail: &str, extra: &Value) -> String {
    json!({ "status": "refused", "kind": kind, "detail": detail, "info": extra }).to_string()
}

/// How a started tool call went.
struct Finished {
    status: CallStatus,
    content: String,
    truncated: bool,
    untrusted: bool,
    cost_usd_micros: u64,
    detail: Option<String>,
}

impl Finished {
    fn failed(status: CallStatus, detail: String) -> Self {
        Self {
            status,
            content: json!({ "status": "failed", "detail": detail }).to_string(),
            truncated: false,
            untrusted: false,
            cost_usd_micros: 0,
            detail: Some(detail),
        }
    }

    fn ran(value: &Value, trust: Trust, max_chars: usize, cost_usd_micros: u64) -> Self {
        let untrusted = trust == Trust::Untrusted;
        let rendered = value.to_string();
        let total = rendered.chars().count();
        let truncated = total > max_chars;
        let content = if truncated {
            let cut: String = rendered.chars().take(max_chars).collect();
            json!({
                "status": "ok",
                "untrusted": untrusted,
                "truncated": true,
                "shown_chars": max_chars,
                "total_chars": total,
                "partial_result": cut,
            })
        } else {
            json!({ "status": "ok", "untrusted": untrusted, "result": value })
        };
        Self {
            status: CallStatus::Ran,
            content: content.to_string(),
            truncated,
            untrusted,
            cost_usd_micros,
            detail: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests;
