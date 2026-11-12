//! Sources: what plugins offer memory to read, searched beside the store and
//! fused with it by rank. A source never writes, and is never written.
//!
//! Privacy is checked here, whatever a source says: an item from a source
//! not declared public is private, and an item the run can't see is
//! dropped. A source that fails or runs out of time is skipped and named.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use plugin_api::{
    MAX_HIT_TEXT, MAX_ITEM_ID, MAX_ITEM_TEXT, MAX_ITEM_TITLE, MAX_ITEM_URL, MAX_SOURCE_HITS,
    Source, SourceFound, SourceItem, SourceQuery, ToolContext, ToolFailure,
};
use protocol::{DataClass, RunId};
use schema::bounded;
use store::{ChunkHit, ChunkId, ClassFilter, PageRequest};

use super::{MAX_SEARCH_LIMIT, Memory, MemoryError, NoteHit, SearchMode, StoreLists, visible_to};
use crate::fuse::fuse_keys;

/// A source memory reads, as the runtime installed it.
#[derive(Debug, Clone)]
pub struct LakeSource {
    /// What memory calls it: `plugin.name`.
    pub id: String,
    /// Whether its items may carry their own class. Otherwise all are
    /// private.
    pub public: bool,
    /// Whether it can be searched by words.
    pub words: bool,
    /// The embedding models, as the catalog names them, whose vectors it
    /// takes; empty if it takes none.
    pub vectors_from: BTreeSet<String>,
    /// The width of the vectors it takes.
    pub dimension: Option<usize>,
    /// How long one search of it may take.
    pub time_limit: Duration,
    /// The source.
    pub source: Arc<dyn Source>,
}

/// The sources each run may read, for the runs an agent definition limits.
/// A run not listed reads them all.
#[derive(Debug, Clone, Default)]
pub struct SourceChoices(Arc<Mutex<HashMap<RunId, Arc<[String]>>>>);

/// Holds a run's choice of sources until it's dropped.
#[derive(Debug)]
pub struct ChoiceHeld {
    choices: SourceChoices,
    run: RunId,
}

impl Drop for ChoiceHeld {
    fn drop(&mut self) {
        self.choices.held().remove(&self.run);
    }
}

impl SourceChoices {
    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<RunId, Arc<[String]>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Limits `run` to the sources named, for as long as what's returned is
    /// held.
    #[must_use]
    pub fn choose(&self, run: RunId, sources: &[String]) -> ChoiceHeld {
        self.held().insert(run, sources.into());
        ChoiceHeld {
            choices: self.clone(),
            run,
        }
    }

    /// The sources `run` is limited to, if it is.
    #[must_use]
    pub fn of(&self, run: RunId) -> Option<Arc<[String]>> {
        self.held().get(&run).cloned()
    }
}

/// A search that reads sources too.
#[derive(Debug, Clone, Copy)]
pub struct LakeSearch<'a> {
    /// The call the search is made in: its run, class, money and time.
    pub context: ToolContext,
    /// The sources it may read, or all if `None`.
    pub chosen: Option<&'a [String]>,
}

/// An item a source found.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemHit {
    /// The source, as `plugin.name`.
    pub source: String,
    /// The item's id within it.
    pub id: String,
    /// Its title.
    pub title: String,
    /// The part of it that matched.
    pub text: String,
    /// Its class, as the runtime judged it.
    pub data_class: DataClass,
    /// Where it came from.
    pub source_url: Option<String>,
    /// When it last changed, if the source knows.
    pub updated_at: Option<DateTime<Utc>>,
    /// Its fused score, comparable only within this search.
    pub score: f32,
    /// How close it is to the query by vector, if the source searched with
    /// the query's vector.
    pub similarity: Option<f32>,
}

/// One thing a search found: a note, or a source's item.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// A note in the store.
    Note(NoteHit),
    /// An item of a source.
    Item(ItemHit),
}

/// A source that was searched and gave nothing, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSource {
    /// The source, as `plugin.name`.
    pub source: String,
    /// Why.
    pub why: String,
}

/// What a search of the store and the sources found.
#[derive(Debug, Clone, PartialEq)]
pub struct LakeResults {
    /// Notes and items, best first.
    pub hits: Vec<Found>,
    /// How the store was searched.
    pub mode: SearchMode,
    /// True when more matched than are listed.
    pub has_more: bool,
    /// Matches left out for not being close enough.
    pub not_close_enough: u32,
    /// What the embedding and the sources cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// The sources searched that gave nothing, and why.
    pub sources_skipped: Vec<SkippedSource>,
}

/// One item read whole, or none.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedItem {
    /// The item, bounded, with its class as the runtime judged it, or
    /// `None` if there's no such item the run may see.
    pub item: Option<SourceItem>,
    /// True if its text was longer than what's kept.
    pub truncated: bool,
    /// What reading it cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
}

/// What one source gave a search.
struct Searched {
    items: Vec<SourceItem>,
    asked: u32,
    cost: u64,
    skipped: Option<String>,
}

/// What ranks in the fusion: a chunk, or a source's item by its place in
/// the source list and its id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    Chunk(ChunkId),
    Item(usize, String),
}

fn cut(text: String, most: usize) -> (String, bool) {
    if text.chars().nth(most).is_some() {
        (text.chars().take(most).collect(), true)
    } else {
        (text, false)
    }
}

/// An item as memory keeps it: bounded, its class judged, its similarity
/// kept only when it could be real.
fn kept(
    source: &LakeSource,
    item: SourceItem,
    text_most: usize,
    vector_sent: bool,
) -> (SourceItem, bool) {
    let class = if source.public {
        item.data_class
    } else {
        DataClass::Private
    };
    let (title, _) = cut(item.title, MAX_ITEM_TITLE);
    let (text, truncated) = cut(item.text, text_most);
    let mut judged = SourceItem::new(item.id, title, text).with_data_class(class);
    judged.source_url = item
        .source_url
        .filter(|url| url.chars().count() <= MAX_ITEM_URL);
    judged.updated_at = item.updated_at;
    judged.similarity = item
        .similarity
        .filter(|close| vector_sent && close.is_finite())
        .map(|close| close.clamp(-1.0, 1.0));
    (judged, truncated)
}

fn usable_id(id: &str) -> bool {
    !id.is_empty() && id.chars().count() <= MAX_ITEM_ID
}

fn took_too_long(limit: Duration) -> String {
    format!("it took longer than {} ms", limit.as_millis())
}

impl LakeSource {
    /// The query's vector, if this source takes it.
    fn takes<'a>(&self, vector: Option<&'a (Vec<f32>, String)>) -> Option<&'a Vec<f32>> {
        vector.and_then(|(vector, model)| {
            (self.vectors_from.contains(model) && self.dimension == Some(vector.len()))
                .then_some(vector)
        })
    }

    /// The time a call to it may take, inside the call's own.
    fn limit(&self, context: ToolContext) -> Duration {
        self.time_limit
            .min(Duration::from_millis(context.time_left_ms))
    }

    async fn search(
        &self,
        text: &str,
        vector: Option<&(Vec<f32>, String)>,
        asked: u32,
        classes: ClassFilter,
        context: ToolContext,
    ) -> Searched {
        let sent = self.takes(vector);
        let nothing = |cost: u64, why: String| Searched {
            items: Vec::new(),
            asked,
            cost,
            skipped: Some(bounded(&why)),
        };
        if sent.is_none() && !self.words {
            return nothing(
                0,
                "it's searched only by vectors, and the query wasn't embedded by a model it takes"
                    .to_owned(),
            );
        }
        let mut query = SourceQuery::new(text, asked);
        if let Some(vector) = sent {
            query = query.with_vector(vector.clone());
        }
        let limit = self.limit(context);
        let found = match tokio::time::timeout(limit, self.source.search(query, context)).await {
            Err(_) => return nothing(0, took_too_long(limit)),
            Ok(Err(failure)) => {
                return nothing(failure.cost_usd_micros(), failure.message().to_owned());
            }
            Ok(Ok(found)) => found,
        };
        let mut seen = HashSet::new();
        let items = found
            .hits
            .into_iter()
            .take(usize::try_from(asked).unwrap_or(usize::MAX))
            .filter(|item| usable_id(&item.id) && seen.insert(item.id.clone()))
            .map(|item| kept(self, item, MAX_HIT_TEXT, sent.is_some()).0)
            .filter(|item| visible_to(classes, item.data_class))
            .collect();
        Searched {
            items,
            asked,
            cost: found.cost_usd_micros,
            skipped: None,
        }
    }
}

/// The context a source is called with: the call's, less what's spent.
fn less_spent(mut context: ToolContext, spent: u64) -> ToolContext {
    context.budget_usd_micros = context.budget_usd_micros.saturating_sub(spent);
    context
}

impl Memory {
    /// Hands memory the sources it reads. Done once, when plugins are
    /// installed; every copy of this memory sees them.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Invalid`] if it was done already, or two sources
    /// share an id.
    pub fn attach_sources(&self, sources: Vec<LakeSource>) -> Result<(), MemoryError> {
        let mut ids = HashSet::new();
        if let Some(twice) = sources.iter().find(|source| !ids.insert(source.id.clone())) {
            return Err(MemoryError::Invalid(format!(
                "two sources are named {}",
                twice.id
            )));
        }
        self.sources
            .set(sources)
            .map_err(|_| MemoryError::Invalid("memory's sources are set already".to_owned()))
    }

    /// The ids of the sources memory reads.
    #[must_use]
    pub fn source_ids(&self) -> Vec<String> {
        self.lake().iter().map(|source| source.id.clone()).collect()
    }

    fn lake(&self) -> &[LakeSource] {
        self.sources.get().map_or(&[], Vec::as_slice)
    }

    /// The sources a search reads.
    fn chosen<'a>(&'a self, chosen: Option<&[String]>) -> Vec<&'a LakeSource> {
        self.lake()
            .iter()
            .filter(|source| chosen.is_none_or(|names| names.contains(&source.id)))
            .collect()
    }

    /// Searches the store and, with `lake`, the sources it may read, and
    /// fuses them by rank. `min_similarity` applies before the limit; an
    /// item found without the query's vector has no similarity, so a
    /// threshold leaves it out, as it does a note found by words.
    ///
    /// # Errors
    ///
    /// As [`Memory::search`]. A source that fails isn't an error: the
    /// result names it in `sources_skipped`.
    pub async fn search_lake(
        &self,
        query: &str,
        classes: ClassFilter,
        scope: crate::EmbedScope,
        limit: u32,
        min_similarity: Option<f32>,
        lake: Option<LakeSearch<'_>>,
    ) -> Result<LakeResults, MemoryError> {
        if limit == 0 || limit > MAX_SEARCH_LIMIT {
            return Err(MemoryError::Invalid(format!(
                "limit must be 1 to {MAX_SEARCH_LIMIT}"
            )));
        }
        if query.trim().is_empty() {
            return Err(MemoryError::Invalid("a search needs a query".to_owned()));
        }
        // A pool wider than the answer: fusing only the top few loses what
        // ranks well in one list and appears in another.
        let pool = PageRequest::new((limit * 3).max(10), 0)?;
        let stored = self.store_lists(query, classes, scope, pool).await?;
        let sources = lake.map_or_else(Vec::new, |lake| self.chosen(lake.chosen));
        let asked = pool.limit().min(MAX_SOURCE_HITS);
        let context = lake.map(|lake| less_spent(lake.context, stored.cost));
        let searched = match context {
            Some(context) => {
                join_all(sources.iter().map(|source| {
                    source.search(query, stored.vector.as_ref(), asked, classes, context)
                }))
                .await
            }
            None => Vec::new(),
        };
        self.ranked(stored, &sources, searched, limit, min_similarity)
            .await
    }

    /// The store's lists and the sources' fused, walked in fused order,
    /// cut to the limit, and filled in.
    async fn ranked(
        &self,
        stored: StoreLists,
        sources: &[&LakeSource],
        searched: Vec<Searched>,
        limit: u32,
        min_similarity: Option<f32>,
    ) -> Result<LakeResults, MemoryError> {
        let chunks: HashMap<ChunkId, &ChunkHit> = stored
            .text
            .items
            .iter()
            .chain(&stored.vectors.items)
            .map(|hit| (hit.chunk.id, hit))
            .collect();
        let similarity: HashMap<ChunkId, f32> = stored
            .vectors
            .items
            .iter()
            .map(|hit| (hit.chunk.id, hit.score))
            .collect();
        let mut lists: Vec<Vec<Key>> = vec![
            stored
                .text
                .items
                .iter()
                .map(|hit| Key::Chunk(hit.chunk.id))
                .collect(),
            stored
                .vectors
                .items
                .iter()
                .map(|hit| Key::Chunk(hit.chunk.id))
                .collect(),
        ];
        let mut items: HashMap<Key, &SourceItem> = HashMap::new();
        let (mut cost, mut skipped) = (stored.cost, Vec::new());
        let mut has_more = stored.text.has_more || stored.vectors.has_more;
        for (place, (source, found)) in sources.iter().zip(&searched).enumerate() {
            cost = cost.saturating_add(found.cost);
            if let Some(why) = &found.skipped {
                skipped.push(SkippedSource {
                    source: source.id.clone(),
                    why: why.clone(),
                });
            }
            has_more |= found.items.len() >= usize::try_from(found.asked).unwrap_or(usize::MAX);
            let keys = found.items.iter().map(|item| {
                let key = Key::Item(place, item.id.clone());
                items.insert(key.clone(), item);
                key
            });
            lists.push(keys.collect());
        }

        // Walk in fused order: rebuilding from any other order loses the rank.
        let (mut notes_seen, mut best, mut not_close_enough) = (HashSet::new(), Vec::new(), 0_u32);
        for (key, score) in fuse_keys(&lists) {
            let close = match &key {
                Key::Chunk(id) => {
                    let Some(hit) = chunks.get(id) else { continue };
                    if !notes_seen.insert(hit.chunk.note_id) {
                        continue;
                    }
                    similarity.get(id).copied()
                }
                Key::Item(..) => items.get(&key).and_then(|item| item.similarity),
            };
            if min_similarity.is_some_and(|min| close.is_none_or(|close| close < min)) {
                not_close_enough = not_close_enough.saturating_add(1);
                continue;
            }
            best.push((key, score, close));
        }
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX);
        has_more |= best.len() > wanted;
        best.truncate(wanted);

        let hits = self.filled(best, &chunks, &items, sources).await?;
        Ok(LakeResults {
            hits,
            mode: stored.mode,
            has_more,
            not_close_enough,
            cost_usd_micros: cost,
            sources_skipped: skipped,
        })
    }

    /// The hits ranked best, filled in: notes from the store, items as
    /// their source gave them.
    async fn filled(
        &self,
        best: Vec<(Key, f32, Option<f32>)>,
        chunks: &HashMap<ChunkId, &ChunkHit>,
        items: &HashMap<Key, &SourceItem>,
        sources: &[&LakeSource],
    ) -> Result<Vec<Found>, MemoryError> {
        let mut hits = Vec::with_capacity(best.len());
        for (key, score, close) in best {
            match key {
                Key::Chunk(id) => {
                    let Some(hit) = chunks.get(&id) else { continue };
                    // A note deleted since the search is simply gone.
                    if let Some(note) = self.store.get_note(hit.chunk.note_id).await? {
                        hits.push(Found::Note(NoteHit {
                            note_id: note.id,
                            title: note.title,
                            text: hit.chunk.text.clone(),
                            tags: note.tags,
                            source_url: note.source_url,
                            provenance: hit.provenance.clone(),
                            similarity: close,
                            score,
                        }));
                    }
                }
                Key::Item(place, ref id) => {
                    let (Some(source), Some(item)) = (sources.get(place), items.get(&key)) else {
                        continue;
                    };
                    hits.push(Found::Item(ItemHit {
                        source: source.id.clone(),
                        id: id.clone(),
                        title: item.title.clone(),
                        text: item.text.clone(),
                        data_class: item.data_class,
                        source_url: item.source_url.clone(),
                        updated_at: item.updated_at,
                        score,
                        similarity: close,
                    }));
                }
            }
        }
        Ok(hits)
    }

    /// Reads one item of source `source` whole, if the run may read that
    /// source and see the item. `chosen` is the run's choice of sources.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Invalid`] if there's no such source for this run, and
    /// [`MemoryError::Source`] if it failed or ran out of time.
    pub async fn open(
        &self,
        source: &str,
        id: &str,
        classes: ClassFilter,
        lake: LakeSearch<'_>,
    ) -> Result<OpenedItem, MemoryError> {
        let Some(found) = self
            .chosen(lake.chosen)
            .into_iter()
            .find(|candidate| candidate.id == source)
        else {
            return Err(MemoryError::Invalid(format!(
                "no source named {source} is open to this run"
            )));
        };
        if !usable_id(id) {
            return Err(MemoryError::Invalid(format!(
                "an item id is 1 to {MAX_ITEM_ID} characters"
            )));
        }
        let limit = found.limit(lake.context);
        let failed = |why: String, cost: u64| MemoryError::Source {
            name: source.to_owned(),
            error: bounded(&why),
            cost_usd_micros: cost,
        };
        let read: SourceFound =
            match tokio::time::timeout(limit, found.source.get(id, lake.context)).await {
                Err(_) => return Err(failed(took_too_long(limit), 0)),
                Ok(Err(failure)) => return Err(failure_of(&failure, failed)),
                Ok(Ok(read)) => read,
            };
        let visible = read
            .hits
            .into_iter()
            .find(|item| item.id == id)
            .map(|item| kept(found, item, MAX_ITEM_TEXT, false))
            .filter(|(item, _)| visible_to(classes, item.data_class));
        let truncated = visible.as_ref().is_some_and(|(_, truncated)| *truncated);
        Ok(OpenedItem {
            item: visible.map(|(item, _)| item),
            truncated,
            cost_usd_micros: read.cost_usd_micros,
        })
    }
}

fn failure_of(failure: &ToolFailure, failed: impl Fn(String, u64) -> MemoryError) -> MemoryError {
    failed(failure.message().to_owned(), failure.cost_usd_micros())
}
