//! The memory service: what the `memory` tool and the runtime's own loops use.

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Trust};
use schema::bounded;
use sha2::{Digest, Sha256};
use store::{
    Chunk, ChunkHit, ChunkId, ClassFilter, Note, NoteId, Page, PageRequest, Provenance, Store,
    StoreError, UnitOfWork, WriteOp,
};

use crate::{ChunkConfig, EmbedError, EmbedScope, Embedder, Piece, chunk_text};

mod lake;

pub use lake::{
    ChoiceHeld, Found, ItemHit, LakeResults, LakeSearch, LakeSource, OpenedItem, SkippedSource,
    SourceChoices,
};

/// The most notes one search may return.
pub const MAX_SEARCH_LIMIT: u32 = 50;

/// How many times an input is halved after the embedder refuses it.
const MAX_HALVINGS: u32 = 2;

/// What a caller supplies for a new note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNote {
    /// A short title.
    pub title: String,
    /// The text.
    pub body: String,
    /// Tags.
    pub tags: Vec<String>,
    /// The URL it came from.
    pub source_url: Option<String>,
}

/// What the runtime says about where a note came from. None of it is the model's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteOrigin {
    /// Who may see it.
    pub data_class: DataClass,
    /// Whether it may be treated as instructions.
    pub trust: Trust,
    /// Who's storing it.
    pub stored_by: Actor,
}

/// What a store did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredNote {
    /// The note.
    pub id: NoteId,
    /// False if identical content already existed and this is that note.
    pub created: bool,
    /// How many chunks it has.
    pub chunks: u32,
    /// How many have no vector, and so can only be found by keyword.
    pub unembedded_chunks: u32,
    /// What embedding it cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
}

/// Changes to a note. Empty fields stay as they are.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoteEdit {
    /// A new title.
    pub title: Option<String>,
    /// New text.
    pub body: Option<String>,
    /// New tags.
    pub tags: Option<Vec<String>>,
}

/// How a search was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchMode {
    /// Keywords and vectors.
    Hybrid,
    /// Keywords only, with why.
    KeywordOnly {
        /// Why vectors weren't used.
        reason: String,
    },
}

/// A note that matched.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteHit {
    /// The note.
    pub note_id: NoteId,
    /// Its title.
    pub title: String,
    /// The part of it that matched best.
    pub text: String,
    /// Its tags.
    pub tags: Vec<String>,
    /// The URL it came from.
    pub source_url: Option<String>,
    /// Where it came from, including whether it's trusted.
    pub provenance: Provenance,
    /// Its fused score, comparable only within this search.
    pub score: f32,
    /// How close it is in meaning to the query, from -1 to 1, if vector
    /// search found it. Comparable between searches with the same embedder.
    pub similarity: Option<f32>,
}

/// What a search found.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults {
    /// The notes, best first.
    pub hits: Vec<NoteHit>,
    /// How the search was done.
    pub mode: SearchMode,
    /// True when more notes matched than are listed.
    pub has_more: bool,
    /// Notes that matched and were left out for not being close enough.
    pub not_close_enough: u32,
    /// What embedding the query cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
}

/// A memory operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    /// The request isn't valid.
    #[error("{0}")]
    Invalid(String),
    /// There's no such note, or the caller may not see it.
    #[error("no such note: {0}")]
    NotFound(NoteId),
    /// The store failed or refused.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A source failed or ran out of time.
    #[error("source {name}: {error}")]
    Source {
        /// The source, as `plugin.name`.
        name: String,
        /// What went wrong.
        error: String,
        /// What it was charged, in millionths of a dollar.
        cost_usd_micros: u64,
    },
    /// The note was embedded, then the store failed or refused to save it.
    /// The embedding was paid for all the same.
    #[error("the note wasn't saved: {error}")]
    NotSaved {
        /// Why the store didn't save it.
        error: StoreError,
        /// What embedding it cost, in millionths of a dollar.
        cost_usd_micros: u64,
    },
}

impl MemoryError {
    /// What was spent before it failed, in millionths of a dollar. It's
    /// spent all the same, so a caller keeping accounts must count it.
    #[must_use]
    pub const fn cost_usd_micros(&self) -> u64 {
        match self {
            Self::NotSaved {
                cost_usd_micros, ..
            }
            | Self::Source {
                cost_usd_micros, ..
            } => *cost_usd_micros,
            _ => 0,
        }
    }
}

/// Long-term memory.
#[derive(Debug, Clone)]
pub struct Memory {
    store: Arc<dyn Store>,
    embedder: Option<Arc<dyn Embedder>>,
    chunking: ChunkConfig,
    sources: Arc<OnceLock<Vec<LakeSource>>>,
}

/// The store's two lists for a query, and what making them found out.
struct StoreLists {
    text: Page<ChunkHit>,
    vectors: Page<ChunkHit>,
    mode: SearchMode,
    cost: u64,
    /// The query's vector and the model that made it, if there is one.
    vector: Option<(Vec<f32>, String)>,
}

fn content_hash(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.trim().as_bytes()))
}

fn check_body(body: &str) -> Result<(), MemoryError> {
    if body.trim().is_empty() {
        return Err(MemoryError::Invalid("a note needs some text".to_owned()));
    }
    Ok(())
}

/// The front half of a text, cut on a character boundary.
fn front_half(text: &str) -> String {
    text.chars().take(text.chars().count() / 2).collect()
}

/// `scope`, less what's been spent.
fn with_spent(scope: EmbedScope, spent: u64) -> EmbedScope {
    EmbedScope {
        budget_usd_micros: scope
            .budget_usd_micros
            .map(|left| left.saturating_sub(spent)),
        ..scope
    }
}

const fn visible_to(classes: ClassFilter, data_class: DataClass) -> bool {
    matches!(classes, ClassFilter::Any) || matches!(data_class, DataClass::Public)
}

impl Memory {
    /// Memory over this store. Without an embedder, search uses keywords.
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        embedder: Option<Arc<dyn Embedder>>,
        chunking: ChunkConfig,
    ) -> Self {
        Self {
            store,
            embedder,
            chunking,
            sources: Arc::default(),
        }
    }

    /// Embeds one input alone, halving it if the embedder refuses it. The
    /// chunk keeps its full text either way.
    async fn embed_one(
        &self,
        embedder: &dyn Embedder,
        text: &str,
        scope: EmbedScope,
        spent: &mut u64,
    ) -> Option<Vec<f32>> {
        let mut input = text.to_owned();
        for _ in 0..=MAX_HALVINGS {
            let left = with_spent(scope, *spent);
            match embedder.embed(std::slice::from_ref(&input), left).await {
                Ok(embedding) => {
                    *spent = spent.saturating_add(embedding.cost_usd_micros);
                    return embedding.vectors.into_iter().next();
                }
                Err(error) => {
                    *spent = spent.saturating_add(error.cost_usd_micros);
                    if !error.try_shorter {
                        return None;
                    }
                    input = front_half(&input);
                }
            }
            if input.is_empty() {
                return None;
            }
        }
        None
    }

    /// One vector or `None` per piece, and the cost. Failure costs a chunk
    /// its vector, never the note.
    async fn embed_pieces(
        &self,
        pieces: &[Piece],
        scope: EmbedScope,
    ) -> (Vec<Option<Vec<f32>>>, u64) {
        let Some(embedder) = &self.embedder else {
            return (vec![None; pieces.len()], 0);
        };
        let inputs: Vec<String> = pieces.iter().map(|piece| piece.text.clone()).collect();
        match embedder.embed(&inputs, scope).await {
            Ok(embedding) if embedding.vectors.len() == pieces.len() => (
                embedding.vectors.into_iter().map(Some).collect(),
                embedding.cost_usd_micros,
            ),
            // A batch with the wrong count of vectors was still paid for.
            Ok(embedding) => (vec![None; pieces.len()], embedding.cost_usd_micros),
            Err(EmbedError {
                try_shorter: false,
                cost_usd_micros,
                ..
            }) => (vec![None; pieces.len()], cost_usd_micros),
            Err(EmbedError {
                cost_usd_micros, ..
            }) => {
                let mut spent = cost_usd_micros;
                let mut one_by_one = Vec::with_capacity(pieces.len());
                for piece in pieces {
                    one_by_one.push(
                        self.embed_one(embedder.as_ref(), &piece.text, scope, &mut spent)
                            .await,
                    );
                }
                (one_by_one, spent)
            }
        }
    }

    async fn build_chunks(
        &self,
        note_id: NoteId,
        body: &str,
        scope: EmbedScope,
    ) -> (Vec<Chunk>, u32, u64) {
        let pieces = chunk_text(body, self.chunking);
        let (vectors, cost) = self.embed_pieces(&pieces, scope).await;
        let unembedded = vectors.iter().filter(|vector| vector.is_none()).count();
        let chunks = pieces
            .into_iter()
            .zip(vectors)
            .zip(0u32..)
            .map(|((piece, embedding), ordinal)| Chunk {
                id: ChunkId::generate(),
                note_id,
                ordinal,
                text: piece.text,
                token_count: piece.tokens,
                embedding,
            })
            .collect();
        (chunks, u32::try_from(unembedded).unwrap_or(u32::MAX), cost)
    }

    /// Stores a note. Identical content of the same privacy class returns
    /// the note that already has it. A note of the other class is never
    /// reused: a public run mustn't learn a private note exists, and a
    /// private note mustn't depend on one that public runs can change.
    ///
    /// # Errors
    ///
    /// [`MemoryError`] if the note is empty or the store fails. An embedder
    /// failure isn't an error: it shows in `unembedded_chunks`.
    pub async fn store(
        &self,
        note: NewNote,
        origin: NoteOrigin,
        budget_usd_micros: Option<u64>,
        now: DateTime<Utc>,
    ) -> Result<StoredNote, MemoryError> {
        check_body(&note.body)?;
        let hash = content_hash(&note.body);
        if let Some(existing) = self
            .store
            .find_note_by_hash(&hash, origin.data_class)
            .await?
        {
            let chunks = self.store.list_chunks(existing.id).await?;
            let unembedded = chunks.iter().filter(|c| c.embedding.is_none()).count();
            return Ok(StoredNote {
                id: existing.id,
                created: false,
                chunks: u32::try_from(chunks.len()).unwrap_or(u32::MAX),
                unembedded_chunks: u32::try_from(unembedded).unwrap_or(u32::MAX),
                cost_usd_micros: 0,
            });
        }
        let id = NoteId::generate();
        let scope = EmbedScope {
            data_class: origin.data_class,
            budget_usd_micros,
        };
        let (chunks, unembedded_chunks, cost_usd_micros) =
            self.build_chunks(id, &note.body, scope).await;
        let count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
        let record = Note {
            id,
            title: note.title,
            body: note.body,
            tags: note.tags,
            source_url: note.source_url,
            content_hash: hash,
            provenance: Provenance {
                data_class: origin.data_class,
                trust: origin.trust,
                stored_by: origin.stored_by,
                stored_at: now,
            },
            version: 1,
            updated_at: now,
        };
        let unit =
            UnitOfWork::new()
                .with(WriteOp::CreateNote(record))
                .with(WriteOp::ReplaceChunks {
                    note_id: id,
                    chunks,
                });
        self.store
            .apply(unit)
            .await
            .map_err(|error| MemoryError::NotSaved {
                error,
                cost_usd_micros,
            })?;
        Ok(StoredNote {
            id,
            created: true,
            chunks: count,
            unembedded_chunks,
            cost_usd_micros,
        })
    }

    /// A note, unless it doesn't exist or `classes` hides it.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Store`] if the store fails.
    pub async fn get(&self, id: NoteId, classes: ClassFilter) -> Result<Option<Note>, MemoryError> {
        Ok(self
            .store
            .get_note(id)
            .await?
            .filter(|note| visible_to(classes, note.provenance.data_class)))
    }

    async fn visible(&self, id: NoteId, classes: ClassFilter) -> Result<Note, MemoryError> {
        self.get(id, classes)
            .await?
            .ok_or(MemoryError::NotFound(id))
    }

    /// Changes a note. Its privacy class never changes, and new text from an
    /// untrusted source makes the whole note untrusted.
    ///
    /// # Errors
    ///
    /// [`MemoryError`] if it's hidden or gone, the text is empty, or it
    /// changed since it was read.
    pub async fn update(
        &self,
        id: NoteId,
        edit: NoteEdit,
        classes: ClassFilter,
        trust: Trust,
        budget_usd_micros: Option<u64>,
        now: DateTime<Utc>,
    ) -> Result<StoredNote, MemoryError> {
        let mut note = self.visible(id, classes).await?;
        let expected_version = note.version;
        if let Some(tags) = edit.tags {
            note.tags = tags;
        }
        if let Some(title) = edit.title {
            note.title = title;
        }
        let new_body = edit.body.filter(|body| *body != note.body);
        if let Some(body) = &new_body {
            check_body(body)?;
        }
        if trust == Trust::Untrusted {
            note.provenance.trust = Trust::Untrusted;
        }
        note.updated_at = now;
        let mut unit = UnitOfWork::new();
        let (count, unembedded_chunks, cost_usd_micros) = if let Some(body) = new_body {
            note.content_hash = content_hash(&body);
            note.body.clone_from(&body);
            let scope = EmbedScope {
                data_class: note.provenance.data_class,
                budget_usd_micros,
            };
            let (chunks, unembedded, cost) = self.build_chunks(id, &body, scope).await;
            let count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
            unit = unit
                .with(WriteOp::UpdateNote {
                    note,
                    expected_version,
                })
                .with(WriteOp::ReplaceChunks {
                    note_id: id,
                    chunks,
                });
            (count, unembedded, cost)
        } else {
            let chunks = self.store.list_chunks(id).await?;
            let unembedded = chunks.iter().filter(|c| c.embedding.is_none()).count();
            unit = unit.with(WriteOp::UpdateNote {
                note,
                expected_version,
            });
            (
                u32::try_from(chunks.len()).unwrap_or(u32::MAX),
                u32::try_from(unembedded).unwrap_or(u32::MAX),
                0,
            )
        };
        self.store
            .apply(unit)
            .await
            .map_err(|error| MemoryError::NotSaved {
                error,
                cost_usd_micros,
            })?;
        Ok(StoredNote {
            id,
            created: false,
            chunks: count,
            unembedded_chunks,
            cost_usd_micros,
        })
    }

    /// Forgets a note.
    ///
    /// # Errors
    ///
    /// [`MemoryError`] if it's hidden or gone, or the store fails.
    pub async fn delete(&self, id: NoteId, classes: ClassFilter) -> Result<(), MemoryError> {
        self.visible(id, classes).await?;
        self.store.apply(WriteOp::DeleteNote(id).into()).await?;
        Ok(())
    }

    /// The query's vector, the model that made it, and what it cost, or
    /// why there isn't one.
    async fn query_vector(
        &self,
        query: &str,
        scope: EmbedScope,
    ) -> Result<(Vec<f32>, Option<String>, u64), String> {
        let Some(embedder) = &self.embedder else {
            return Err("no embedder is configured".to_owned());
        };
        let input = [query.to_owned()];
        let embedding = embedder
            .embed(&input, scope)
            .await
            .map_err(|error| format!("the embedder failed: {error}"))?;
        let vector = embedding
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| "the embedder returned nothing".to_owned())?;
        Ok((vector, embedding.model, embedding.cost_usd_micros))
    }

    /// The store's text and vector lists for a query. A vector failure
    /// isn't an error: the lists say keyword-only and why.
    async fn store_lists(
        &self,
        query: &str,
        classes: ClassFilter,
        scope: EmbedScope,
        pool: PageRequest,
    ) -> Result<StoreLists, MemoryError> {
        let text = self.store.search_text(query, classes, pool).await?;
        let nothing = Page {
            items: Vec::new(),
            has_more: false,
        };
        let keyword_only = |reason: &str| SearchMode::KeywordOnly {
            reason: bounded(reason),
        };
        let (vector, model, cost) = match self.query_vector(query, scope).await {
            Ok(made) => made,
            Err(reason) => {
                return Ok(StoreLists {
                    text,
                    vectors: nothing,
                    mode: keyword_only(&reason),
                    cost: 0,
                    vector: None,
                });
            }
        };
        let (vectors, mode) = match self.store.search_vector(&vector, classes, pool).await {
            Ok(page) => (page, SearchMode::Hybrid),
            Err(error) => (
                nothing,
                keyword_only(&format!("vector search failed: {error}")),
            ),
        };
        Ok(StoreLists {
            text,
            vectors,
            mode,
            cost,
            vector: model.map(|model| (vector, model)),
        })
    }

    /// Searches notes. `scope` is the run's own, and decides who may embed
    /// the query. `classes` decides what it may find.
    ///
    /// # Errors
    ///
    /// [`MemoryError`] if the limit is out of range or the store fails. A
    /// vector failure isn't an error: the result says keyword-only and why.
    pub async fn search(
        &self,
        query: &str,
        classes: ClassFilter,
        scope: EmbedScope,
        limit: u32,
    ) -> Result<SearchResults, MemoryError> {
        self.search_close(query, classes, scope, limit, None).await
    }

    /// The same, keeping only notes at least `min_similarity` close to the
    /// query by vector. The threshold applies before the limit, so it never
    /// costs a note that qualifies; a note found only by its words has no
    /// similarity, so a threshold leaves it out. Sources aren't read.
    ///
    /// # Errors
    ///
    /// As [`Memory::search`].
    pub async fn search_close(
        &self,
        query: &str,
        classes: ClassFilter,
        scope: EmbedScope,
        limit: u32,
        min_similarity: Option<f32>,
    ) -> Result<SearchResults, MemoryError> {
        let found = self
            .search_lake(query, classes, scope, limit, min_similarity, None)
            .await?;
        Ok(SearchResults {
            hits: found
                .hits
                .into_iter()
                .filter_map(|hit| match hit {
                    Found::Note(note) => Some(note),
                    Found::Item(_) => None,
                })
                .collect(),
            mode: found.mode,
            has_more: found.has_more,
            not_close_enough: found.not_close_enough,
            cost_usd_micros: found.cost_usd_micros,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::format_collect,
    clippy::cast_possible_truncation,
    reason = "a test on small texts"
)]
mod tests;
