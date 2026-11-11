//! Fusing ranked lists. Scores from different searches can't be compared, so
//! this uses only each chunk's rank.

use std::collections::HashMap;

use store::{ChunkHit, ChunkId};

/// The constant in reciprocal rank fusion. The usual value.
pub const RRF_K: f32 = 60.0;

/// A chunk and its fused score.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedChunk {
    /// The chunk.
    pub hit: ChunkHit,
    /// Its fused score, higher is better.
    pub score: f32,
}

/// Reciprocal rank fusion, best first, with ties broken by chunk id.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "ranks are far below f32's exact range"
)]
pub fn fuse(lists: &[&[ChunkHit]]) -> Vec<FusedChunk> {
    let mut merged: HashMap<ChunkId, FusedChunk> = HashMap::new();
    for list in lists {
        for (rank, hit) in list.iter().enumerate() {
            let score = 1.0 / (RRF_K + rank as f32 + 1.0);
            merged
                .entry(hit.chunk.id)
                .and_modify(|fused| fused.score += score)
                .or_insert_with(|| FusedChunk {
                    hit: hit.clone(),
                    score,
                });
        }
    }
    let mut ranked: Vec<FusedChunk> = merged.into_values().collect();
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.hit.chunk.id.cmp(&b.hit.chunk.id))
    });
    ranked
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use protocol::{Actor, DataClass, Trust};
    use store::{Chunk, NoteId, Provenance};

    fn hit(id: ChunkId) -> ChunkHit {
        ChunkHit {
            chunk: Chunk {
                id,
                note_id: NoteId::generate(),
                ordinal: 0,
                text: String::new(),
                token_count: 1,
                embedding: None,
            },
            provenance: Provenance {
                data_class: DataClass::Private,
                trust: Trust::Trusted,
                stored_by: Actor::new("t"),
                stored_at: DateTime::UNIX_EPOCH,
            },
            score: 0.0,
        }
    }

    #[test]
    fn a_chunk_in_both_lists_beats_one_at_the_top_of_a_single_list() {
        let (a, b, both) = (
            ChunkId::generate(),
            ChunkId::generate(),
            ChunkId::generate(),
        );
        let text = [hit(a), hit(both)];
        let vector = [hit(b), hit(both)];
        let fused = fuse(&[&text, &vector]);
        assert_eq!(fused[0].hit.chunk.id, both);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn the_scores_of_the_lists_are_ignored() {
        let (a, b) = (ChunkId::generate(), ChunkId::generate());
        let mut low = hit(a);
        low.score = 0.001;
        let mut high = hit(b);
        high.score = 1000.0;
        let fused = fuse(&[&[low, high]]);
        assert_eq!(fused[0].hit.chunk.id, a);
    }

    #[test]
    fn nothing_in_gives_nothing_out() {
        assert_eq!(fuse(&[&[], &[]]).len(), 0);
    }
}
