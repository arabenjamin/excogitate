//! Long-term memory: storing notes and finding them again.
//!
//! A note is split into chunks small enough to embed. A search runs full-text
//! and vector retrieval, fuses them by rank, and returns notes. Whenever
//! vectors aren't available it falls back to keywords and says so in the
//! result: a half-blind search must not look like a healthy one.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod chunk;
mod embed;
mod fuse;
mod service;

pub use chunk::{ChunkConfig, Piece, chunk_text, estimate_tokens};
pub use embed::{EmbedError, EmbedScope, Embedder, Embedding, GatewayEmbedder};
pub use fuse::{FusedChunk, RRF_K, fuse};
pub use service::{
    MAX_SEARCH_LIMIT, Memory, MemoryError, NewNote, NoteEdit, NoteHit, NoteOrigin, SearchMode,
    SearchResults, StoredNote,
};
