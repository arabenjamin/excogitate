//! The `http`, `model` and `memory` capabilities a plugin can be granted,
//! made from the runtime's own parts: HTTP is the `http_request` tool, with
//! its network rules; model calls go through the gateway, routed and charged
//! like the runtime's own; and notes go into the runtime's own memory.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use memory::{Memory, NewNote, NoteOrigin};
use model::{ChatRequest, Gateway, Message, RoutePolicy};
use plugin_api::{
    CapabilityError, CapabilityUse, HttpCapability, HttpRequest, HttpResponse, MemoryCapability,
    ModelCapability, ModelReply, NoteStored, NoteToStore, PluginCapability, ToolContext,
};
use plugin_host::{CapabilityMeter, CapabilitySource};
use protocol::{Actor, Trust};
use tools::HttpRequestTool;

/// Makes both capabilities for any plugin that's granted them.
#[derive(Debug)]
pub(crate) struct RuntimeCapabilities {
    pub(crate) http: Arc<HttpRequestTool>,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) max_output_tokens: u32,
    pub(crate) memory: Memory,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
mod tests {
    use memory::ChunkConfig;
    use protocol::{DataClass, RunId};
    use store::{ClassFilter, NoteId, NoteReader, Store};
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
}
