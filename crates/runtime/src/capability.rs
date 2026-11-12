//! The `http`, `model`, `memory` and `jobs` capabilities a plugin can be
//! granted, made from the runtime's own parts: HTTP is the `http_request`
//! tool, with its network rules; model calls go through the gateway, routed
//! and charged like the runtime's own; notes go into the runtime's own
//! memory; and workflow runs are queued as the REST API queues them.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use memory::{Memory, NewNote, NoteOrigin};
use model::{ChatRequest, Gateway, Message, RoutePolicy};
use plugin_api::{
    CapabilityError, CapabilityUse, HttpCapability, HttpRequest, HttpResponse, JobPhase,
    JobStarted, JobState, JobsCapability, MAX_IDEMPOTENCY_KEY, MAX_OUTSTANDING_JOBS,
    MemoryCapability, ModelCapability, ModelReply, NoteStored, NoteToStore, PluginCapability,
    RunWorkflow, ToolContext,
};
use plugin_host::{CapabilityMeter, CapabilitySource};
use protocol::{Actor, Trust};
use serde_json::json;
use store::{Job, JobId, JobStatus, PageRequest, Store};
use tools::HttpRequestTool;
use uuid::Uuid;
use work::{Enqueued, JobTransport, StoreTransport};

use crate::jobs::WORKFLOW_KIND;

/// Makes both capabilities for any plugin that's granted them.
#[derive(Debug)]
pub(crate) struct RuntimeCapabilities {
    pub(crate) http: Arc<HttpRequestTool>,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) max_output_tokens: u32,
    pub(crate) memory: Memory,
    pub(crate) store: Arc<dyn Store>,
}

impl CapabilitySource for RuntimeCapabilities {
    fn http(&self, _plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn HttpCapability> {
        Arc::new(PluginHttp {
            tool: Arc::clone(&self.http),
            meter,
        })
    }

    fn model(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn ModelCapability> {
        Arc::new(PluginModel {
            gateway: Arc::clone(&self.gateway),
            purpose: purpose(plugin),
            meter,
            max_output_tokens: self.max_output_tokens,
        })
    }

    fn memory(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn MemoryCapability> {
        Arc::new(PluginMemory {
            memory: self.memory.clone(),
            stored_by: Actor::new(purpose(plugin)),
            meter,
        })
    }

    fn jobs(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn JobsCapability> {
        Arc::new(PluginJobs {
            store: Arc::clone(&self.store),
            plugin: plugin.to_owned(),
            requested_by: Actor::new(purpose(plugin)),
            meter,
        })
    }
}

/// What a plugin's model calls are for, which a paid provider's `paid_for`
/// list has to name before the plugin may pay.
fn purpose(plugin: &str) -> String {
    format!("plugin:{plugin}")
}

/// Refuses a capability call once the run has no tool calls left: each one
/// counts as a tool call.
fn check_calls_left(meter: &CapabilityMeter, context: ToolContext) -> Result<(), CapabilityError> {
    if meter.calls(context.run) >= context.tool_calls_left {
        return Err(CapabilityError::Refused(
            "the run has no tool calls left, and a capability call counts as one".to_owned(),
        ));
    }
    Ok(())
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug)]
struct PluginHttp {
    tool: Arc<HttpRequestTool>,
    meter: Arc<CapabilityMeter>,
}

#[async_trait]
impl HttpCapability for PluginHttp {
    async fn request(
        &self,
        request: HttpRequest,
        context: ToolContext,
    ) -> Result<HttpResponse, CapabilityError> {
        check_calls_left(&self.meter, context)?;
        let what = schema::bounded(&format!(
            "{} {}",
            request.method.to_uppercase(),
            request.url
        ));
        let begun = Instant::now();
        let answer = self
            .tool
            .request_for_plugin(request)
            .await
            .map_err(|failure| CapabilityError::Refused(failure.message().to_owned()));
        let used = CapabilityUse::new(PluginCapability::Http, what, answer.is_ok())
            .with_duration_ms(millis(begun));
        self.meter.record(context.run, used);
        answer
    }
}

#[derive(Debug)]
struct PluginModel {
    gateway: Arc<Gateway>,
    purpose: String,
    meter: Arc<CapabilityMeter>,
    max_output_tokens: u32,
}

#[async_trait]
impl ModelCapability for PluginModel {
    async fn complete(
        &self,
        prompt: &str,
        context: ToolContext,
    ) -> Result<ModelReply, CapabilityError> {
        // One call at a time per run, so what's left is known when it's
        // checked: what the run had, less what this plugin has spent in the
        // call that's still running.
        let _turn = self.meter.turn(context.run).await;
        check_calls_left(&self.meter, context)?;
        let begun = Instant::now();
        let budget = context
            .budget_usd_micros
            .saturating_sub(self.meter.pending(context.run));
        let policy = RoutePolicy {
            capabilities: Vec::new(),
            data_class: context.data_class,
            purpose: self.purpose.clone(),
            budget_usd_micros: Some(budget),
            embedding_dimension: None,
            only_model: None,
        };
        let request = ChatRequest {
            messages: vec![Message::user(prompt)],
            tools: Vec::new(),
            max_output_tokens: self.max_output_tokens,
            temperature: None,
            response_schema: None,
        };
        let completion = match self.gateway.chat(&policy, &request).await {
            Ok(completion) => completion,
            Err(error) => {
                let used = CapabilityUse::new(PluginCapability::Model, "no model answered", false)
                    .with_cost(error.cost_usd_micros())
                    .with_duration_ms(millis(begun));
                self.meter.record(context.run, used);
                return Err(CapabilityError::Failed(schema::bounded(&error.to_string())));
            }
        };
        let answered = !completion.response.content.trim().is_empty();
        let used = CapabilityUse::new(PluginCapability::Model, &completion.model, answered)
            .with_cost(completion.cost_usd_micros)
            .with_duration_ms(millis(begun));
        self.meter.record(context.run, used);
        if !answered {
            return Err(CapabilityError::Failed(
                "the model's reply was empty".to_owned(),
            ));
        }
        Ok(ModelReply::new(
            completion.response.content,
            completion.model,
            completion.cost_usd_micros,
        ))
    }
}

/// The longest title made from a note's first line, when it has none.
const DERIVED_TITLE: usize = 80;

#[derive(Debug)]
struct PluginMemory {
    memory: Memory,
    stored_by: Actor,
    meter: Arc<CapabilityMeter>,
}

#[async_trait]
impl MemoryCapability for PluginMemory {
    async fn store(
        &self,
        note: NoteToStore,
        context: ToolContext,
    ) -> Result<NoteStored, CapabilityError> {
        // One at a time per run, as model calls are, so what's left to
        // spend on embedding is known when it's checked.
        let _turn = self.meter.turn(context.run).await;
        check_calls_left(&self.meter, context)?;
        note.check()?;
        // A Harness² probe has no side effects: the note passes its checks
        // and isn't kept, as a probe's other writes aren't.
        if context.probing {
            return Ok(NoteStored::new(uuid::Uuid::new_v4(), true, 0, 0));
        }
        let begun = Instant::now();
        let title = if note.title.trim().is_empty() {
            let first = note.text.lines().find(|line| !line.trim().is_empty());
            first
                .unwrap_or_default()
                .trim()
                .chars()
                .take(DERIVED_TITLE)
                .collect()
        } else {
            note.title
        };
        let what = schema::bounded(&title);
        let budget = context
            .budget_usd_micros
            .saturating_sub(self.meter.pending(context.run));
        let origin = NoteOrigin {
            data_class: context.data_class,
            // What a plugin brings in may be anything, so it's never trusted.
            trust: Trust::Untrusted,
            stored_by: self.stored_by.clone(),
        };
        let new = NewNote {
            title,
            body: note.text,
            tags: note.tags,
            source_url: note.source_url,
        };
        let result = self
            .memory
            .store(new, origin, Some(budget), Utc::now())
            .await;
        let (ok, cost) = match &result {
            Ok(stored) => (true, stored.cost_usd_micros),
            Err(error) => (false, error.cost_usd_micros()),
        };
        let used = CapabilityUse::new(PluginCapability::Memory, what, ok)
            .with_cost(cost)
            .with_duration_ms(millis(begun));
        self.meter.record(context.run, used);
        match result {
            Ok(stored) => Ok(NoteStored::new(
                stored.id.as_uuid(),
                stored.created,
                stored.chunks,
                stored.unembedded_chunks,
            )
            .with_cost(stored.cost_usd_micros)),
            Err(memory::MemoryError::Invalid(why)) => Err(CapabilityError::Refused(why)),
            Err(error) => Err(CapabilityError::Failed(schema::bounded(&error.to_string()))),
        }
    }
}

/// Jobs read per page when counting a plugin's outstanding ones.
const COUNT_PAGE: u32 = 500;

#[derive(Debug)]
struct PluginJobs {
    store: Arc<dyn Store>,
    plugin: String,
    requested_by: Actor,
    meter: Arc<CapabilityMeter>,
}

fn failed(error: impl std::fmt::Display) -> CapabilityError {
    CapabilityError::Failed(schema::bounded(&error.to_string()))
}

impl PluginJobs {
    /// How many of this plugin's jobs are waiting or running. Counting stops
    /// once it reaches the cap, and a queue too long to count to it counts
    /// as full, so a start is never let through on a guess.
    async fn outstanding(&self) -> Result<usize, CapabilityError> {
        let mut mine = 0;
        for status in [JobStatus::Queued, JobStatus::Claimed] {
            let mut offset = 0;
            loop {
                let page = PageRequest::new(COUNT_PAGE, offset).map_err(failed)?;
                let found = self
                    .store
                    .list_jobs(Some(status), page)
                    .await
                    .map_err(failed)?;
                mine += found
                    .items
                    .iter()
                    .filter(|job| job.requested_by == self.requested_by)
                    .count();
                if mine >= MAX_OUTSTANDING_JOBS {
                    return Ok(mine);
                }
                if !found.has_more {
                    break;
                }
                offset = offset.saturating_add(COUNT_PAGE);
                if offset >= COUNT_PAGE.saturating_mul(10) {
                    return Ok(MAX_OUTSTANDING_JOBS);
                }
            }
        }
        Ok(mine)
    }

    /// The job's id: one per key, for this plugin and workflow.
    fn job_id(&self, workflow: &str, key: Option<&str>) -> JobId {
        key.map_or_else(JobId::generate, |key| {
            let seed = format!("plugin|{}|{workflow}|{key}", self.plugin);
            JobId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
        })
    }

    fn check(run: &RunWorkflow) -> Result<(), CapabilityError> {
        if !(-1000..=1000).contains(&run.priority) {
            return Err(CapabilityError::Refused(
                "a priority is from -1000 to 1000".to_owned(),
            ));
        }
        if run
            .idempotency_key
            .as_deref()
            .is_some_and(|key| key.is_empty() || key.chars().count() > MAX_IDEMPOTENCY_KEY)
        {
            return Err(CapabilityError::Refused(format!(
                "an idempotency key is 1 to {MAX_IDEMPOTENCY_KEY} characters"
            )));
        }
        Ok(())
    }

    async fn start(
        &self,
        run: RunWorkflow,
        context: ToolContext,
    ) -> Result<JobStarted, CapabilityError> {
        Self::check(&run)?;
        workflow::check_workflow_run(self.store.as_ref(), &run.workflow, &run.inputs)
            .await
            .map_err(|error| CapabilityError::Refused(schema::bounded(&error.to_string())))?;
        // A Harness² probe starts nothing: the run passes its checks only.
        if context.probing {
            return Ok(JobStarted::new(Uuid::new_v4(), false));
        }
        let id = self.job_id(&run.workflow, run.idempotency_key.as_deref());
        // A start already made returns its job, whatever the count.
        if self.store.get_job(id).await.map_err(failed)?.is_some() {
            return Ok(JobStarted::new(id.as_uuid(), true));
        }
        if self.outstanding().await? >= MAX_OUTSTANDING_JOBS {
            return Err(CapabilityError::Refused(format!(
                "plugin {} has {MAX_OUTSTANDING_JOBS} jobs waiting or running already",
                self.plugin
            )));
        }
        let job = Job {
            id,
            kind: WORKFLOW_KIND.to_owned(),
            payload: json!({ "workflow": run.workflow, "inputs": run.inputs }),
            priority: run.priority,
            status: JobStatus::Queued,
            requested_by: self.requested_by.clone(),
            run_at: Utc::now(),
            attempts: 0,
            // A retry would repeat whatever the earlier steps did.
            max_attempts: 1,
            lease: None,
            last_error: None,
            result: None,
            finished_at: None,
        };
        let enqueued = StoreTransport(Arc::clone(&self.store))
            .enqueue(job)
            .await
            .map_err(failed)?;
        Ok(JobStarted::new(
            id.as_uuid(),
            enqueued == Enqueued::AlreadyThere,
        ))
    }
}

#[async_trait]
impl JobsCapability for PluginJobs {
    async fn run_workflow(
        &self,
        run: RunWorkflow,
        context: ToolContext,
    ) -> Result<JobStarted, CapabilityError> {
        // One start at a time per run, so two can't both pass the count.
        let _turn = self.meter.turn(context.run).await;
        check_calls_left(&self.meter, context)?;
        let begun = Instant::now();
        let what = schema::bounded(&run.workflow);
        let started = self.start(run, context).await;
        let used = CapabilityUse::new(PluginCapability::Jobs, what, started.is_ok())
            .with_duration_ms(millis(begun));
        self.meter.record(context.run, used);
        started
    }

    async fn job(&self, job_id: Uuid, _: ToolContext) -> Result<Option<JobState>, CapabilityError> {
        let found = self
            .store
            .get_job(JobId::from_uuid(job_id))
            .await
            .map_err(failed)?;
        // Another's job is as good as not there.
        let Some(job) = found.filter(|job| job.requested_by == self.requested_by) else {
            return Ok(None);
        };
        let phase = match job.status {
            JobStatus::Queued => JobPhase::Queued,
            JobStatus::Claimed => JobPhase::Running,
            JobStatus::Succeeded => JobPhase::Succeeded,
            JobStatus::Dead => JobPhase::Dead,
        };
        let last_error = job.last_error.map(|error| schema::bounded(&error));
        Ok(Some(
            JobState::new(job_id, phase).with_outcome(last_error, job.result),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#[allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]
mod tests {
    use memory::ChunkConfig;
    use protocol::{DataClass, RunId};
    use store::{ClassFilter, JobQueue, NoteId, NoteReader, Store};
    use store_memory::MemoryStore;

    use super::*;

    #[test]
    fn a_plugins_purpose_carries_its_name() {
        assert_eq!(purpose("feeds"), "plugin:feeds");
    }

    fn plugin_memory() -> (PluginMemory, Arc<MemoryStore>, Arc<CapabilityMeter>) {
        let store = Arc::new(MemoryStore::new(3));
        let memory = Memory::new(
            Arc::clone(&store) as Arc<dyn Store>,
            None,
            ChunkConfig::default(),
        );
        let meter = Arc::new(CapabilityMeter::default());
        let capability = PluginMemory {
            memory,
            stored_by: Actor::new(purpose("feeds")),
            meter: Arc::clone(&meter),
        };
        (capability, store, meter)
    }

    #[tokio::test]
    async fn a_plugins_note_is_kept_untrusted_under_the_runs_class() {
        let (capability, store, meter) = plugin_memory();
        let run = RunId::generate();
        let context = ToolContext::new(run, DataClass::Public, 0);
        let note = NoteToStore::new("", "\n  Otters hold hands.\nWhile they sleep.")
            .with_tags(vec!["otters".to_owned()]);
        let stored = capability.store(note, context).await.unwrap();
        assert!(stored.created);
        let kept = store
            .get_note(NoteId::from_uuid(stored.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            kept.title, "Otters hold hands.",
            "the first line, when untitled"
        );
        assert_eq!(kept.provenance.trust, Trust::Untrusted);
        assert_eq!(kept.provenance.data_class, DataClass::Public);
        assert_eq!(kept.provenance.stored_by.as_str(), "plugin:feeds");
        assert_eq!(meter.calls(run), 1, "it counts as a capability call");
        // Memory's own search finds it.
        let found = capability
            .memory
            .search(
                "otters",
                ClassFilter::PublicOnly,
                memory::EmbedScope {
                    data_class: DataClass::Public,
                    budget_usd_micros: None,
                },
                5,
            )
            .await
            .unwrap();
        assert_eq!(found.hits.len(), 1);
    }

    #[tokio::test]
    async fn a_probe_keeps_nothing() {
        let (capability, store, _) = plugin_memory();
        let context = ToolContext::new(RunId::generate(), DataClass::Private, 0).with_probing(true);
        let stored = capability
            .store(NoteToStore::new("t", "kept only in a real run"), context)
            .await
            .unwrap();
        assert_eq!(
            store.get_note(NoteId::from_uuid(stored.id)).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_note_past_its_limits_or_the_runs_calls_is_refused() {
        let (capability, _, _) = plugin_memory();
        let run = RunId::generate();
        let long = "x".repeat(plugin_api::MAX_NOTE_TEXT + 1);
        let context = ToolContext::new(run, DataClass::Private, 0);
        let refused = capability.store(NoteToStore::new("t", long), context).await;
        assert!(
            matches!(refused, Err(CapabilityError::Refused(_))),
            "{refused:?}"
        );
        let empty = capability.store(NoteToStore::new("t", "  "), context).await;
        assert!(
            matches!(empty, Err(CapabilityError::Refused(_))),
            "{empty:?}"
        );
        let spent = context.with_tool_calls_left(0);
        let refused = capability
            .store(NoteToStore::new("t", "words"), spent)
            .await;
        assert!(
            matches!(&refused, Err(CapabilityError::Refused(m)) if m.contains("tool calls")),
            "{refused:?}"
        );
    }

    fn jobs_for(plugin: &str, store: &Arc<MemoryStore>) -> PluginJobs {
        PluginJobs {
            store: Arc::clone(store) as Arc<dyn Store>,
            plugin: plugin.to_owned(),
            requested_by: Actor::new(purpose(plugin)),
            meter: Arc::new(CapabilityMeter::default()),
        }
    }

    async fn with_workflow() -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::new(3));
        let body = json!({
            "inputs": {
                "type": "object", "additionalProperties": false, "required": ["url"],
                "properties": { "url": { "type": "string", "maxLength": 100 } }
            },
            "steps": [{ "type": "tool", "id": "look", "tool": "memory",
                        "arguments": { "operation": "search", "query": "{{inputs.url}}" } }]
        });
        let definition = store::Definition {
            key: store::DefinitionKey {
                kind: store::DefinitionKind::Workflow,
                name: "learn".to_owned(),
            },
            body,
            source: "test".to_owned(),
            updated_at: Utc::now(),
        };
        store
            .apply(store::UnitOfWork::new().with(store::WriteOp::PutDefinition(definition)))
            .await
            .unwrap();
        store
    }

    fn here() -> ToolContext {
        ToolContext::new(RunId::generate(), DataClass::Private, 0)
    }

    #[tokio::test]
    async fn a_started_run_is_a_workflow_job_of_the_plugins() {
        let store = with_workflow().await;
        let jobs = jobs_for("feeds", &store);
        let run = RunWorkflow::new("learn", json!({ "url": "https://example.com/a" }));
        let started = jobs.run_workflow(run, here()).await.unwrap();
        assert!(!started.already_existed);
        let job = store
            .get_job(JobId::from_uuid(started.job_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.kind, WORKFLOW_KIND);
        assert_eq!(job.requested_by.as_str(), "plugin:feeds");
        assert_eq!(job.max_attempts, 1, "a run is never retried");
        assert_eq!(job.payload["inputs"]["url"], "https://example.com/a");
        let state = jobs.job(started.job_id, here()).await.unwrap().unwrap();
        assert_eq!(state.phase, JobPhase::Queued);
        // Another plugin can't see it.
        let other = jobs_for("other", &store);
        assert_eq!(other.job(started.job_id, here()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_key_starts_a_run_once_per_plugin() {
        let store = with_workflow().await;
        let jobs = jobs_for("feeds", &store);
        let run = || {
            RunWorkflow::new("learn", json!({ "url": "https://example.com/v/1" }))
                .with_idempotency_key("video-1")
        };
        let first = jobs.run_workflow(run(), here()).await.unwrap();
        let again = jobs.run_workflow(run(), here()).await.unwrap();
        assert_eq!(again.job_id, first.job_id);
        assert!(again.already_existed);
        let theirs = jobs_for("other", &store)
            .run_workflow(run(), here())
            .await
            .unwrap();
        assert_ne!(theirs.job_id, first.job_id, "keys don't cross plugins");
    }

    #[tokio::test]
    async fn a_start_that_cant_run_is_refused_and_a_probe_starts_nothing() {
        let store = with_workflow().await;
        let jobs = jobs_for("feeds", &store);
        let refused = |result: Result<JobStarted, CapabilityError>| {
            assert!(
                matches!(result, Err(CapabilityError::Refused(_))),
                "{result:?}"
            );
        };
        refused(
            jobs.run_workflow(RunWorkflow::new("missing", json!({})), here())
                .await,
        );
        refused(
            jobs.run_workflow(RunWorkflow::new("learn", json!({ "url": 5 })), here())
                .await,
        );
        refused(
            jobs.run_workflow(
                RunWorkflow::new("learn", json!({ "url": "u" })).with_priority(5000),
                here(),
            )
            .await,
        );
        let probe = here().with_probing(true);
        let stubbed = jobs
            .run_workflow(RunWorkflow::new("learn", json!({ "url": "u" })), probe)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_job(JobId::from_uuid(stubbed.job_id))
                .await
                .unwrap(),
            None
        );
        // A probe's start is still checked.
        refused(
            jobs.run_workflow(RunWorkflow::new("missing", json!({})), probe)
                .await,
        );
    }

    #[tokio::test]
    async fn a_plugin_with_too_many_jobs_waiting_starts_no_more() {
        let store = with_workflow().await;
        let jobs = jobs_for("feeds", &store);
        for n in 0..MAX_OUTSTANDING_JOBS {
            let run = RunWorkflow::new("learn", json!({ "url": format!("u{n}") }));
            jobs.run_workflow(run, here()).await.unwrap();
        }
        let past = jobs
            .run_workflow(
                RunWorkflow::new("learn", json!({ "url": "one more" })),
                here(),
            )
            .await;
        assert!(
            matches!(&past, Err(CapabilityError::Refused(m)) if m.contains("waiting or running")),
            "{past:?}"
        );
        // Another plugin isn't held up by this one's queue.
        jobs_for("other", &store)
            .run_workflow(RunWorkflow::new("learn", json!({ "url": "u" })), here())
            .await
            .unwrap();
    }
}
