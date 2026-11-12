use std::sync::Mutex;

use async_trait::async_trait;
use store::NoteReader;
use store_memory::MemoryStore;

use super::*;
use crate::{EmbedError, Embedding};

const DIM: usize = 3;

const SCOPE_PRIVATE: EmbedScope = EmbedScope {
    data_class: DataClass::Private,
    budget_usd_micros: None,
};

const SCOPE_PUBLIC: EmbedScope = EmbedScope {
    data_class: DataClass::Public,
    budget_usd_micros: None,
};

/// Embeds by the first word. Records what it was asked.
#[derive(Debug, Default)]
struct Fake {
    calls: Mutex<Vec<(Vec<String>, DataClass)>>,
    fail: Mutex<Option<EmbedError>>,
    max_chars: Option<usize>,
}

fn vector_for(text: &str) -> Vec<f32> {
    if text.trim_start().starts_with("cat") {
        vec![1.0, 0.0, 0.0]
    } else if text.trim_start().starts_with("dog") {
        vec![0.0, 1.0, 0.0]
    } else {
        vec![0.0, 0.0, 1.0]
    }
}

#[async_trait]
impl Embedder for Fake {
    fn dimension(&self) -> usize {
        DIM
    }

    async fn embed(&self, inputs: &[String], scope: EmbedScope) -> Result<Embedding, EmbedError> {
        self.calls
            .lock()
            .unwrap()
            .push((inputs.to_vec(), scope.data_class));
        if let Some(error) = self.fail.lock().unwrap().clone() {
            return Err(error);
        }
        if let Some(max) = self.max_chars
            && inputs.iter().any(|input| input.chars().count() > max)
        {
            return Err(EmbedError {
                message: "too long".to_owned(),
                try_shorter: true,
                cost_usd_micros: 0,
            });
        }
        Ok(Embedding {
            vectors: inputs.iter().map(|text| vector_for(text)).collect(),
            cost_usd_micros: 7,
            model: None,
        })
    }
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

fn memory_with(embedder: Option<Arc<Fake>>) -> (Memory, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new(DIM));
    let memory = Memory::new(
        Arc::clone(&store) as Arc<dyn Store>,
        embedder.map(|fake| fake as Arc<dyn Embedder>),
        ChunkConfig {
            max_tokens: 30,
            overlap_tokens: 0,
        },
    );
    (memory, store)
}

fn note(body: &str) -> NewNote {
    NewNote {
        title: "A title".to_owned(),
        body: body.to_owned(),
        tags: vec!["t".to_owned()],
        source_url: None,
    }
}

fn origin(data_class: DataClass, trust: Trust) -> NoteOrigin {
    NoteOrigin {
        data_class,
        trust,
        stored_by: Actor::new("tester"),
    }
}

fn private() -> NoteOrigin {
    origin(DataClass::Private, Trust::Trusted)
}

fn long_text() -> String {
    (0..200).map(|n| format!("word{n}. ")).collect()
}

fn assert_keyword_only(mode: &SearchMode, wanted: &str) {
    match mode {
        SearchMode::KeywordOnly { reason } => assert!(reason.contains(wanted), "{reason}"),
        SearchMode::Hybrid => panic!("expected keyword-only"),
    }
}

#[tokio::test]
async fn a_note_is_stored_chunked_and_embedded_as_private() {
    let fake = Arc::new(Fake::default());
    let (memory, store) = memory_with(Some(Arc::clone(&fake)));
    let stored = memory
        .store(note(&long_text()), private(), None, now())
        .await
        .unwrap();
    assert!(stored.created);
    assert!(stored.chunks > 1);
    assert_eq!(stored.unembedded_chunks, 0);
    let chunks = store.list_chunks(stored.id).await.unwrap();
    assert_eq!(chunks.len() as u32, stored.chunks);
    assert!(chunks.iter().all(|chunk| chunk.embedding.is_some()));
    assert_eq!(fake.calls.lock().unwrap()[0].1, DataClass::Private);
}

#[tokio::test]
async fn the_same_content_returns_the_existing_note() {
    let (memory, store) = memory_with(Some(Arc::new(Fake::default())));
    let first = memory
        .store(note("cat facts"), private(), None, now())
        .await
        .unwrap();
    let second = memory
        .store(note("  cat facts\n"), private(), None, now())
        .await
        .unwrap();
    assert_eq!(first.id, second.id);
    assert!(!second.created);
    assert!(
        store
            .find_note_by_hash(&content_hash("cat facts"), DataClass::Private)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn the_same_content_in_the_other_class_is_a_note_of_its_own() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    let secret = memory
        .store(note("the launch date"), private(), None, now())
        .await
        .unwrap();
    // A public run storing the same words learns nothing of the private
    // note, and its own note is kept where public runs can find it.
    let public = memory
        .store(
            note("the launch date"),
            origin(DataClass::Public, Trust::Trusted),
            None,
            now(),
        )
        .await
        .unwrap();
    assert!(public.created);
    assert_ne!(public.id, secret.id);
    let found = memory
        .search("launch date", ClassFilter::PublicOnly, SCOPE_PUBLIC, 5)
        .await
        .unwrap();
    assert_eq!(
        found.hits.iter().map(|hit| hit.note_id).collect::<Vec<_>>(),
        [public.id]
    );
}

#[tokio::test]
async fn embedding_costs_are_reported() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    let stored = memory
        .store(note("cat"), private(), None, now())
        .await
        .unwrap();
    assert_eq!(stored.cost_usd_micros, 7);
    let again = memory
        .store(note("cat"), private(), None, now())
        .await
        .unwrap();
    assert_eq!(again.cost_usd_micros, 0, "nothing new was embedded");
    let found = memory
        .search("cat", ClassFilter::Any, SCOPE_PRIVATE, 5)
        .await
        .unwrap();
    assert_eq!(found.cost_usd_micros, 7);
}

/// Refuses long inputs, costs 5 a call, and remembers the budget it was given.
#[derive(Debug, Default)]
struct Metered {
    budgets: Mutex<Vec<Option<u64>>>,
}

#[async_trait]
impl Embedder for Metered {
    fn dimension(&self) -> usize {
        DIM
    }

    async fn embed(&self, inputs: &[String], scope: EmbedScope) -> Result<Embedding, EmbedError> {
        self.budgets.lock().unwrap().push(scope.budget_usd_micros);
        if inputs.len() > 1 {
            return Err(EmbedError {
                message: "one at a time".to_owned(),
                try_shorter: true,
                cost_usd_micros: 0,
            });
        }
        Ok(Embedding {
            vectors: vec![vec![1.0, 0.0, 0.0]],
            cost_usd_micros: 5,
            model: None,
        })
    }
}

#[tokio::test]
async fn retries_spend_only_what_is_left() {
    let metered = Arc::new(Metered::default());
    let memory = Memory::new(
        Arc::new(MemoryStore::new(DIM)) as Arc<dyn Store>,
        Some(metered.clone() as Arc<dyn Embedder>),
        ChunkConfig {
            max_tokens: 30,
            overlap_tokens: 0,
        },
    );
    let body = "word ".repeat(1200);
    let stored = memory
        .store(note(&body), private(), Some(100), now())
        .await
        .unwrap();
    assert!(stored.chunks > 1, "the test needs several chunks");
    assert_eq!(stored.cost_usd_micros, 5 * u64::from(stored.chunks));
    let budgets = metered.budgets.lock().unwrap();
    assert_eq!(budgets[0], Some(100));
    assert_eq!(budgets[1], Some(100));
    assert_eq!(budgets[2], Some(95));
}

#[tokio::test]
async fn an_empty_note_is_refused() {
    let (memory, _) = memory_with(None);
    let result = memory.store(note("  \n"), private(), None, now()).await;
    assert!(matches!(result, Err(MemoryError::Invalid(_))));
}

#[tokio::test]
async fn a_dead_embedder_costs_vectors_never_the_note() {
    let fake = Arc::new(Fake::default());
    *fake.fail.lock().unwrap() = Some(EmbedError {
        message: "connection refused".to_owned(),
        try_shorter: false,
        cost_usd_micros: 0,
    });
    let (memory, _) = memory_with(Some(Arc::clone(&fake)));
    let stored = memory
        .store(note(&long_text()), private(), None, now())
        .await
        .unwrap();
    assert_eq!(stored.unembedded_chunks, stored.chunks);
    // A dead server gets one call, not one per chunk.
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_over_long_chunk_is_embedded_from_its_front_half() {
    let fake = Arc::new(Fake {
        max_chars: Some(20),
        ..Fake::default()
    });
    let (memory, store) = memory_with(Some(Arc::clone(&fake)));
    let body = "cat ".repeat(12);
    let stored = memory
        .store(note(&body), private(), None, now())
        .await
        .unwrap();
    assert_eq!(stored.unembedded_chunks, 0);
    let chunks = store.list_chunks(stored.id).await.unwrap();
    assert_eq!(chunks[0].text, body, "the chunk keeps its full text");
}

#[tokio::test]
async fn an_input_that_never_fits_gives_up_after_the_halvings() {
    let fake = Arc::new(Fake {
        max_chars: Some(1),
        ..Fake::default()
    });
    let (memory, _) = memory_with(Some(Arc::clone(&fake)));
    let stored = memory
        .store(note(&"cat ".repeat(12)), private(), None, now())
        .await
        .unwrap();
    assert_eq!(stored.unembedded_chunks, 1);
    assert_eq!(fake.calls.lock().unwrap().len(), 1 + 3);
}

#[tokio::test]
async fn search_is_hybrid_and_finds_by_either_path() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    memory
        .store(note("cat whiskers and naps"), private(), None, now())
        .await
        .unwrap();
    let dog = memory
        .store(note("dog parks"), private(), None, now())
        .await
        .unwrap();
    let found = memory
        .search("parks", ClassFilter::Any, SCOPE_PRIVATE, 5)
        .await
        .unwrap();
    assert_eq!(found.mode, SearchMode::Hybrid);
    assert_eq!(found.hits[0].note_id, dog.id);
}

#[tokio::test]
async fn a_private_query_is_embedded_as_private() {
    let fake = Arc::new(Fake::default());
    let (memory, _) = memory_with(Some(Arc::clone(&fake)));
    memory
        .search("cat", ClassFilter::Any, SCOPE_PRIVATE, 5)
        .await
        .unwrap();
    assert_eq!(
        fake.calls.lock().unwrap().last().unwrap().1,
        DataClass::Private
    );
}

#[tokio::test]
async fn fusion_sees_past_the_number_of_results_asked_for() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    // Top of the keyword list, last of the vector list.
    memory
        .store(note("dog wanted wanted wanted"), private(), None, now())
        .await
        .unwrap();
    // Top of the vector list, and no keyword match.
    memory
        .store(note("catnip one"), private(), None, now())
        .await
        .unwrap();
    // Second in both, so it wins, though it's in neither top one.
    let both = memory
        .store(note("cat wanted"), private(), None, now())
        .await
        .unwrap();
    memory
        .store(note("catnip two"), private(), None, now())
        .await
        .unwrap();
    memory
        .store(note("catnip three"), private(), None, now())
        .await
        .unwrap();
    let found = memory
        .search("cat wanted", ClassFilter::Any, SCOPE_PRIVATE, 1)
        .await
        .unwrap();
    assert_eq!(found.hits[0].note_id, both.id);
}

#[tokio::test]
async fn a_note_appears_once_however_many_chunks_match() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    let body: String = (0..100).map(|n| format!("shared item{n}. ")).collect();
    let stored = memory
        .store(note(&body), private(), None, now())
        .await
        .unwrap();
    assert!(stored.chunks > 1);
    let found = memory
        .search("shared", ClassFilter::Any, SCOPE_PRIVATE, 5)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
}

#[tokio::test]
async fn without_an_embedder_search_says_keyword_only() {
    let (memory, _) = memory_with(None);
    memory
        .store(note("cat naps"), private(), None, now())
        .await
        .unwrap();
    let found = memory
        .search("naps", ClassFilter::Any, SCOPE_PRIVATE, 5)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
    assert_keyword_only(&found.mode, "no embedder");
}

#[tokio::test]
async fn a_failing_embedder_shows_in_the_result() {
    let fake = Arc::new(Fake::default());
    let (memory, _) = memory_with(Some(Arc::clone(&fake)));
    memory
        .store(note("cat naps"), private(), None, now())
        .await
        .unwrap();
    *fake.fail.lock().unwrap() = Some(EmbedError {
        message: "it broke".to_owned(),
        try_shorter: false,
        cost_usd_micros: 0,
    });
    let found = memory
        .search("naps", ClassFilter::Any, SCOPE_PRIVATE, 5)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
    assert_keyword_only(&found.mode, "it broke");
}

#[tokio::test]
async fn a_cut_result_says_so() {
    let (memory, _) = memory_with(None);
    for n in 0..4 {
        memory
            .store(note(&format!("shared {n}")), private(), None, now())
            .await
            .unwrap();
    }
    let found = memory
        .search("shared", ClassFilter::Any, SCOPE_PRIVATE, 2)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 2);
    assert!(found.has_more);
}

#[tokio::test]
async fn a_public_search_cannot_reach_private_notes() {
    let fake = Arc::new(Fake::default());
    let (memory, _) = memory_with(Some(Arc::clone(&fake)));
    let secret = memory
        .store(note("cat secret"), private(), None, now())
        .await
        .unwrap();
    let open = memory
        .store(
            note("cat open"),
            origin(DataClass::Public, Trust::Trusted),
            None,
            now(),
        )
        .await
        .unwrap();
    let found = memory
        .search("cat", ClassFilter::PublicOnly, SCOPE_PUBLIC, 5)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].note_id, open.id);
    assert_eq!(
        fake.calls.lock().unwrap().last().unwrap().1,
        DataClass::Public
    );
    assert_eq!(
        memory
            .get(secret.id, ClassFilter::PublicOnly)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        memory.delete(secret.id, ClassFilter::PublicOnly).await,
        Err(MemoryError::NotFound(secret.id))
    );
    let edit = NoteEdit {
        title: Some("mine now".to_owned()),
        ..NoteEdit::default()
    };
    let result = memory
        .update(
            secret.id,
            edit,
            ClassFilter::PublicOnly,
            Trust::Trusted,
            None,
            now(),
        )
        .await;
    assert_eq!(result, Err(MemoryError::NotFound(secret.id)));
}

#[tokio::test]
async fn an_untrusted_note_stays_labelled_in_search() {
    let (memory, _) = memory_with(None);
    memory
        .store(
            note("fetched words"),
            origin(DataClass::Private, Trust::Untrusted),
            None,
            now(),
        )
        .await
        .unwrap();
    let found = memory
        .search("fetched", ClassFilter::Any, SCOPE_PRIVATE, 5)
        .await
        .unwrap();
    assert_eq!(found.hits[0].provenance.trust, Trust::Untrusted);
}

#[tokio::test]
async fn an_edit_is_found_and_the_old_text_is_not() {
    // Keywords only: a vector search always returns its nearest neighbors.
    let (memory, store) = memory_with(None);
    let stored = memory
        .store(note("oldword here"), private(), None, now())
        .await
        .unwrap();
    let edit = NoteEdit {
        body: Some("newword here".to_owned()),
        ..NoteEdit::default()
    };
    memory
        .update(
            stored.id,
            edit,
            ClassFilter::Any,
            Trust::Trusted,
            None,
            now(),
        )
        .await
        .unwrap();
    let search = |q: &'static str| {
        let memory = memory.clone();
        async move {
            memory
                .search(q, ClassFilter::Any, SCOPE_PRIVATE, 5)
                .await
                .unwrap()
                .hits
                .len()
        }
    };
    assert_eq!(search("oldword").await, 0);
    assert_eq!(search("newword").await, 1);
    let updated = store.get_note(stored.id).await.unwrap().unwrap();
    assert_eq!(updated.version, 2);
    assert_eq!(updated.content_hash, content_hash("newword here"));
}

#[tokio::test]
async fn an_untrusted_edit_taints_the_note_and_never_changes_its_class() {
    let (memory, store) = memory_with(None);
    let stored = memory
        .store(note("trusted text"), private(), None, now())
        .await
        .unwrap();
    let edit = NoteEdit {
        body: Some("written by a model".to_owned()),
        ..NoteEdit::default()
    };
    memory
        .update(
            stored.id,
            edit,
            ClassFilter::Any,
            Trust::Untrusted,
            None,
            now(),
        )
        .await
        .unwrap();
    let updated = store.get_note(stored.id).await.unwrap().unwrap();
    assert_eq!(updated.provenance.trust, Trust::Untrusted);
    assert_eq!(updated.provenance.data_class, DataClass::Private);
}

#[tokio::test]
async fn a_title_edit_does_not_re_embed() {
    let fake = Arc::new(Fake::default());
    let (memory, store) = memory_with(Some(Arc::clone(&fake)));
    let stored = memory
        .store(note("cat"), private(), None, now())
        .await
        .unwrap();
    let calls = fake.calls.lock().unwrap().len();
    let edit = NoteEdit {
        title: Some("new title".to_owned()),
        ..NoteEdit::default()
    };
    memory
        .update(
            stored.id,
            edit,
            ClassFilter::Any,
            Trust::Trusted,
            None,
            now(),
        )
        .await
        .unwrap();
    assert_eq!(fake.calls.lock().unwrap().len(), calls);
    assert_eq!(store.list_chunks(stored.id).await.unwrap().len(), 1);
    assert_eq!(
        store.get_note(stored.id).await.unwrap().unwrap().title,
        "new title"
    );
}

#[tokio::test]
async fn delete_forgets_and_a_second_delete_says_so() {
    let (memory, _) = memory_with(None);
    let stored = memory
        .store(note("gone soon"), private(), None, now())
        .await
        .unwrap();
    memory.delete(stored.id, ClassFilter::Any).await.unwrap();
    assert_eq!(memory.get(stored.id, ClassFilter::Any).await.unwrap(), None);
    assert_eq!(
        memory.delete(stored.id, ClassFilter::Any).await,
        Err(MemoryError::NotFound(stored.id))
    );
}

#[tokio::test]
async fn limits_and_empty_queries_are_refused() {
    let (memory, _) = memory_with(None);
    for (query, limit) in [("a", 0), ("a", MAX_SEARCH_LIMIT + 1), ("  ", 5)] {
        let result = memory
            .search(query, ClassFilter::Any, SCOPE_PRIVATE, limit)
            .await;
        assert!(matches!(result, Err(MemoryError::Invalid(_))));
    }
}

#[tokio::test]
async fn a_similarity_threshold_applies_before_the_limit() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    // Two notes full of the word but far from it by vector, and one close.
    for text in ["bird cat cat cat", "dog cat cat cat"] {
        memory
            .store(note(text), private(), None, now())
            .await
            .unwrap();
    }
    let close = memory
        .store(note("cat alone"), private(), None, now())
        .await
        .unwrap();
    let found = memory
        .search_close("cat", ClassFilter::Any, SCOPE_PRIVATE, 1, Some(0.6))
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1, "{:?}", found.hits);
    assert_eq!(found.hits[0].note_id, close.id);
    assert_eq!(found.not_close_enough, 2);
}

/// Edits the note it's embedding for, so the update that asked loses the
/// race and isn't saved.
#[derive(Debug)]
struct Interrupting {
    store: Arc<MemoryStore>,
    note: Mutex<Option<NoteId>>,
}

#[async_trait]
impl Embedder for Interrupting {
    fn dimension(&self) -> usize {
        DIM
    }

    async fn embed(&self, inputs: &[String], _: EmbedScope) -> Result<Embedding, EmbedError> {
        let target = *self.note.lock().unwrap();
        if let Some(id) = target {
            let mut note = self.store.get_note(id).await.unwrap().unwrap();
            let expected_version = note.version;
            note.title = "changed meanwhile".to_owned();
            self.store
                .apply(
                    WriteOp::UpdateNote {
                        note,
                        expected_version,
                    }
                    .into(),
                )
                .await
                .unwrap();
        }
        Ok(Embedding {
            vectors: inputs.iter().map(|text| vector_for(text)).collect(),
            cost_usd_micros: 7,
            model: None,
        })
    }
}

#[tokio::test]
async fn an_update_that_isnt_saved_still_reports_its_embedding() {
    let store = Arc::new(MemoryStore::new(DIM));
    let embedder = Arc::new(Interrupting {
        store: Arc::clone(&store),
        note: Mutex::new(None),
    });
    let memory = Memory::new(
        Arc::clone(&store) as Arc<dyn Store>,
        Some(Arc::clone(&embedder) as Arc<dyn Embedder>),
        ChunkConfig {
            max_tokens: 30,
            overlap_tokens: 0,
        },
    );
    let stored = memory
        .store(note("first words"), private(), None, now())
        .await
        .unwrap();
    *embedder.note.lock().unwrap() = Some(stored.id);
    let edit = NoteEdit {
        body: Some("second words".to_owned()),
        ..NoteEdit::default()
    };
    let error = memory
        .update(
            stored.id,
            edit,
            ClassFilter::Any,
            Trust::Trusted,
            None,
            now(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::NotSaved { .. }), "{error:?}");
    assert_eq!(error.cost_usd_micros(), 7);
}
