//! The `memory` tool: the model's typed way into long-term memory.
//!
//! The arguments are one flat object, because endpoints that speak the
//! `OpenAI` wire format want an object at the root of a tool's schema. Which
//! fields each operation needs is stated with conditionals in that schema.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use memory::{
    EmbedScope, Found, ItemHit, LakeSearch, Memory, MemoryError, NewNote, NoteEdit, NoteHit,
    NoteOrigin, SearchMode, SourceChoices, StoredNote,
};
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::{Actor, DataClass, Trust};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use store::{ClassFilter, Note, NoteId};
use uuid::Uuid;

use crate::clamp::clamped;

const MAX_TITLE: usize = 256;
const MAX_TEXT: usize = 100_000;
const MAX_TAGS: usize = 16;
const MAX_TAG: usize = 64;
const MAX_URL: usize = 2048;
const MAX_REASON: usize = 512;
const DEFAULT_LIMIT: u32 = 5;
/// The longest title made from a note's first line.
const DERIVED_TITLE: usize = 80;

const DESCRIPTION: &str = "Long-term memory. `store` keeps text (needs `text`). \
`search` finds notes by meaning and keywords (needs `query`), and items in the sources plugins \
offer, listed under `sources`. `get` reads one note (needs `id`). `open` reads one source item \
whole (needs `source` and `item`). \
`update` changes one (needs `id` and at least one of `title`, `text`, `tags`). \
`delete` forgets one (needs `id`). Notes written by the owner can't be changed or deleted here.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Store,
    Search,
    Get,
    Update,
    Delete,
    Open,
}

/// The longest source name.
const MAX_SOURCE: usize = 64;
/// The most source items or skipped sources one result lists.
const MAX_LISTED: usize = 50;

/// The fields each operation can't do without.
const REQUIRED: [(&str, &[&str]); 5] = [
    ("store", &["text"]),
    ("search", &["query"]),
    ("get", &["id"]),
    ("delete", &["id"]),
    ("open", &["source", "item"]),
];

/// Adds the per-operation requirements. A field that's required must also
/// be there for real, so `null` doesn't count.
fn require_per_operation(schema: &mut Schema) {
    let mut conditions: Vec<Value> = REQUIRED
        .iter()
        .map(|(operation, fields)| condition(operation, fields))
        .collect();
    conditions.push(condition("update", &["id"]));
    schema.insert("allOf".to_owned(), Value::Array(conditions));
}

fn condition(operation: &str, fields: &[&str]) -> Value {
    let present: serde_json::Map<String, Value> = fields
        .iter()
        .map(|field| ((*field).to_owned(), json!({ "not": { "type": "null" } })))
        .collect();
    json!({
        "if": {
            "properties": { "operation": { "const": operation } },
            "required": ["operation"]
        },
        "then": { "required": fields, "properties": present }
    })
}

/// What the model asks memory to do.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = require_per_operation)]
struct MemoryArgs {
    /// What to do.
    operation: Operation,
    /// A short title. For `store` it defaults to the first line of the text.
    #[schemars(length(max = MAX_TITLE))]
    title: Option<String>,
    /// The text of the note.
    #[schemars(length(min = 1, max = MAX_TEXT))]
    text: Option<String>,
    /// Tags for the note.
    #[schemars(length(max = MAX_TAGS), inner(length(max = MAX_TAG)))]
    tags: Option<Vec<String>>,
    /// The URL the text came from.
    #[schemars(length(max = MAX_URL))]
    source_url: Option<String>,
    /// The note to read, change or delete.
    id: Option<Uuid>,
    /// What to search for.
    #[schemars(length(min = 1, max = 1024))]
    query: Option<String>,
    /// How many notes a search returns. The default is 5.
    #[schemars(range(min = 1, max = memory::MAX_SEARCH_LIMIT))]
    limit: Option<u32>,
    /// Keep only notes at least this close in meaning, from 0 to 1. A note
    /// found by keyword alone has no similarity, so it's left out.
    #[schemars(range(min = 0.0, max = 1.0))]
    min_similarity: Option<f32>,
    /// For `open`: the source the item is in, as a search named it.
    #[schemars(length(min = 1, max = MAX_SOURCE))]
    source: Option<String>,
    /// For `open`: the item's id, as a search gave it.
    #[schemars(length(min = 1, max = plugin_api::MAX_ITEM_ID))]
    item: Option<String>,
}

/// Whether a note may be treated as instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TrustLabel {
    Trusted,
    Untrusted,
}

impl From<Trust> for TrustLabel {
    fn from(trust: Trust) -> Self {
        match trust {
            Trust::Trusted => Self::Trusted,
            Trust::Untrusted => Self::Untrusted,
        }
    }
}

/// One note a search found.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Hit {
    /// The note.
    id: Uuid,
    /// Its title.
    #[schemars(length(max = MAX_TITLE))]
    title: String,
    /// The part of it that matched best.
    #[schemars(length(max = MAX_TEXT))]
    text: String,
    /// Its tags.
    #[schemars(length(max = MAX_TAGS), inner(length(max = MAX_TAG)))]
    tags: Vec<String>,
    /// The URL it came from.
    #[schemars(length(max = MAX_URL))]
    source_url: Option<String>,
    /// Whether it may be treated as instructions.
    trust: TrustLabel,
    /// Its score, comparable only within this search.
    score: f32,
    /// How close it is in meaning to the query, if vector search found it.
    similarity: Option<f32>,
}

/// An item a plugin's source holds. Always untrusted.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct SourceHit {
    /// The source, as `plugin.name`.
    #[schemars(length(max = MAX_SOURCE))]
    source: String,
    /// The item's id within it: `open` reads it whole.
    #[schemars(length(max = plugin_api::MAX_ITEM_ID))]
    id: String,
    /// Its title.
    #[schemars(length(max = MAX_TITLE))]
    title: String,
    /// The part that matched, or all of it when read with `open`.
    #[schemars(length(max = MAX_TEXT))]
    text: String,
    /// Read with `open`: true if the item is longer than `text`. A search
    /// hit's text is only the part that matched.
    truncated: bool,
    /// The URL it came from.
    #[schemars(length(max = MAX_URL))]
    source_url: Option<String>,
    /// When it last changed, if the source knows.
    updated_at: Option<DateTime<Utc>>,
    /// Its score, comparable with the notes' within this search. None when
    /// read with `open`.
    score: Option<f32>,
    /// How close it is in meaning to the query, if the source searched
    /// with the query's vector.
    similarity: Option<f32>,
}

/// A source that was searched and gave nothing.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct SkippedSource {
    /// The source, as `plugin.name`.
    #[schemars(length(max = MAX_SOURCE))]
    source: String,
    /// Why.
    #[schemars(length(max = MAX_REASON))]
    why: String,
}

/// What the tool did.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum MemoryResult {
    /// The note was stored or changed.
    Stored {
        /// The note.
        id: Uuid,
        /// False if it already existed, or was changed.
        created: bool,
        /// How many chunks it has.
        chunks: u32,
        /// How many of them only a keyword search can find.
        unembedded_chunks: u32,
    },
    /// What a search found.
    Found {
        /// The notes, best first.
        #[schemars(length(max = memory::MAX_SEARCH_LIMIT))]
        hits: Vec<Hit>,
        /// Set when vectors weren't used, with why.
        #[schemars(length(max = MAX_REASON))]
        keyword_only_because: Option<String>,
        /// True when more notes matched than are listed.
        has_more: bool,
        /// Notes that matched and were left out for being less similar
        /// than `min_similarity`.
        #[serde(default)]
        not_close_enough: u32,
        /// Items in plugins' sources, ranked with the notes: compare
        /// `score`. `open` lists the one item read here.
        #[schemars(length(max = MAX_LISTED))]
        sources: Vec<SourceHit>,
        /// Sources searched that gave nothing, and why.
        #[schemars(length(max = MAX_LISTED))]
        sources_skipped: Vec<SkippedSource>,
    },
    /// One note.
    Note {
        /// The note.
        id: Uuid,
        /// Its title.
        #[schemars(length(max = MAX_TITLE))]
        title: String,
        /// Its text.
        #[schemars(length(max = MAX_TEXT))]
        text: String,
        /// True if the note is longer than `text`.
        truncated: bool,
        /// Its tags.
        #[schemars(length(max = MAX_TAGS), inner(length(max = MAX_TAG)))]
        tags: Vec<String>,
        /// The URL it came from.
        #[schemars(length(max = MAX_URL))]
        source_url: Option<String>,
        /// Whether it may be treated as instructions.
        trust: TrustLabel,
    },
    /// The note is gone.
    Deleted {
        /// The note.
        id: Uuid,
    },
}

fn tags_for_output(tags: Vec<String>) -> Vec<String> {
    tags.into_iter()
        .take(MAX_TAGS)
        .map(|tag| clamped(tag, MAX_TAG))
        .collect()
}

fn hit(found: NoteHit) -> Hit {
    Hit {
        id: found.note_id.as_uuid(),
        title: clamped(found.title, MAX_TITLE),
        text: clamped(found.text, MAX_TEXT),
        tags: tags_for_output(found.tags),
        source_url: found.source_url.map(|url| clamped(url, MAX_URL)),
        trust: found.provenance.trust.into(),
        score: found.score,
        similarity: found.similarity,
    }
}

fn item_hit(found: ItemHit) -> SourceHit {
    SourceHit {
        source: clamped(found.source, MAX_SOURCE),
        id: found.id,
        title: clamped(found.title, MAX_TITLE),
        text: clamped(found.text, MAX_TEXT),
        truncated: false,
        source_url: found.source_url.map(|url| clamped(url, MAX_URL)),
        updated_at: found.updated_at,
        score: Some(found.score),
        similarity: found.similarity,
    }
}

fn stored(note: &StoredNote) -> MemoryResult {
    MemoryResult::Stored {
        id: note.id.as_uuid(),
        created: note.created,
        chunks: note.chunks,
        unembedded_chunks: note.unembedded_chunks,
    }
}

fn failure(error: &MemoryError) -> ToolFailure {
    ToolFailure::new(schema::bounded(&error.to_string())).with_cost(error.cost_usd_micros())
}

fn missing(what: &str) -> ToolFailure {
    ToolFailure::new(format!("this operation needs `{what}`"))
}

/// The first line of `text`, as a title.
fn title_from(text: &str) -> String {
    clamped(
        text.trim().lines().next().unwrap_or_default().to_owned(),
        DERIVED_TITLE,
    )
}

/// What a run may see: a public run only sees what's public.
const fn classes_for(data_class: DataClass) -> ClassFilter {
    match data_class {
        DataClass::Private => ClassFilter::Any,
        DataClass::Public => ClassFilter::PublicOnly,
    }
}

/// Gives a model long-term memory.
#[derive(Debug, Clone)]
pub struct MemoryTool {
    memory: Memory,
    choices: SourceChoices,
}

impl MemoryTool {
    /// The tool, over this memory.
    #[must_use]
    pub fn new(memory: Memory) -> Self {
        Self {
            memory,
            choices: SourceChoices::default(),
        }
    }

    /// The same tool, reading for each run only the sources `choices` names
    /// for it, if it names any.
    #[must_use]
    pub fn with_choices(mut self, choices: SourceChoices) -> Self {
        self.choices = choices;
        self
    }

    /// The tool's input and output schemas, as published.
    #[must_use]
    pub fn schemas() -> (Value, Value) {
        (
            schema::schema_of::<MemoryArgs>(),
            schema::schema_of::<MemoryResult>(),
        )
    }

    async fn store(
        &self,
        args: MemoryArgs,
        context: ToolContext,
    ) -> Result<(MemoryResult, Trust, u64), ToolFailure> {
        let body = args.text.ok_or_else(|| missing("text"))?;
        let title = args.title.unwrap_or_else(|| title_from(&body));
        let note = NewNote {
            title,
            body,
            tags: args.tags.unwrap_or_default(),
            source_url: args.source_url,
        };
        let origin = NoteOrigin {
            data_class: context.data_class,
            trust: Trust::Untrusted,
            stored_by: Actor::new(format!("run:{}", context.run)),
        };
        let done = self
            .memory
            .store(note, origin, Some(context.budget_usd_micros), Utc::now())
            .await
            .map_err(|error| failure(&error))?;
        Ok((stored(&done), Trust::Trusted, done.cost_usd_micros))
    }

    async fn search(
        &self,
        args: MemoryArgs,
        context: ToolContext,
    ) -> Result<(MemoryResult, Trust, u64), ToolFailure> {
        let query = args.query.ok_or_else(|| missing("query"))?;
        let scope = EmbedScope {
            data_class: context.data_class,
            budget_usd_micros: Some(context.budget_usd_micros),
        };
        let chosen = self.choices.of(context.run);
        let lake = LakeSearch {
            context,
            chosen: chosen.as_deref(),
        };
        let results = self
            .memory
            .search_lake(
                &query,
                classes_for(context.data_class),
                scope,
                args.limit.unwrap_or(DEFAULT_LIMIT),
                args.min_similarity,
                Some(lake),
            )
            .await
            .map_err(|error| failure(&error))?;
        let (mut notes, mut items): (Vec<NoteHit>, Vec<ItemHit>) = (Vec::new(), Vec::new());
        for found in results.hits {
            match found {
                Found::Note(note) => notes.push(note),
                Found::Item(item) => items.push(item),
            }
        }
        // An item from a source is never trusted.
        let all_trusted = items.is_empty()
            && !notes.is_empty()
            && notes
                .iter()
                .all(|found| found.provenance.trust == Trust::Trusted);
        let keyword_only_because = match results.mode {
            SearchMode::Hybrid => None,
            SearchMode::KeywordOnly { reason } => Some(clamped(reason, MAX_REASON)),
        };
        let sources_skipped = results
            .sources_skipped
            .into_iter()
            .take(MAX_LISTED)
            .map(|skipped| SkippedSource {
                source: clamped(skipped.source, MAX_SOURCE),
                why: clamped(skipped.why, MAX_REASON),
            })
            .collect();
        Ok((
            MemoryResult::Found {
                hits: notes.into_iter().map(hit).collect(),
                keyword_only_because,
                has_more: results.has_more,
                not_close_enough: results.not_close_enough,
                sources: items.into_iter().map(item_hit).collect(),
                sources_skipped,
            },
            if all_trusted {
                Trust::Trusted
            } else {
                Trust::Untrusted
            },
            results.cost_usd_micros,
        ))
    }

    /// The note, if this run may see it.
    async fn visible_note(&self, id: NoteId, context: ToolContext) -> Result<Note, ToolFailure> {
        self.memory
            .get(id, classes_for(context.data_class))
            .await
            .map_err(|error| failure(&error))?
            .ok_or_else(|| ToolFailure::new(format!("no note {id} is visible to this run")))
    }

    /// The note, if this run may change it. A prompt injection must not be
    /// able to rewrite or delete what the owner wrote.
    async fn changeable(&self, id: NoteId, context: ToolContext) -> Result<(), ToolFailure> {
        let note = self.visible_note(id, context).await?;
        if note.provenance.trust == Trust::Trusted {
            return Err(ToolFailure::new(format!(
                "note {id} was written by the owner and can't be changed by a tool"
            )));
        }
        Ok(())
    }

    async fn get(
        &self,
        id: NoteId,
        context: ToolContext,
    ) -> Result<(MemoryResult, Trust, u64), ToolFailure> {
        let note = self.visible_note(id, context).await?;
        let trust = note.provenance.trust;
        let truncated = note.body.chars().nth(MAX_TEXT).is_some();
        Ok((
            MemoryResult::Note {
                id: id.as_uuid(),
                title: clamped(note.title, MAX_TITLE),
                text: clamped(note.body, MAX_TEXT),
                truncated,
                tags: tags_for_output(note.tags),
                source_url: note.source_url.map(|url| clamped(url, MAX_URL)),
                trust: trust.into(),
            },
            trust,
            0,
        ))
    }

    async fn update(
        &self,
        id: NoteId,
        args: MemoryArgs,
        context: ToolContext,
    ) -> Result<(MemoryResult, Trust, u64), ToolFailure> {
        let edit = NoteEdit {
            title: args.title,
            body: args.text,
            tags: args.tags,
        };
        if edit.title.is_none() && edit.body.is_none() && edit.tags.is_none() {
            return Err(ToolFailure::new(
                "nothing to change: give `title`, `text` or `tags`".to_owned(),
            ));
        }
        self.changeable(id, context).await?;
        let done = self
            .memory
            .update(
                id,
                edit,
                classes_for(context.data_class),
                Trust::Untrusted,
                Some(context.budget_usd_micros),
                Utc::now(),
            )
            .await
            .map_err(|error| failure(&error))?;
        Ok((stored(&done), Trust::Trusted, done.cost_usd_micros))
    }

    async fn open(
        &self,
        args: MemoryArgs,
        context: ToolContext,
    ) -> Result<(MemoryResult, Trust, u64), ToolFailure> {
        let source = args.source.ok_or_else(|| missing("source"))?;
        let item = args.item.ok_or_else(|| missing("item"))?;
        let chosen = self.choices.of(context.run);
        let lake = LakeSearch {
            context,
            chosen: chosen.as_deref(),
        };
        let opened = self
            .memory
            .open(&source, &item, classes_for(context.data_class), lake)
            .await
            .map_err(|error| failure(&error))?;
        let Some(found) = opened.item else {
            return Err(ToolFailure::new(format!(
                "{source} has no item {item} visible to this run"
            ))
            .with_cost(opened.cost_usd_micros));
        };
        let whole = SourceHit {
            source: clamped(source, MAX_SOURCE),
            id: found.id,
            title: clamped(found.title, MAX_TITLE),
            text: clamped(found.text, MAX_TEXT),
            truncated: opened.truncated,
            source_url: found.source_url.map(|url| clamped(url, MAX_URL)),
            updated_at: found.updated_at,
            score: None,
            similarity: None,
        };
        Ok((
            MemoryResult::Found {
                hits: Vec::new(),
                keyword_only_because: None,
                has_more: false,
                not_close_enough: 0,
                sources: vec![whole],
                sources_skipped: Vec::new(),
            },
            Trust::Untrusted,
            opened.cost_usd_micros,
        ))
    }

    async fn delete(
        &self,
        id: NoteId,
        context: ToolContext,
    ) -> Result<(MemoryResult, Trust, u64), ToolFailure> {
        self.changeable(id, context).await?;
        self.memory
            .delete(id, classes_for(context.data_class))
            .await
            .map_err(|error| failure(&error))?;
        Ok((
            MemoryResult::Deleted { id: id.as_uuid() },
            Trust::Trusted,
            0,
        ))
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::for_types::<MemoryArgs, MemoryResult>("memory", DESCRIPTION, Effect::Write)
    }

    fn effect_of(&self, arguments: &Value) -> Effect {
        match arguments.get("operation").and_then(Value::as_str) {
            Some("search" | "get" | "open") => Effect::Read,
            _ => Effect::Write,
        }
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let args: MemoryArgs = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        let id = args.id.map(NoteId::from_uuid);
        let (result, trust, cost_usd_micros) = match args.operation {
            Operation::Store => self.store(args, context).await?,
            Operation::Search => self.search(args, context).await?,
            Operation::Open => self.open(args, context).await?,
            Operation::Get => self.get(id.ok_or_else(|| missing("id"))?, context).await?,
            Operation::Update => {
                self.update(id.ok_or_else(|| missing("id"))?, args, context)
                    .await?
            }
            Operation::Delete => {
                self.delete(id.ok_or_else(|| missing("id"))?, context)
                    .await?
            }
        };
        let value = serde_json::to_value(result)
            .map_err(|error| ToolFailure::new(schema::bounded(&error.to_string())))?;
        Ok(ToolOutput::new(value, trust).with_cost(cost_usd_micros))
    }
}
