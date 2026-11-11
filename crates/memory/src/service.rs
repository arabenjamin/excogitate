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

use crate::{ChunkConfig, EmbedError, Embedder, Piece, chunk_text, fuse};

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
        data_class: DataClass,
    ) -> Option<Vec<f32>> {
        let mut input = text.to_owned();
        for _ in 0..=MAX_HALVINGS {
            match embedder
                .embed(std::slice::from_ref(&input), data_class)
                .await
            {
                Ok(vectors) => return vectors.into_iter().next(),
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

    /// One vector or `None` per piece. Failure costs a chunk its vector,
    /// never the note.
    async fn embed_pieces(&self, pieces: &[Piece], data_class: DataClass) -> Vec<Option<Vec<f32>>> {
        let Some(embedder) = &self.embedder else {
            return vec![None; pieces.len()];
        };
        let inputs: Vec<String> = pieces.iter().map(|piece| piece.text.clone()).collect();
        match embedder.embed(&inputs, data_class).await {
            Ok(vectors) if vectors.len() == pieces.len() => vectors.into_iter().map(Some).collect(),
            Ok(_)
            | Err(EmbedError {
                try_shorter: false, ..
            }) => vec![None; pieces.len()],
            Err(_) => {
                let mut one_by_one = Vec::with_capacity(pieces.len());
                for piece in pieces {
                    one_by_one.push(
                        self.embed_one(embedder.as_ref(), &piece.text, data_class)
                            .await,
                    );
                }
                one_by_one
            }
        }
    }

    async fn build_chunks(
        &self,
        note_id: NoteId,
        body: &str,
        data_class: DataClass,
    ) -> (Vec<Chunk>, u32) {
        let pieces = chunk_text(body, self.chunking);
        let vectors = self.embed_pieces(&pieces, data_class).await;
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
        (chunks, u32::try_from(unembedded).unwrap_or(u32::MAX))
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
            });
        }
        let id = NoteId::generate();
        let (chunks, unembedded_chunks) =
            self.build_chunks(id, &note.body, origin.data_class).await;
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
        let (count, unembedded_chunks) = if let Some(body) = new_body {
            note.content_hash = content_hash(&body);
            note.body.clone_from(&body);
            let (chunks, unembedded) = self
                .build_chunks(id, &body, note.provenance.data_class)
                .await;
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
            (count, unembedded)
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
            )
        };
        self.store.apply(unit).await?;
        Ok(StoredNote {
            id,
            created: false,
            chunks: count,
            unembedded_chunks,
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
        data_class: DataClass,
        pool: PageRequest,
    ) -> Result<Page<ChunkHit>, String> {
        let Some(embedder) = &self.embedder else {
            return Err("no embedder is configured".to_owned());
        };
        let input = [query.to_owned()];
        let vectors = embedder
            .embed(&input, data_class)
            .await
            .map_err(|error| format!("the embedder failed: {error}"))?;
        let vector = vectors
            .into_iter()
            .next()
            .ok_or_else(|| "the embedder returned nothing".to_owned())?;
        self.store
            .search_vector(&vector, classes, pool)
            .await
            .map_err(|error| format!("vector search failed: {error}"))
    }

    /// Searches notes. `data_class` is the run's own class, and decides who
    /// may embed the query. `classes` decides what it may find.
    ///
    /// # Errors
    ///
    /// [`MemoryError`] if the limit is out of range or the store fails. A
    /// vector failure isn't an error: the result says keyword-only and why.
    pub async fn search(
        &self,
        query: &str,
        classes: ClassFilter,
        data_class: DataClass,
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
        let (vector, mode) = match self.vector_hits(query, classes, data_class, pool).await {
            Ok(page) => (page, SearchMode::Hybrid),
            Err(reason) => (
                Page {
                    items: Vec::new(),
                    has_more: false,
                },
                SearchMode::KeywordOnly {
                    reason: bounded(&reason),
                },
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
