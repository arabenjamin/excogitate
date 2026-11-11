//! The retrieval eval: made-up notes and questions, on the real `DuckDB` store,
//! with an embedder that stands in for meaning. A fall in these numbers means
//! the pipeline got worse. Tuning against a real embedder is a separate job.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::DateTime;
use memory::{
    ChunkConfig, EmbedError, EmbedScope, Embedder, Embedding, Memory, NewNote, NoteOrigin,
    SearchMode,
};
use protocol::{Actor, DataClass, Trust};
use serde::Deserialize;
use store::{ClassFilter, NoteId, PageRequest, Store};
use store_duckdb::{DuckDbConfig, DuckDbStore};

const DIMENSION: usize = 256;

#[derive(Deserialize)]
struct Golden {
    synonyms: Vec<Vec<String>>,
    unknown_to_embedders: Vec<String>,
    notes: Vec<GoldenNote>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct GoldenNote {
    id: String,
    title: String,
    body: String,
}

#[derive(Deserialize)]
struct Case {
    query: String,
    expect: String,
    kind: String,
}

fn golden() -> Golden {
    serde_saphyr::from_str(include_str!("../../../eval/retrieval_golden.yaml")).unwrap()
}

/// Hashes each word's meaning group into a bag-of-words vector. Like a real
/// embedding model, it knows meanings and has no grip on codes and rare names.
#[derive(Debug)]
struct MeaningEmbedder {
    groups: HashMap<String, usize>,
    unknown: HashSet<String>,
}

impl MeaningEmbedder {
    fn new(golden: &Golden) -> Self {
        let mut groups = HashMap::new();
        for (group, members) in golden.synonyms.iter().enumerate() {
            for word in members {
                groups.insert(word.clone(), group);
            }
        }
        Self {
            groups,
            unknown: golden.unknown_to_embedders.iter().cloned().collect(),
        }
    }

    fn knows(&self, word: &str) -> bool {
        !word.is_empty()
            && !word.contains(|c: char| c.is_ascii_digit())
            && !self.unknown.contains(word)
    }

    fn slot(&self, word: &str) -> usize {
        if let Some(group) = self.groups.get(word) {
            return group % DIMENSION;
        }
        // FNV-1a, so the vector is the same on every run and platform.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in word.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        usize::try_from(hash % DIMENSION as u64).unwrap()
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0; DIMENSION];
        let lowered = text.to_lowercase();
        for word in lowered
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| self.knows(w))
        {
            if let Some(slot) = vector.get_mut(self.slot(word)) {
                *slot += 1.0;
            }
        }
        vector
    }
}

#[async_trait]
impl Embedder for MeaningEmbedder {
    fn dimension(&self) -> usize {
        DIMENSION
    }

    async fn embed(&self, inputs: &[String], _: EmbedScope) -> Result<Embedding, EmbedError> {
        Ok(Embedding {
            vectors: inputs.iter().map(|text| self.vector(text)).collect(),
            cost_usd_micros: 0,
        })
    }
}

struct Loaded {
    store: Arc<DuckDbStore>,
    memory: Memory,
    keyword_memory: Memory,
    embedder: Arc<MeaningEmbedder>,
    ids: HashMap<String, NoteId>,
}

async fn load(golden: &Golden) -> Loaded {
    let store = Arc::new(
        DuckDbStore::open(&DuckDbConfig {
            path: None,
            extension_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.cache/duckdb-extensions"),
            embedding_dimension: DIMENSION,
        })
        .unwrap(),
    );
    let embedder = Arc::new(MeaningEmbedder::new(golden));
    let memory = Memory::new(
        Arc::clone(&store) as Arc<dyn Store>,
        Some(Arc::clone(&embedder) as Arc<dyn Embedder>),
        ChunkConfig::default(),
    );
    let keyword_memory = Memory::new(
        Arc::clone(&store) as Arc<dyn Store>,
        None,
        ChunkConfig::default(),
    );
    let mut ids = HashMap::new();
    for note in &golden.notes {
        let stored = memory
            .store(
                NewNote {
                    title: note.title.clone(),
                    body: note.body.clone(),
                    tags: vec![],
                    source_url: None,
                },
                NoteOrigin {
                    data_class: DataClass::Private,
                    trust: Trust::Trusted,
                    stored_by: Actor::new("eval"),
                },
                None,
                DateTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        assert_eq!(stored.unembedded_chunks, 0);
        ids.insert(note.id.clone(), stored.id);
    }
    Loaded {
        store,
        memory,
        keyword_memory,
        embedder,
        ids,
    }
}

const K: u32 = 5;

const SCOPE: EmbedScope = EmbedScope {
    data_class: DataClass::Private,
    budget_usd_micros: None,
};

/// Recall at K, and mean reciprocal rank, from each case's 1-based rank.
#[expect(clippy::cast_precision_loss, reason = "small counts")]
fn score(ranks: &[Option<usize>]) -> (f64, f64) {
    let total = ranks.len() as f64;
    let found = ranks
        .iter()
        .filter(|rank| rank.is_some_and(|r| r <= K as usize))
        .count();
    let reciprocal: f64 = ranks.iter().flatten().map(|rank| 1.0 / *rank as f64).sum();
    (found as f64 / total, reciprocal / total)
}

fn rank_of(ids: &[NoteId], wanted: NoteId) -> Option<usize> {
    ids.iter().position(|id| *id == wanted).map(|at| at + 1)
}

async fn hybrid_ids(loaded: &Loaded, query: &str) -> Vec<NoteId> {
    let results = loaded
        .memory
        .search(query, ClassFilter::Any, SCOPE, K)
        .await
        .unwrap();
    assert_eq!(results.mode, SearchMode::Hybrid);
    results.hits.iter().map(|hit| hit.note_id).collect()
}

async fn keyword_ids(loaded: &Loaded, query: &str) -> Vec<NoteId> {
    let results = loaded
        .keyword_memory
        .search(query, ClassFilter::Any, SCOPE, K)
        .await
        .unwrap();
    results.hits.iter().map(|hit| hit.note_id).collect()
}

async fn vector_ids(loaded: &Loaded, query: &str) -> Vec<NoteId> {
    use store::NoteReader;
    let vector = loaded.embedder.vector(query);
    let page = PageRequest::new(K, 0).unwrap();
    let hits = loaded
        .store
        .search_vector(&vector, ClassFilter::Any, page)
        .await
        .unwrap();
    hits.items.iter().map(|hit| hit.chunk.note_id).collect()
}

fn report(name: &str, ranks: &[Option<usize>]) -> (f64, f64) {
    let (recall, mrr) = score(ranks);
    println!("{name:10} recall@{K} {recall:.3}  MRR {mrr:.3}");
    (recall, mrr)
}

#[tokio::test(flavor = "multi_thread")]
async fn hybrid_retrieval_beats_either_path_alone() {
    let golden = golden();
    let loaded = load(&golden).await;
    let (mut hybrid, mut keyword, mut vector) = (Vec::new(), Vec::new(), Vec::new());
    let mut by_kind: HashMap<&str, Vec<Option<usize>>> = HashMap::new();
    for case in &golden.cases {
        let wanted = *loaded.ids.get(&case.expect).unwrap();
        let rank = rank_of(&hybrid_ids(&loaded, &case.query).await, wanted);
        println!("{:8} {:30} rank {rank:?}", case.kind, case.query);
        hybrid.push(rank);
        by_kind.entry(&case.kind).or_default().push(rank);
        keyword.push(rank_of(&keyword_ids(&loaded, &case.query).await, wanted));
        vector.push(rank_of(&vector_ids(&loaded, &case.query).await, wanted));
    }
    let (hybrid_recall, hybrid_mrr) = report("hybrid", &hybrid);
    let (keyword_recall, _) = report("keyword", &keyword);
    let (vector_recall, _) = report("vector", &vector);

    // Floors from the first measured run. Raise them when retrieval improves.
    assert!(
        hybrid_recall >= FLOOR_RECALL,
        "recall fell to {hybrid_recall}"
    );
    assert!(hybrid_mrr >= FLOOR_MRR, "MRR fell to {hybrid_mrr}");
    assert!(
        hybrid_recall > keyword_recall,
        "fusion adds nothing over keywords"
    );
    assert!(
        hybrid_recall > vector_recall,
        "fusion adds nothing over vectors"
    );
    // Fusion mustn't bury what only one path can find.
    for (kind, ranks) in &by_kind {
        let (recall, _) = report(kind, ranks);
        assert!(recall >= 0.85, "{kind} recall fell to {recall}");
    }
}

const FLOOR_RECALL: f64 = 0.95;
const FLOOR_MRR: f64 = 0.80;
