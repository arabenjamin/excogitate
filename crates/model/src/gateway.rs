//! The one way the rest of the runtime calls a model: route, reserve the money,
//! call, settle, and move to the next model when that could help.

use std::fmt;
use std::sync::Arc;

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
    /// What it cost, in millionths of a dollar. When the server doesn't
    /// report usage, the worst case is charged.
    pub cost_usd_micros: u64,
    /// The models that failed first.
    pub failed_attempts: Vec<FailedAttempt>,
}

/// An answered embeddings call.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedded {
    /// One vector per input.
    pub vectors: Vec<Vec<f32>>,
    /// The model that made them.
    pub model: String,
    /// What it cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
}

/// What a successful attempt produced.
struct Answered<T> {
    value: T,
    model: String,
    cost_usd_micros: u64,
    failed_attempts: Vec<FailedAttempt>,
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

/// Routes calls to models.
pub struct Gateway {
    catalog: Catalog,
    client: Arc<dyn ModelClient>,
    ledger: Arc<dyn SpendLedger>,
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
        }
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
    fn open_reservation(&self, candidate: &Candidate, at: DateTime<Utc>) -> Reservation {
        Reservation {
            ledger: Arc::clone(&self.ledger),
            provider: candidate
                .model
                .is_paid()
                .then(|| candidate.model.provider.name.clone()),
            reserved: candidate.worst_case_usd_micros,
            abandoned: candidate.input_usd_micros,
            at,
        }
    }

    /// Tries candidates in order until one answers, keeping the books.
    /// `call` returns the answer and what it really cost.
    async fn first_success<T, F, Fut>(
        &self,
        candidates: Vec<Candidate>,
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
            let open = self.open_reservation(&candidate, reserved_at);
            let answer = call(candidate.clone()).await;
            open.close();
            match answer {
                Ok((value, cost_usd_micros)) => {
                    self.settle(&candidate, cost_usd_micros, reserved_at).await;
                    return Ok(Answered {
                        value,
                        model: model_name,
                        cost_usd_micros,
                        failed_attempts,
                    });
                }
                Err(error) => {
                    self.settle(&candidate, 0, reserved_at).await;
                    let go_on = error.worth_trying_elsewhere();
                    failed_attempts.push(FailedAttempt {
                        model: model_name,
                        error,
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
        let candidates = plan(&self.catalog, &required, policy, estimate)?;
        let client = &self.client;
        let answered = self
            .first_success(candidates, |candidate| async move {
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
        let candidates = plan(&self.catalog, &[Capability::Embeddings], policy, estimate)?;
        let client = &self.client;
        let answered = self
            .first_success(candidates, |candidate| async move {
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
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            gateway.chat(&policy, &request()),
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
        let client = Arc::new(Scripted::new([
            Err(ModelError::Timeout),
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
