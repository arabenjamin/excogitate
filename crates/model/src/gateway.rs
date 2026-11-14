//! The one way the rest of the runtime calls a model: route, reserve the money,
//! call, settle, and move to the next model when that could help.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};

use crate::route::{Candidate, NoRoute, Rejection, RoutePolicy, TokenEstimate, plan};
use crate::{
    Capability, Catalog, ChatRequest, ChatResponse, ModelClient, ModelError, SpendLedger,
    estimate_tokens,
};

/// The most models tried for one call.
const MAX_ATTEMPTS: usize = 3;

/// A model that was tried and failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedAttempt {
    /// The model.
    pub model: String,
    /// What went wrong.
    pub error: ModelError,
    /// What it was charged, in millionths of a dollar: the prompt's price
    /// when the request may have reached the model, otherwise nothing.
    pub cost_usd_micros: u64,
}

/// A call could not be answered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// No model was allowed to take it.
    #[error(transparent)]
    NoRoute(#[from] NoRoute),
    /// Models were tried, and they failed.
    #[error("every model tried failed: {}", explain(.0))]
    Failed(Vec<FailedAttempt>),
}

impl GatewayError {
    /// What the failed attempts were charged, in millionths of a dollar.
    /// It's spent all the same, so a caller keeping accounts must count it.
    #[must_use]
    pub fn cost_usd_micros(&self) -> u64 {
        match self {
            Self::NoRoute(_) => 0,
            Self::Failed(attempts) => failed_cost(attempts),
        }
    }
}

fn failed_cost(attempts: &[FailedAttempt]) -> u64 {
    attempts.iter().fold(0, |sum, attempt| {
        sum.saturating_add(attempt.cost_usd_micros)
    })
}

fn explain(attempts: &[FailedAttempt]) -> String {
    let parts: Vec<String> = attempts
        .iter()
        .map(|attempt| format!("{}: {}", attempt.model, attempt.error))
        .collect();
    parts.join("; ")
}

/// An answered chat call, with what's needed for its receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The reply.
    pub response: ChatResponse,
    /// The model that gave it.
    pub model: String,
    /// What it cost, in millionths of a dollar, including what the models
    /// that failed first were charged. When the server doesn't report
    /// usage, the worst case is charged.
    pub cost_usd_micros: u64,
    /// The models that failed first.
    pub failed_attempts: Vec<FailedAttempt>,
    /// The models passed over without a call, because they were cooling.
    pub cooling: Vec<String>,
}

/// An answered embeddings call.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedded {
    /// One vector per input.
    pub vectors: Vec<Vec<f32>>,
    /// The model that made them.
    pub model: String,
    /// What it cost, in millionths of a dollar, including what the models
    /// that failed first were charged.
    pub cost_usd_micros: u64,
}

/// What a successful attempt produced.
struct Answered<T> {
    value: T,
    model: String,
    cost_usd_micros: u64,
    failed_attempts: Vec<FailedAttempt>,
}

/// What a call has been charged without an answer: attempts that failed and
/// attempts dropped mid-flight. For a caller that may cut a call off, as a
/// run's time limit does: the call returns nothing then, so this is how the
/// run learns what the day's ledger was charged for it.
#[derive(Debug, Clone, Default)]
pub struct Abandoned(Arc<AtomicU64>);

impl Abandoned {
    /// What has been charged so far, in millionths of a dollar.
    #[must_use]
    pub fn charged(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn add(&self, micros: u64) {
        // Saturating: a sum this large is already wrong, never wrapped.
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
                Some(now.saturating_add(micros))
            });
    }
}

/// A reservation not yet settled. If the call is dropped while it's out, as
/// by a run's time limit, it's settled at what the prompt alone costs: the
/// provider may bill a little more for output it had started, but leaving
/// the worst case would fill the day's cap with spend that never happened.
struct Reservation {
    ledger: Arc<dyn SpendLedger>,
    /// The provider, for a paid model; nothing to settle otherwise.
    provider: Option<String>,
    reserved: u64,
    abandoned: u64,
    at: DateTime<Utc>,
    /// Told what a dropped call was charged.
    tally: Option<Abandoned>,
}

impl Reservation {
    /// The call finished: it'll be settled the ordinary way.
    fn close(mut self) {
        self.provider = None;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let Some(provider) = self.provider.take() else {
            return;
        };
        if let Some(tally) = &self.tally {
            tally.add(self.abandoned);
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (ledger, reserved, actual, at) = (
            Arc::clone(&self.ledger),
            self.reserved,
            self.abandoned,
            self.at,
        );
        handle.spawn(async move { ledger.settle(&provider, reserved, actual, at).await });
    }
}

/// Which models are left alone, until when, and whether the server said
/// so. A model that failed in a way the next call would too
/// ([`ModelError::cooling_secs`]) is passed over while it cools, so a rate
/// limit is waited out by using another model, never by asking again in a
/// loop. A model that said to wait (a 429) isn't asked before its time is
/// up, even when there's no other. One that failed or was slow is asked
/// again if nothing else can take the call: one timeout mustn't stop a
/// one-model setup for a minute.
#[derive(Debug, Default)]
struct Cooling(Mutex<HashMap<String, Cooled>>);

#[derive(Debug, Clone, Copy)]
struct Cooled {
    until: DateTime<Utc>,
    /// The server said to slow down.
    told_to_wait: bool,
}

impl Cooling {
    fn of(&self, model: &str, now: DateTime<Utc>) -> Option<Cooled> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match held.get(model) {
            Some(cooled) if cooled.until > now => Some(*cooled),
            Some(_) => {
                held.remove(model);
                None
            }
            None => None,
        }
    }

    fn start(&self, model: &str, error: &ModelError, now: DateTime<Utc>) {
        if let Some(secs) = error.cooling_secs() {
            let until = now + chrono::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX));
            let told_to_wait = matches!(error, ModelError::RateLimited { .. });
            let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            // A later failure never shortens a wait already set, nor
            // forgets that the server said to wait.
            let entry = held.entry(model.to_owned()).or_insert(Cooled {
                until,
                told_to_wait,
            });
            entry.until = entry.until.max(until);
            entry.told_to_wait |= told_to_wait;
        }
    }

    /// A call answered: the model is fine again.
    fn end(&self, model: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(model);
    }
}

/// Routes calls to models.
pub struct Gateway {
    catalog: Catalog,
    client: Arc<dyn ModelClient>,
    ledger: Arc<dyn SpendLedger>,
    cooling: Cooling,
    clock: fn() -> DateTime<Utc>,
}

impl fmt::Debug for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gateway")
            .field("models", &self.catalog.models().len())
            .finish_non_exhaustive()
    }
}

impl Gateway {
    /// A gateway over these models.
    #[must_use]
    pub fn new(
        catalog: Catalog,
        client: Arc<dyn ModelClient>,
        ledger: Arc<dyn SpendLedger>,
    ) -> Self {
        Self {
            catalog,
            client,
            ledger,
            cooling: Cooling::default(),
            clock: Utc::now,
        }
    }

    /// The candidates to try, in order, and the names of those passed over
    /// while they cool. Those not cooling go first, and only they when
    /// there are any. With none, the ones that only failed are tried again;
    /// with those gone too, every model said to wait, and the call fails at
    /// once saying until when.
    fn not_cooling(
        &self,
        candidates: Vec<Candidate>,
    ) -> Result<(Vec<Candidate>, Vec<String>), NoRoute> {
        let now = (self.clock)();
        let mut ready = Vec::new();
        let mut failed_lately = Vec::new();
        let mut rejected = Vec::new();
        for candidate in candidates {
            match self.cooling.of(&candidate.model.name, now) {
                None => ready.push(candidate),
                Some(cooled) => {
                    rejected.push((
                        candidate.model.name.clone(),
                        Rejection::Cooling {
                            until: cooled.until,
                        },
                    ));
                    if !cooled.told_to_wait {
                        failed_lately.push(candidate);
                    }
                }
            }
        }
        if ready.is_empty() && failed_lately.is_empty() {
            return Err(NoRoute { rejected });
        }
        if ready.is_empty() {
            // Nothing else can take it: ask again rather than fail.
            return Ok((failed_lately, Vec::new()));
        }
        Ok((ready, rejected.into_iter().map(|(name, _)| name).collect()))
    }

    /// Reserves the worst case on a priced model. Returns false at the cap.
    async fn reserve(&self, candidate: &Candidate, at: DateTime<Utc>) -> bool {
        let model = &candidate.model;
        if !model.is_paid() {
            return true;
        }
        // Loading the catalog guarantees a priced model's provider has a cap.
        let cap = model.provider.daily_cap_usd_micros.unwrap_or(0);
        self.ledger
            .reserve(
                &model.provider.name,
                candidate.worst_case_usd_micros,
                cap,
                at,
            )
            .await
    }

    /// Settles on the day of the reservation, even past midnight.
    async fn settle(&self, candidate: &Candidate, actual: u64, at: DateTime<Utc>) {
        let model = &candidate.model;
        if model.is_paid() {
            self.ledger
                .settle(
                    &model.provider.name,
                    candidate.worst_case_usd_micros,
                    actual,
                    at,
                )
                .await;
        }
    }

    /// A guard that settles the reservation if the call is abandoned before
    /// it's settled the ordinary way.
    fn open_reservation(
        &self,
        candidate: &Candidate,
        at: DateTime<Utc>,
        tally: Option<&Abandoned>,
    ) -> Reservation {
        Reservation {
            ledger: Arc::clone(&self.ledger),
            provider: candidate
                .model
                .is_paid()
                .then(|| candidate.model.provider.name.clone()),
            reserved: candidate.worst_case_usd_micros,
            abandoned: candidate.input_usd_micros,
            at,
            tally: tally.cloned(),
        }
    }

    /// Tries candidates in order until one answers, keeping the books.
    /// `call` returns the answer and what it really cost.
    async fn first_success<T, F, Fut>(
        &self,
        candidates: Vec<Candidate>,
        tally: Option<&Abandoned>,
        call: F,
    ) -> Result<Answered<T>, GatewayError>
    where
        F: Fn(Candidate) -> Fut,
        Fut: Future<Output = Result<(T, u64), ModelError>>,
    {
        let mut failed_attempts = Vec::new();
        let mut at_cap = Vec::new();
        for candidate in candidates {
            if failed_attempts.len() == MAX_ATTEMPTS {
                break;
            }
            let model_name = candidate.model.name.clone();
            let reserved_at = Utc::now();
            if !self.reserve(&candidate, reserved_at).await {
                at_cap.push((model_name, Rejection::OverDailyCap));
                continue;
            }
            let open = self.open_reservation(&candidate, reserved_at, tally);
            let answer = call(candidate.clone()).await;
            open.close();
            match answer {
                Ok((value, cost_usd_micros)) => {
                    self.cooling.end(&model_name);
                    self.settle(&candidate, cost_usd_micros, reserved_at).await;
                    return Ok(Answered {
                        value,
                        model: model_name,
                        cost_usd_micros: cost_usd_micros
                            .saturating_add(failed_cost(&failed_attempts)),
                        failed_attempts,
                    });
                }
                Err(error) => {
                    self.cooling.start(&model_name, &error, (self.clock)());
                    // A request that may have reached the model may be
                    // billed: charge the least such a call costs.
                    let charged = if error.reached_the_model() {
                        candidate.input_usd_micros
                    } else {
                        0
                    };
                    self.settle(&candidate, charged, reserved_at).await;
                    // Counted now: if a later attempt is cut off, this one's
                    // charge is still the run's.
                    if let Some(tally) = tally {
                        tally.add(charged);
                    }
                    let go_on = error.worth_trying_elsewhere();
                    failed_attempts.push(FailedAttempt {
                        model: model_name,
                        error,
                        cost_usd_micros: charged,
                    });
                    if !go_on {
                        break;
                    }
                }
            }
        }
        if failed_attempts.is_empty() {
            return Err(NoRoute { rejected: at_cap }.into());
        }
        Err(GatewayError::Failed(failed_attempts))
    }

    /// Answers a chat call on the best model allowed to take it.
    ///
    /// # Errors
    ///
    /// [`GatewayError::NoRoute`] if none may take it, with each one's reason.
    /// [`GatewayError::Failed`] if those tried failed, with each one's error.
    pub async fn chat(
        &self,
        policy: &RoutePolicy,
        request: &ChatRequest,
    ) -> Result<Completion, GatewayError> {
        self.chat_counting(policy, request, None).await
    }

    /// As [`chat`](Self::chat), telling `abandoned` what the ledger was
    /// charged if the call is dropped before it ends.
    ///
    /// # Errors
    ///
    /// As for [`chat`](Self::chat).
    pub async fn chat_counting(
        &self,
        policy: &RoutePolicy,
        request: &ChatRequest,
        abandoned: Option<&Abandoned>,
    ) -> Result<Completion, GatewayError> {
        let mut required = policy.capabilities.clone();
        required.push(Capability::Chat);
        if !request.tools.is_empty() {
            required.push(Capability::Tools);
        }
        let input = estimate_tokens(request);
        let estimate = TokenEstimate {
            input,
            largest_input: input,
            output: request.max_output_tokens,
        };
        let (candidates, cooling) =
            self.not_cooling(plan(&self.catalog, &required, policy, estimate)?)?;
        let client = &self.client;
        let answered = self
            .first_success(candidates, abandoned, |candidate| async move {
                let model = &candidate.model;
                let response = client.chat(model, request, candidate.output_tokens).await?;
                let cost = response
                    .usage
                    .map_or(candidate.worst_case_usd_micros, |usage| {
                        // Charge the higher of the price and what the server
                        // billed, so a cap never undercounts.
                        model
                            .price
                            .cost(usage.input_tokens, usage.output_tokens)
                            .max(usage.reported_cost_usd_micros.unwrap_or(0))
                    });
                Ok((response, cost))
            })
            .await?;
        Ok(Completion {
            response: answered.value,
            model: answered.model,
            cost_usd_micros: answered.cost_usd_micros,
            failed_attempts: answered.failed_attempts,
            cooling,
        })
    }

    /// Embeds `inputs` on the best model allowed to. Set
    /// [`RoutePolicy::embedding_dimension`] to the store's width. Each input
    /// has to fit the model on its own; together they set the price, which
    /// is charged at the estimate.
    ///
    /// # Errors
    ///
    /// As for [`chat`](Self::chat).
    pub async fn embed(
        &self,
        policy: &RoutePolicy,
        inputs: &[String],
    ) -> Result<Embedded, GatewayError> {
        let tokens = |bytes: usize| u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX);
        let estimate = TokenEstimate {
            input: tokens(inputs.iter().map(String::len).sum()),
            largest_input: tokens(inputs.iter().map(String::len).max().unwrap_or(0)),
            output: 0,
        };
        let (candidates, _) = self.not_cooling(plan(
            &self.catalog,
            &[Capability::Embeddings],
            policy,
            estimate,
        )?)?;
        let client = &self.client;
        let answered = self
            .first_success(candidates, None, |candidate| async move {
                let vectors = client.embed(&candidate.model, inputs).await?;
                Ok((vectors, candidate.worst_case_usd_micros))
            })
            .await?;
        Ok(Embedded {
            vectors: answered.value,
            model: answered.model,
            cost_usd_micros: answered.cost_usd_micros,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::{ChatRequest, ChatResponse, FinishReason, Model, ModelClient, ModelError, Usage};

    /// A client that plays back a script and records who was called.
    #[derive(Debug, Default)]
    pub(crate) struct Scripted {
        pub(crate) replies: Mutex<VecDeque<Result<ChatResponse, ModelError>>>,
        pub(crate) calls: Mutex<Vec<(String, ChatRequest, u32)>>,
    }

    impl Scripted {
        pub(crate) fn new(
            replies: impl IntoIterator<Item = Result<ChatResponse, ModelError>>,
        ) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().collect()),
                calls: Mutex::default(),
            }
        }

        pub(crate) fn called(&self) -> Vec<String> {
            let calls = self.calls.lock().unwrap();
            calls.iter().map(|(name, ..)| name.clone()).collect()
        }
    }

    pub(crate) fn text(content: &str) -> ChatResponse {
        ChatResponse {
            content: content.to_owned(),
            tool_calls: vec![],
            finish: FinishReason::Stop,
            usage: Some(Usage {
                input_tokens: 1_000_000,
                output_tokens: 0,
                reported_cost_usd_micros: None,
            }),
        }
    }

    #[async_trait]
    impl ModelClient for Scripted {
        async fn chat(
            &self,
            model: &Model,
            request: &ChatRequest,
            output_tokens: u32,
        ) -> Result<ChatResponse, ModelError> {
            self.calls
                .lock()
                .unwrap()
                .push((model.name.clone(), request.clone(), output_tokens));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ModelError::Timeout))
        }

        async fn embed(
            &self,
            model: &Model,
            inputs: &[String],
        ) -> Result<Vec<Vec<f32>>, ModelError> {
            self.calls.lock().unwrap().push((
                model.name.clone(),
                ChatRequest {
                    messages: vec![],
                    tools: vec![],
                    max_output_tokens: 0,
                    temperature: None,
                    response_schema: None,
                },
                0,
            ));
            Ok(inputs.iter().map(|_| vec![0.0; 2]).collect())
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::test_support::{Scripted, text};
    use super::*;
    use crate::catalog::test_support::catalog;
    use crate::{MemoryLedger, Message};
    use protocol::DataClass;

    const YAML: &str = "
version: 1
providers:
  - {name: one, base_url: 'http://a/v1', no_log: true, timeout_secs: 5}
  - {name: two, base_url: 'http://b/v1', no_log: true, timeout_secs: 5}
  - {name: paid, base_url: 'http://c/v1', no_log: true, timeout_secs: 5,
     spend_cap_usd_micros_per_day: 100, paid_for: [work]}
models:
  - {name: first, provider: one, wire_model: a, capabilities: [chat, embeddings],
     embedding_dimension: 2, context_tokens: 2000000, max_output_tokens: 10}
  - {name: second, provider: two, wire_model: b, capabilities: [chat],
     context_tokens: 2000000, max_output_tokens: 10}
  - {name: priced, provider: paid, wire_model: c, capabilities: [chat, investigative],
     context_tokens: 2000000, max_output_tokens: 10,
     price: {input_usd_micros_per_mtok: 40, output_usd_micros_per_mtok: 0}}
";

    fn policy() -> RoutePolicy {
        RoutePolicy {
            capabilities: vec![],
            data_class: DataClass::Private,
            purpose: "work".to_owned(),
            budget_usd_micros: None,
            embedding_dimension: None,
            only_model: None,
        }
    }

    fn priced_policy() -> RoutePolicy {
        RoutePolicy {
            capabilities: vec![Capability::Investigative],
            ..policy()
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user("x".repeat(3_000_000))],
            tools: vec![],
            max_output_tokens: 99,
            temperature: None,
            response_schema: None,
        }
    }

    fn gateway(client: Arc<Scripted>) -> Gateway {
        Gateway::new(catalog(YAML), client, Arc::new(MemoryLedger::new()))
    }

    #[tokio::test]
    async fn a_failure_moves_to_the_next_model_and_is_reported() {
        let client = Arc::new(Scripted::new([Err(ModelError::Timeout), Ok(text("hi"))]));
        let done = gateway(Arc::clone(&client))
            .chat(&policy(), &request())
            .await
            .unwrap();
        assert_eq!(done.model, "second");
        assert_eq!(done.failed_attempts.len(), 1);
        assert_eq!(done.failed_attempts[0].model, "first");
        assert_eq!(client.called(), ["first", "second"]);
    }

    #[tokio::test]
    async fn a_rate_limited_model_is_left_alone_while_it_cools() {
        let client = Arc::new(Scripted::new([
            Err(ModelError::RateLimited {
                retry_after_secs: Some(30),
            }),
            Ok(text("a")),
            Ok(text("b")),
        ]));
        let gateway = gateway(Arc::clone(&client));
        gateway.chat(&policy(), &request()).await.unwrap();
        let second = gateway.chat(&policy(), &request()).await.unwrap();
        // Straight to the next model: no second try at the cooling one.
        assert_eq!(second.model, "second");
        assert_eq!(second.failed_attempts.len(), 0);
        assert_eq!(client.called(), ["first", "second", "second"]);
    }

    fn two_hours_on() -> DateTime<Utc> {
        Utc::now() + chrono::Duration::hours(2)
    }

    #[tokio::test]
    async fn with_every_model_told_to_wait_a_call_fails_at_once_and_says_until_when() {
        let client = Arc::new(Scripted::new([
            Err(ModelError::RateLimited {
                retry_after_secs: Some(30),
            }),
            Ok(text("later")),
        ]));
        let mut gateway = gateway(Arc::clone(&client));
        // Only `priced` can do this call.
        assert!(gateway.chat(&priced_policy(), &request()).await.is_err());
        let error = gateway
            .chat(&priced_policy(), &request())
            .await
            .unwrap_err();
        let GatewayError::NoRoute(why) = &error else {
            panic!("{error}");
        };
        assert!(
            matches!(why.rejected.as_slice(), [(name, Rejection::Cooling { .. })] if name == "priced"),
            "{error}"
        );
        assert!(error.to_string().contains("left alone until"), "{error}");
        assert_eq!(client.called(), ["priced"]);

        // Once its time is up, it's tried again.
        gateway.clock = two_hours_on;
        let done = gateway.chat(&priced_policy(), &request()).await.unwrap();
        assert_eq!(done.model, "priced");
    }

    #[tokio::test]
    async fn a_model_that_only_failed_is_asked_again_when_nothing_else_can() {
        let client = Arc::new(Scripted::new([
            // Never reached it, so the tiny test cap isn't spent.
            Err(ModelError::Transport("refused".to_owned())),
            Ok(text("back")),
            Ok(text("again")),
        ]));
        let gateway = gateway(Arc::clone(&client));
        // Only `priced` can do this call.
        assert!(gateway.chat(&priced_policy(), &request()).await.is_err());
        // It's cooling, but there's no other: one failure mustn't stop it
        // for a minute.
        let done = gateway.chat(&priced_policy(), &request()).await.unwrap();
        assert_eq!(done.model, "priced");
        assert_eq!(done.cooling.len(), 0);
        // Its answer ends the cooling.
        gateway.chat(&priced_policy(), &request()).await.unwrap();
        assert_eq!(client.called(), ["priced", "priced", "priced"]);
    }

    #[test]
    fn what_cools_a_model_and_for_how_long() {
        let limited = |secs| ModelError::RateLimited {
            retry_after_secs: secs,
        };
        assert_eq!(limited(Some(5)).cooling_secs(), Some(5));
        assert_eq!(
            limited(None).cooling_secs(),
            Some(crate::client::DEFAULT_COOLING_SECS)
        );
        // A server's own wait can't keep a model out of use for long.
        assert_eq!(
            limited(Some(86_400)).cooling_secs(),
            Some(crate::client::MAX_COOLING_SECS)
        );
        assert!(ModelError::Timeout.cooling_secs().is_some());
        assert!(
            ModelError::Transport("refused".to_owned())
                .cooling_secs()
                .is_some()
        );
        let status = |code| ModelError::Status {
            code,
            excerpt: String::new(),
        };
        assert!(status(503).cooling_secs().is_some());
        assert_eq!(status(400).cooling_secs(), None);
        assert_eq!(ModelError::BadResponse("x".to_owned()).cooling_secs(), None);
    }

    #[tokio::test]
    async fn a_request_the_server_calls_bad_is_not_retried() {
        let bad = ModelError::Status {
            code: 400,
            excerpt: String::new(),
        };
        let client = Arc::new(Scripted::new([Err(bad), Ok(text("no"))]));
        let error = gateway(Arc::clone(&client))
            .chat(&policy(), &request())
            .await
            .unwrap_err();
        assert!(matches!(error, GatewayError::Failed(a) if a.len() == 1));
        assert_eq!(client.called(), ["first"]);
    }

    #[tokio::test]
    async fn the_output_limit_is_cut_to_the_model() {
        let client = Arc::new(Scripted::new([Ok(text("hi"))]));
        gateway(Arc::clone(&client))
            .chat(&policy(), &request())
            .await
            .unwrap();
        assert_eq!(client.calls.lock().unwrap()[0].2, 10);
    }

    /// Never answers.
    #[derive(Debug)]
    struct Hangs;

    #[async_trait::async_trait]
    impl ModelClient for Hangs {
        async fn chat(
            &self,
            _: &crate::Model,
            _: &ChatRequest,
            _: u32,
        ) -> Result<ChatResponse, ModelError> {
            std::future::pending().await
        }

        async fn embed(&self, _: &crate::Model, _: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
            std::future::pending().await
        }
    }

    /// Keeps every settlement.
    #[derive(Debug, Default)]
    struct Settlements(std::sync::Mutex<Vec<(u64, u64)>>);

    #[async_trait::async_trait]
    impl SpendLedger for Settlements {
        async fn reserve(&self, _: &str, _: u64, _: u64, _: DateTime<Utc>) -> bool {
            true
        }

        async fn settle(&self, _: &str, reserved: u64, actual: u64, _: DateTime<Utc>) {
            self.0.lock().unwrap().push((reserved, actual));
        }
    }

    #[tokio::test]
    async fn an_abandoned_call_is_settled_at_what_its_prompt_costs() {
        let yaml = YAML.replace(
            "output_usd_micros_per_mtok: 0",
            "output_usd_micros_per_mtok: 1000000",
        );
        let ledger = Arc::new(Settlements::default());
        let gateway = Gateway::new(
            catalog(&yaml),
            Arc::new(Hangs),
            Arc::clone(&ledger) as Arc<dyn SpendLedger>,
        );
        let policy = priced_policy();
        let tally = Abandoned::default();
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            gateway.chat_counting(&policy, &request(), Some(&tally)),
        )
        .await;
        assert!(abandoned.is_err());
        // The settlement is spawned when the call is dropped.
        for _ in 0..100 {
            if !ledger.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let settled = ledger.0.lock().unwrap().clone();
        assert_eq!(settled.len(), 1, "{settled:?}");
        let (reserved, actual) = settled[0];
        // Ten output tokens at a dollar per million: the worst case less the
        // output it never gave.
        assert_eq!(reserved - actual, 10, "{settled:?}");
        assert!(actual > 0);
        // The caller is told the same charge, at once.
        assert_eq!(tally.charged(), actual);
    }

    #[tokio::test]
    async fn a_paid_call_that_reached_the_model_is_charged_its_prompt() {
        for (error, charged) in [
            (ModelError::Timeout, true),
            (ModelError::BadResponse("no choices".to_owned()), true),
            (
                ModelError::RateLimited {
                    retry_after_secs: None,
                },
                false,
            ),
            (ModelError::Transport("refused".to_owned()), false),
        ] {
            let ledger = Arc::new(Settlements::default());
            let gateway = Gateway::new(
                catalog(YAML),
                Arc::new(Scripted::new([Err(error.clone())])),
                Arc::clone(&ledger) as Arc<dyn SpendLedger>,
            );
            let failed = gateway
                .chat(&priced_policy(), &request())
                .await
                .unwrap_err();
            let settled = ledger.0.lock().unwrap().clone();
            assert_eq!(settled.len(), 1, "{error}: {settled:?}");
            let (_, actual) = settled[0];
            // What the run is told it spent is what the ledger was charged.
            assert_eq!(failed.cost_usd_micros(), actual, "{error}");
            assert_eq!(actual > 0, charged, "{error}: {settled:?}");
        }
    }

    #[tokio::test]
    async fn an_answer_counts_what_the_failures_before_it_were_charged() {
        // Two priced models: the cheaper times out, the other answers.
        let yaml = YAML
            .replace(
                "spend_cap_usd_micros_per_day: 100,",
                "spend_cap_usd_micros_per_day: 1000000,",
            )
            .replace(
                "  - {name: second, provider: two, wire_model: b, capabilities: [chat],\n     context_tokens: 2000000, max_output_tokens: 10}",
                "  - {name: second, provider: paid, wire_model: b, capabilities: [chat, investigative],\n     context_tokens: 2000000, max_output_tokens: 10,\n     price: {input_usd_micros_per_mtok: 80, output_usd_micros_per_mtok: 0}}",
            );
        let gateway = Gateway::new(
            catalog(&yaml),
            Arc::new(Scripted::new([Err(ModelError::Timeout), Ok(text("hi"))])),
            Arc::new(MemoryLedger::new()),
        );
        let done = gateway.chat(&priced_policy(), &request()).await.unwrap();
        assert_eq!(done.model, "second");
        let timed_out = done.failed_attempts[0].cost_usd_micros;
        assert!(timed_out > 0);
        // The answer's own usage: a million input tokens at 80 per million.
        assert_eq!(done.cost_usd_micros, timed_out + 80);
    }

    /// Times out on its first call, and never answers the next.
    #[derive(Debug, Default)]
    struct FailsThenHangs(std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl ModelClient for FailsThenHangs {
        async fn chat(
            &self,
            _: &crate::Model,
            _: &ChatRequest,
            _: u32,
        ) -> Result<ChatResponse, ModelError> {
            if !self.0.swap(true, Ordering::Relaxed) {
                return Err(ModelError::Timeout);
            }
            std::future::pending().await
        }

        async fn embed(&self, _: &crate::Model, _: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_call_cut_off_still_counts_the_attempts_that_failed_before_it() {
        let yaml = YAML
            .replace(
                "spend_cap_usd_micros_per_day: 100,",
                "spend_cap_usd_micros_per_day: 1000000,",
            )
            .replace(
                "  - {name: second, provider: two, wire_model: b, capabilities: [chat],\n     context_tokens: 2000000, max_output_tokens: 10}",
                "  - {name: second, provider: paid, wire_model: b, capabilities: [chat, investigative],\n     context_tokens: 2000000, max_output_tokens: 10,\n     price: {input_usd_micros_per_mtok: 80, output_usd_micros_per_mtok: 0}}",
            );
        let ledger = Arc::new(Settlements::default());
        let gateway = Gateway::new(
            catalog(&yaml),
            Arc::new(FailsThenHangs::default()),
            Arc::clone(&ledger) as Arc<dyn SpendLedger>,
        );
        let tally = Abandoned::default();
        let cut = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            gateway.chat_counting(&priced_policy(), &request(), Some(&tally)),
        )
        .await;
        assert!(cut.is_err());
        // The dropped call's settlement is spawned when it's dropped.
        for _ in 0..100 {
            if ledger.0.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let settled = ledger.0.lock().unwrap().clone();
        assert_eq!(settled.len(), 2, "{settled:?}");
        // The first model's prompt for its timeout, and the second's for the
        // call dropped mid-flight: the run is told what the ledger took.
        let charged: u64 = settled.iter().map(|(_, actual)| actual).sum();
        assert!(settled.iter().all(|(_, actual)| *actual > 0), "{settled:?}");
        assert_eq!(tally.charged(), charged);
    }

    #[tokio::test]
    async fn spend_counts_against_the_daily_cap() {
        let client = Arc::new(Scripted::new([Ok(text("a")), Ok(text("b")), Ok(text("c"))]));
        let gateway = gateway(Arc::clone(&client));
        let policy = priced_policy();
        let first = gateway.chat(&policy, &request()).await.unwrap();
        assert_eq!(first.cost_usd_micros, 40);
        gateway.chat(&policy, &request()).await.unwrap();
        let error = gateway.chat(&policy, &request()).await.unwrap_err();
        assert!(error.to_string().contains("daily cap"), "{error}");
        assert_eq!(client.called().len(), 2);
    }

    #[tokio::test]
    async fn unknown_usage_is_charged_at_the_worst_case() {
        let mut reply = text("a");
        reply.usage = None;
        let client = Arc::new(Scripted::new([Ok(reply)]));
        let done = gateway(client)
            .chat(&priced_policy(), &request())
            .await
            .unwrap();
        assert_eq!(done.cost_usd_micros, 41);
    }

    #[tokio::test]
    async fn a_billed_cost_above_the_price_is_what_counts() {
        let mut reply = text("a");
        if let Some(usage) = reply.usage.as_mut() {
            usage.reported_cost_usd_micros = Some(5_000);
        }
        let client = Arc::new(Scripted::new([Ok(reply)]));
        let done = gateway(client)
            .chat(&priced_policy(), &request())
            .await
            .unwrap();
        assert_eq!(done.cost_usd_micros, 5_000);
    }

    #[tokio::test]
    async fn a_billed_cost_below_the_price_doesnt_lower_it() {
        let mut reply = text("a");
        if let Some(usage) = reply.usage.as_mut() {
            usage.reported_cost_usd_micros = Some(0);
        }
        let priced = gateway(Arc::new(Scripted::new([Ok(text("a"))])))
            .chat(&priced_policy(), &request())
            .await
            .unwrap()
            .cost_usd_micros;
        let done = gateway(Arc::new(Scripted::new([Ok(reply)])))
            .chat(&priced_policy(), &request())
            .await
            .unwrap();
        assert_eq!(done.cost_usd_micros, priced);
        assert!(priced > 0);
    }

    #[tokio::test]
    async fn a_failed_call_gives_its_reservation_back() {
        // A refusal never reached the model, so it's charged nothing. (A
        // 400: it doesn't cool the model, so the next calls reach it.)
        let client = Arc::new(Scripted::new([
            Err(ModelError::Status {
                code: 400,
                excerpt: String::new(),
            }),
            Ok(text("a")),
            Ok(text("b")),
        ]));
        let gateway = gateway(client);
        let policy = priced_policy();
        assert!(gateway.chat(&policy, &request()).await.is_err());
        gateway.chat(&policy, &request()).await.unwrap();
        gateway.chat(&policy, &request()).await.unwrap();
    }

    #[tokio::test]
    async fn tools_require_the_capability() {
        let client = Arc::new(Scripted::new([]));
        let mut with_tools = request();
        with_tools.tools.push(crate::ToolSpec {
            name: "t".to_owned(),
            description: String::new(),
            parameters: serde_json::json!({}),
        });
        let error = gateway(client)
            .chat(&policy(), &with_tools)
            .await
            .unwrap_err();
        assert!(matches!(error, GatewayError::NoRoute(_)));
    }

    #[tokio::test]
    async fn a_batch_may_exceed_the_context_when_each_input_fits() {
        let client = Arc::new(Scripted::new([]));
        let gateway = gateway(client);
        let mut policy = policy();
        policy.embedding_dimension = Some(2);
        let batch = vec![
            "x".repeat(3_000_000),
            "y".repeat(3_000_000),
            "z".repeat(3_000_000),
        ];
        assert_eq!(
            gateway.embed(&policy, &batch).await.unwrap().vectors.len(),
            3
        );
        let too_big = vec!["x".repeat(9_000_000)];
        assert!(matches!(
            gateway.embed(&policy, &too_big).await,
            Err(GatewayError::NoRoute(_))
        ));
    }

    #[tokio::test]
    async fn embedding_routes_by_width() {
        let client = Arc::new(Scripted::new([]));
        let gateway = gateway(client);
        let mut policy = policy();
        policy.embedding_dimension = Some(2);
        let done = gateway.embed(&policy, &["a".to_owned()]).await.unwrap();
        assert_eq!((done.model.as_str(), done.vectors.len()), ("first", 1));
        policy.embedding_dimension = Some(9);
        assert!(gateway.embed(&policy, &["a".to_owned()]).await.is_err());
    }
}
