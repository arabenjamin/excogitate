//! The memory service: what the `memory` tool and the brain's own loops use.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Trust};
use schema::bounded;
use sha2::{Digest, Sha256};
use store::{
    Chunk, ChunkHit, ChunkId, ClassFilter, Note, NoteId, Page, PageRequest, Provenance, Store,
    StoreError, UnitOfWork, WriteOp,
};

use crate::{ChunkConfig, EmbedError, EmbedScope, Embedder, Piece, chunk_text, fuse};

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

/// What the brain says about where a note came from. None of it is the model's.
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
}

/// Long-term memory.
#[derive(Debug, Clone)]
pub struct Memory {
    store: Arc<dyn Store>,
    embedder: Option<Arc<dyn Embedder>>,
    chunking: ChunkConfig,
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
                Err(EmbedError {
                    try_shorter: false, ..
                }) => return None,
                Err(_) => input = front_half(&input),
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
            Ok(_)
            | Err(EmbedError {
                try_shorter: false, ..
            }) => (vec![None; pieces.len()], 0),
            Err(_) => {
                let mut spent = 0;
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

    /// Stores a note. Identical content returns the note that already has it.
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
        if let Some(existing) = self.store.find_note_by_hash(&hash).await? {
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
        self.store.apply(unit).await?;
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
        self.store.apply(unit).await?;
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

    /// The vector search, or why there isn't one.
    async fn vector_hits(
        &self,
        query: &str,
        classes: ClassFilter,
        scope: EmbedScope,
        pool: PageRequest,
    ) -> Result<(Page<ChunkHit>, u64), String> {
        let Some(embedder) = &self.embedder else {
            return Err("no embedder is configured".to_owned());
        };
        let input = [query.to_owned()];
        let embedding = embedder
            .embed(&input, scope)
            .await
            .map_err(|error| format!("the embedder failed: {error}"))?;
        let cost = embedding.cost_usd_micros;
        let vector = embedding
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| "the embedder returned nothing".to_owned())?;
        self.store
            .search_vector(&vector, classes, pool)
            .await
            .map(|page| (page, cost))
            .map_err(|error| format!("vector search failed: {error}"))
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
        if limit == 0 || limit > MAX_SEARCH_LIMIT {
            return Err(MemoryError::Invalid(format!(
                "limit must be 1 to {MAX_SEARCH_LIMIT}"
            )));
        }
        if query.trim().is_empty() {
            return Err(MemoryError::Invalid("a search needs a query".to_owned()));
        }
        // A pool wider than the answer: fusing only the top few loses notes
        // that rank well in one list and appear in the other.
        let pool = PageRequest::new((limit * 3).max(10), 0)?;
        let text = self.store.search_text(query, classes, pool).await?;
        let (vector, mode, cost_usd_micros) =
            match self.vector_hits(query, classes, scope, pool).await {
                Ok((page, cost)) => (page, SearchMode::Hybrid, cost),
                Err(reason) => (
                    Page {
                        items: Vec::new(),
                        has_more: false,
                    },
                    SearchMode::KeywordOnly {
                        reason: bounded(&reason),
                    },
                    0,
                ),
            };
        let fused = fuse(&[text.items.as_slice(), vector.items.as_slice()]);

        // Walk in fused order: rebuilding from any other order loses the rank.
        let mut seen = HashSet::new();
        let mut best = Vec::new();
        for chunk in fused {
            if seen.insert(chunk.hit.chunk.note_id) {
                best.push(chunk);
            }
        }
        let wanted = limit as usize;
        let has_more = best.len() > wanted || text.has_more || vector.has_more;
        best.truncate(wanted);

        let mut hits = Vec::with_capacity(best.len());
        for chunk in best {
            // A note deleted since the search is simply gone.
            if let Some(note) = self.store.get_note(chunk.hit.chunk.note_id).await? {
                hits.push(NoteHit {
                    note_id: note.id,
                    title: note.title,
                    text: chunk.hit.chunk.text,
                    tags: note.tags,
                    source_url: note.source_url,
                    provenance: chunk.hit.provenance,
                    score: chunk.score,
                });
            }
        }
        Ok(SearchResults {
            hits,
            mode,
            has_more,
            cost_usd_micros,
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
