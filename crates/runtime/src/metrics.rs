//! Prometheus metrics, counted from the events every run already writes.
//!
//! [`MetricsSink`] wraps the real sink: each event passes through unchanged
//! and is counted on the way, so the run path has nothing to report. The
//! HTTP layer times requests, and the jobs gauge is read from the store when
//! Prometheus scrapes.
//!
//! A label's values come from definitions, registered tools and the model
//! catalog, but the sink can't tell a real name from a malformed one, so
//! each label keeps at most [`MAX_VALUES`] values and folds the rest into
//! `other`. No run id or argument is ever a label.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use async_trait::async_trait;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use chrono::{DateTime, Utc};
use dispatch::Step;
use events::{Event, EventError, EventPage, EventSink, RunPage, RunQuery};
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;
use protocol::RunId;
use serde::Serialize;
use store::{JobStatus, PageRequest, Store};

/// The most values one label keeps before the rest are counted as `other`.
pub const MAX_VALUES: usize = 256;
/// The longest label value kept.
const MAX_VALUE_CHARS: usize = 128;
/// The most runs whose agent is remembered until they end.
const MAX_OPEN_RUNS: usize = 10_000;
/// How many queued or claimed jobs the gauge counts at most, each.
pub const MAX_JOBS_COUNTED: u32 = 10_000;

type Labels = Vec<(&'static str, String)>;
type Counters = Family<Labels, Counter>;
type Timings = Family<Labels, Histogram, fn() -> Histogram>;

fn seconds() -> Histogram {
    // 10 ms to about 10 minutes.
    Histogram::new(exponential_buckets(0.01, 2.0, 17))
}

fn timings() -> Timings {
    Family::new_with_constructor(seconds as fn() -> Histogram)
}

/// What a value serializes to, as a label: an enum's snake-case word.
fn word<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Every metric, shared by the sink, the HTTP layer and the endpoint.
#[derive(Debug)]
pub struct Metrics {
    runs_started: Counters,
    runs: Counters,
    run_cost: Counters,
    budget_hits: Counters,
    tool_calls: Counters,
    tool_seconds: Timings,
    refusals: Counters,
    model_calls: Counters,
    model_failures: Counters,
    model_tokens: Counters,
    model_seconds: Timings,
    capability_calls: Counters,
    http_requests: Counters,
    http_seconds: Timings,
    sink_failures: Counter,
    jobs: Family<Labels, Gauge>,
    /// Each label's values so far.
    seen: Mutex<HashMap<&'static str, HashSet<String>>>,
    /// Each open run's agent, oldest first, so a run's end is labelled too.
    open: Mutex<(HashMap<RunId, String>, VecDeque<RunId>)>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            runs_started: Family::default(),
            runs: Family::default(),
            run_cost: Family::default(),
            budget_hits: Family::default(),
            tool_calls: Family::default(),
            tool_seconds: timings(),
            refusals: Family::default(),
            model_calls: Family::default(),
            model_failures: Family::default(),
            model_tokens: Family::default(),
            model_seconds: timings(),
            capability_calls: Family::default(),
            http_requests: Family::default(),
            http_seconds: timings(),
            sink_failures: Counter::default(),
            jobs: Family::default(),
            seen: Mutex::new(HashMap::new()),
            open: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a duration in ms as seconds: precision past 2^52 ms doesn't matter"
)]
fn as_seconds(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

impl Metrics {
    /// No metrics counted yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The value as a label: kept if the label has room or has seen it,
    /// otherwise `other`.
    fn label(&self, name: &'static str, value: &str) -> String {
        let value: String = value.chars().take(MAX_VALUE_CHARS).collect();
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let values = seen.entry(name).or_default();
        if values.contains(&value) {
            return value;
        }
        if values.len() < MAX_VALUES {
            values.insert(value.clone());
            return value;
        }
        "other".to_owned()
    }

    fn started(&self, run: RunId, agent: &str) {
        let agent = self.label("agent", agent);
        self.runs_started
            .get_or_create(&vec![("agent", agent.clone())])
            .inc();
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let (agents, order) = &mut *open;
        if agents.insert(run, agent).is_none() {
            order.push_back(run);
        }
        while order.len() > MAX_OPEN_RUNS {
            if let Some(oldest) = order.pop_front() {
                agents.remove(&oldest);
            }
        }
    }

    fn agent_of(&self, run: RunId, ended: bool) -> String {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let (agents, order) = &mut *open;
        let agent = if ended {
            order.retain(|open| *open != run);
            agents.remove(&run)
        } else {
            agents.get(&run).cloned()
        };
        agent.unwrap_or_else(|| "unknown".to_owned())
    }

    /// Counts one event.
    pub fn count(&self, event: &Event) {
        let Ok(step) = serde_json::from_value::<Step>(event.body.clone()) else {
            return;
        };
        match step {
            Step::Started { agent, .. } => self.started(event.run, &agent),
            Step::Message { .. } => {}
            Step::Model {
                model,
                error,
                input_tokens,
                output_tokens,
                duration_ms,
                fell_back,
                ..
            } => {
                for passed in fell_back {
                    let name = self.label("model", &passed.model);
                    let reason = self.label("reason", &passed.reason);
                    self.model_failures
                        .get_or_create(&vec![("model", name), ("reason", reason)])
                        .inc();
                }
                let model = self.label("model", model.as_deref().unwrap_or("none"));
                let ok = if error.is_none() { "true" } else { "false" };
                self.model_calls
                    .get_or_create(&vec![("model", model.clone()), ("ok", ok.to_owned())])
                    .inc();
                for (direction, tokens) in [("input", input_tokens), ("output", output_tokens)] {
                    self.model_tokens
                        .get_or_create(&vec![
                            ("model", model.clone()),
                            ("direction", direction.to_owned()),
                        ])
                        .inc_by(tokens);
                }
                self.model_seconds
                    .get_or_create(&vec![("model", model)])
                    .observe(as_seconds(duration_ms));
            }
            Step::Tool {
                tool,
                status,
                refusal,
                effect,
                duration_ms,
                ..
            } => {
                let tool = self.label("tool", &tool);
                let effect = effect.map_or_else(|| "unknown".to_owned(), |effect| word(&effect));
                self.tool_calls
                    .get_or_create(&vec![
                        ("tool", tool.clone()),
                        ("status", word(&status)),
                        ("effect", effect),
                    ])
                    .inc();
                if let Some(refusal) = refusal {
                    self.refusals
                        .get_or_create(&vec![("tool", tool.clone()), ("kind", word(&refusal))])
                        .inc();
                }
                self.tool_seconds
                    .get_or_create(&vec![("tool", tool)])
                    .observe(as_seconds(duration_ms));
            }
            Step::Capability { capability, ok, .. } => {
                let capability = self.label("capability", &capability);
                self.capability_calls
                    .get_or_create(&vec![("capability", capability), ("ok", ok.to_string())])
                    .inc();
            }
            Step::Ended {
                outcome,
                budget_hit,
                cost_usd_micros,
            } => {
                let agent = self.agent_of(event.run, true);
                self.runs
                    .get_or_create(&vec![("agent", agent.clone()), ("outcome", word(&outcome))])
                    .inc();
                self.run_cost
                    .get_or_create(&vec![("agent", agent.clone())])
                    .inc_by(cost_usd_micros);
                if let Some(kind) = budget_hit {
                    self.budget_hits
                        .get_or_create(&vec![("agent", agent), ("kind", word(&kind))])
                        .inc();
                }
            }
        }
    }

    /// Times one HTTP request. `route` is the route's pattern, never the
    /// path asked for.
    pub fn request(&self, route: &str, status: u16, started: Instant) {
        let labels = vec![
            ("route", self.label("route", route)),
            ("status", status.to_string()),
        ];
        self.http_requests.get_or_create(&labels).inc();
        self.http_seconds
            .get_or_create(&labels)
            .observe(started.elapsed().as_secs_f64());
    }

    /// Reads how many jobs are queued and claimed, up to
    /// [`MAX_JOBS_COUNTED`] each. A store that fails leaves the last count.
    pub async fn read_jobs(&self, store: &dyn Store) {
        for (status, state) in [
            (JobStatus::Queued, "queued"),
            (JobStatus::Claimed, "claimed"),
        ] {
            let mut counted: u32 = 0;
            let mut failed = false;
            while counted < MAX_JOBS_COUNTED {
                let Ok(page) = PageRequest::new(store::MAX_PAGE_LIMIT, counted) else {
                    break;
                };
                match store.list_jobs(Some(status), page).await {
                    Ok(found) => {
                        counted = counted
                            .saturating_add(u32::try_from(found.items.len()).unwrap_or(u32::MAX));
                        if !found.has_more {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("metrics: couldn't count jobs: {error}");
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                self.jobs
                    .get_or_create(&vec![("state", state.to_owned())])
                    .set(i64::from(counted.min(MAX_JOBS_COUNTED)));
            }
        }
    }

    /// The metrics in the `OpenMetrics` text format, each name after
    /// `prefix_`.
    #[must_use]
    pub fn render(&self, prefix: &str) -> String {
        let mut registry = Registry::with_prefix(prefix);
        registry.register(
            "runs_started",
            "Runs started, by what ran",
            self.runs_started.clone(),
        );
        registry.register("runs", "Runs ended, by what ran and how", self.runs.clone());
        registry.register(
            "run_cost_usd_micros",
            "What ended runs cost, in millionths of a dollar",
            self.run_cost.clone(),
        );
        registry.register(
            "run_budget_hits",
            "Runs stopped by a limit, by the limit",
            self.budget_hits.clone(),
        );
        registry.register(
            "tool_calls",
            "Tool calls, by tool, what became of each and whether it writes",
            self.tool_calls.clone(),
        );
        registry.register(
            "tool_call_duration_seconds",
            "How long tool calls took",
            self.tool_seconds.clone(),
        );
        registry.register(
            "tool_refusals",
            "Tool calls refused, by why",
            self.refusals.clone(),
        );
        registry.register(
            "model_calls",
            "Model calls, by the model that answered (none when none did)",
            self.model_calls.clone(),
        );
        registry.register(
            "model_failures",
            "Models a call passed over before one answered, or before it gave up: \
             failed, or left alone while cooling, by reason",
            self.model_failures.clone(),
        );
        registry.register(
            "model_tokens",
            "Model tokens, in and out",
            self.model_tokens.clone(),
        );
        registry.register(
            "model_call_duration_seconds",
            "How long model calls took, retries and fallbacks included",
            self.model_seconds.clone(),
        );
        registry.register(
            "capability_calls",
            "Calls plugins made through a capability",
            self.capability_calls.clone(),
        );
        registry.register(
            "http_requests",
            "HTTP requests, by route and status",
            self.http_requests.clone(),
        );
        registry.register(
            "http_request_duration_seconds",
            "How long HTTP requests took",
            self.http_seconds.clone(),
        );
        registry.register(
            "sink_append_failures",
            "Event appends the sink failed, each a lost piece of a trajectory",
            self.sink_failures.clone(),
        );
        registry.register(
            "jobs",
            "Jobs queued or claimed, read when scraped",
            self.jobs.clone(),
        );
        let mut out = String::new();
        if let Err(error) = encode(&mut out, &registry) {
            let _ = writeln!(out, "# metrics couldn't be written: {error}");
        }
        out
    }
}

/// The metric name prefix for a product name: lower case, and anything but
/// a letter, digit or underscore made an underscore.
#[must_use]
pub fn prefix_for(product: &str) -> String {
    let prefix: String = product
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if prefix.starts_with(|c: char| c.is_ascii_alphabetic()) {
        prefix
    } else {
        format!("r_{prefix}")
    }
}

/// The event sink with every event counted on its way through.
#[derive(Debug)]
pub struct MetricsSink {
    inner: Arc<dyn EventSink>,
    metrics: Arc<Metrics>,
}

impl MetricsSink {
    /// `inner`, counted into `metrics`.
    #[must_use]
    pub fn new(inner: Arc<dyn EventSink>, metrics: Arc<Metrics>) -> Self {
        Self { inner, metrics }
    }
}

#[async_trait]
impl EventSink for MetricsSink {
    async fn append(&self, events: Vec<Event>) -> Result<(), EventError> {
        for event in &events {
            self.metrics.count(event);
        }
        let appended = self.inner.append(events).await;
        if appended.is_err() {
            self.metrics.sink_failures.inc();
        }
        appended
    }

    async fn read_run(
        &self,
        run: RunId,
        offset: usize,
        limit: usize,
    ) -> Result<EventPage, EventError> {
        self.inner.read_run(run, offset, limit).await
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, EventError> {
        self.inner.list_runs(query).await
    }

    async fn prune_runs(&self, before: DateTime<Utc>, limit: u32) -> Result<u32, EventError> {
        self.inner.prune_runs(before, limit).await
    }
}

/// The HTTP layer: times each request under its route's pattern, or
/// `other` for a path no route matched.
pub(crate) async fn timed(metrics: Arc<Metrics>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "other".to_owned(), |path| path.as_str().to_owned());
    let response = next.run(request).await;
    metrics.request(&route, response.status().as_u16(), started);
    response
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

    use super::*;
    use dispatch::{CallStatus, Outcome};
    use events::{EventKind, MemorySink};
    use protocol::DataClass;

    fn event(run: RunId, step: &Step) -> Event {
        Event {
            run,
            at: Utc::now(),
            kind: step.kind(),
            data_class: DataClass::Private,
            body: step.body(),
        }
    }

    fn started(agent: &str) -> Step {
        serde_json::from_value(serde_json::json!({
            "type": "started", "agent": agent, "harness": "h", "purpose": "test",
            "offered": [], "budget": {"max_rounds": 1, "max_tool_calls": 1, "max_writes": 0,
            "max_total_tokens": 1, "max_usd_micros": 0, "max_request_bytes": 1,
            "max_duration_ms": 1}
        }))
        .unwrap()
    }

    fn line<'a>(text: &'a str, start: &str) -> Option<&'a str> {
        text.lines().find(|line| line.starts_with(start))
    }

    #[tokio::test]
    async fn each_event_moves_its_metric_and_passes_through() {
        let inner = Arc::new(MemorySink::new());
        let metrics = Arc::new(Metrics::new());
        let sink = MetricsSink::new(
            Arc::clone(&inner) as Arc<dyn EventSink>,
            Arc::clone(&metrics),
        );
        let run = RunId::generate();
        let steps = [
            started("agent:digest"),
            Step::Model {
                model: Some("local".to_owned()),
                reply: Some("ok".to_owned()),
                tool_calls: 1,
                cost_usd_micros: 0,
                error: None,
                input_tokens: 120,
                output_tokens: 30,
                duration_ms: 1500,
                fell_back: vec![
                    dispatch::FellBack {
                        model: "cloud".to_owned(),
                        reason: "rate_limited".to_owned(),
                    },
                    dispatch::FellBack {
                        model: "spare".to_owned(),
                        reason: "cooling".to_owned(),
                    },
                ],
            },
            Step::Tool {
                index: 0,
                tool: "memory".to_owned(),
                status: CallStatus::Ran,
                refusal: None,
                effect: None,
                by_harness: false,
                arguments: "{}".to_owned(),
                reply: None,
                duration_ms: 40,
            },
            Step::Ended {
                outcome: Outcome::Answered,
                budget_hit: None,
                cost_usd_micros: 7,
            },
        ];
        sink.append(steps.iter().map(|step| event(run, step)).collect())
            .await
            .unwrap();
        assert_eq!(inner.read_run(run, 0, 10).await.unwrap().events.len(), 4);

        let text = metrics.render("rt");
        for expected in [
            r#"rt_runs_started_total{agent="agent:digest"} 1"#,
            r#"rt_runs_total{agent="agent:digest",outcome="answered"} 1"#,
            r#"rt_run_cost_usd_micros_total{agent="agent:digest"} 7"#,
            r#"rt_model_calls_total{model="local",ok="true"} 1"#,
            r#"rt_model_failures_total{model="cloud",reason="rate_limited"} 1"#,
            r#"rt_model_failures_total{model="spare",reason="cooling"} 1"#,
            r#"rt_model_tokens_total{model="local",direction="input"} 120"#,
            r#"rt_model_tokens_total{model="local",direction="output"} 30"#,
            r#"rt_model_call_duration_seconds_count{model="local"} 1"#,
            r#"rt_tool_calls_total{tool="memory",status="ran",effect="unknown"} 1"#,
            r#"rt_tool_call_duration_seconds_count{tool="memory"} 1"#,
        ] {
            assert!(
                line(&text, expected).is_some(),
                "{expected} not in:\n{text}"
            );
        }
        assert!(
            metrics.open.lock().unwrap().0.is_empty(),
            "the run's end forgets it"
        );
    }

    #[test]
    fn a_label_past_its_limit_is_other() {
        let metrics = Metrics::new();
        for n in 0..MAX_VALUES {
            assert_eq!(metrics.label("tool", &format!("t{n}")), format!("t{n}"));
        }
        assert_eq!(metrics.label("tool", "one_more"), "other");
        assert_eq!(metrics.label("tool", "t3"), "t3", "a value seen stays");
        assert_eq!(
            metrics.label("model", "m"),
            "m",
            "each label has its own room"
        );
        let long = "x".repeat(500);
        assert_eq!(
            metrics.label("model", &long).chars().count(),
            MAX_VALUE_CHARS
        );
    }

    #[test]
    fn an_unreadable_event_is_passed_over() {
        let metrics = Metrics::new();
        metrics.count(&Event {
            run: RunId::generate(),
            at: Utc::now(),
            kind: EventKind::ToolCall,
            data_class: DataClass::Private,
            body: serde_json::json!({"n": 1}),
        });
        assert!(!metrics.render("rt").contains("rt_tool_calls_total{"));
    }

    #[test]
    fn the_prefix_is_a_metric_name() {
        assert_eq!(prefix_for("My-Brain 2"), "my_brain_2");
        assert_eq!(prefix_for("9lives"), "r_9lives");
    }
}
