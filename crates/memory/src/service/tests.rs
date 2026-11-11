use std::sync::Mutex;

use async_trait::async_trait;
use store::NoteReader;
use store_memory::MemoryStore;

use super::*;
use crate::EmbedError;

const DIM: usize = 3;

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

    async fn embed(
        &self,
        inputs: &[String],
        data_class: DataClass,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls
            .lock()
            .unwrap()
            .push((inputs.to_vec(), data_class));
        if let Some(error) = self.fail.lock().unwrap().clone() {
            return Err(error);
        }
        if let Some(max) = self.max_chars
            && inputs.iter().any(|input| input.chars().count() > max)
        {
            return Err(EmbedError {
                message: "too long".to_owned(),
                try_shorter: true,
            });
        }
        Ok(inputs.iter().map(|text| vector_for(text)).collect())
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
        .store(note(&long_text()), private(), now())
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
        .store(note("cat facts"), private(), now())
        .await
        .unwrap();
    let second = memory
        .store(note("  cat facts\n"), private(), now())
        .await
        .unwrap();
    assert_eq!(first.id, second.id);
    assert!(!second.created);
    assert!(
        store
            .find_note_by_hash(&content_hash("cat facts"))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn an_empty_note_is_refused() {
    let (memory, _) = memory_with(None);
    let result = memory.store(note("  \n"), private(), now()).await;
    assert!(matches!(result, Err(MemoryError::Invalid(_))));
}

#[tokio::test]
async fn a_dead_embedder_costs_vectors_never_the_note() {
    let fake = Arc::new(Fake::default());
    *fake.fail.lock().unwrap() = Some(EmbedError {
        message: "connection refused".to_owned(),
        try_shorter: false,
    });
    let (memory, _) = memory_with(Some(Arc::clone(&fake)));
    let stored = memory
        .store(note(&long_text()), private(), now())
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
    let stored = memory.store(note(&body), private(), now()).await.unwrap();
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
        .store(note(&"cat ".repeat(12)), private(), now())
        .await
        .unwrap();
    assert_eq!(stored.unembedded_chunks, 1);
    assert_eq!(fake.calls.lock().unwrap().len(), 1 + 3);
}

#[tokio::test]
async fn search_is_hybrid_and_finds_by_either_path() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    memory
        .store(note("cat whiskers and naps"), private(), now())
        .await
        .unwrap();
    let dog = memory
        .store(note("dog parks"), private(), now())
        .await
        .unwrap();
    let found = memory
        .search("parks", ClassFilter::Any, DataClass::Private, 5)
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
        .search("cat", ClassFilter::Any, DataClass::Private, 5)
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
        .store(note("dog wanted wanted wanted"), private(), now())
        .await
        .unwrap();
    // Top of the vector list, and no keyword match.
    memory
        .store(note("catnip one"), private(), now())
        .await
        .unwrap();
    // Second in both, so it wins, though it's in neither top one.
    let both = memory
        .store(note("cat wanted"), private(), now())
        .await
        .unwrap();
    memory
        .store(note("catnip two"), private(), now())
        .await
        .unwrap();
    memory
        .store(note("catnip three"), private(), now())
        .await
        .unwrap();
    let found = memory
        .search("cat wanted", ClassFilter::Any, DataClass::Private, 1)
        .await
        .unwrap();
    assert_eq!(found.hits[0].note_id, both.id);
}

#[tokio::test]
async fn a_note_appears_once_however_many_chunks_match() {
    let (memory, _) = memory_with(Some(Arc::new(Fake::default())));
    let body: String = (0..100).map(|n| format!("shared item{n}. ")).collect();
    let stored = memory.store(note(&body), private(), now()).await.unwrap();
    assert!(stored.chunks > 1);
    let found = memory
        .search("shared", ClassFilter::Any, DataClass::Private, 5)
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
}

#[tokio::test]
async fn without_an_embedder_search_says_keyword_only() {
    let (memory, _) = memory_with(None);
    memory
        .store(note("cat naps"), private(), now())
        .await
        .unwrap();
    let found = memory
        .search("naps", ClassFilter::Any, DataClass::Private, 5)
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
        .store(note("cat naps"), private(), now())
        .await
        .unwrap();
    *fake.fail.lock().unwrap() = Some(EmbedError {
        message: "it broke".to_owned(),
        try_shorter: false,
    });
    let found = memory
        .search("naps", ClassFilter::Any, DataClass::Private, 5)
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
            .store(note(&format!("shared {n}")), private(), now())
            .await
            .unwrap();
    }
    let found = memory
        .search("shared", ClassFilter::Any, DataClass::Private, 2)
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
        .store(note("cat secret"), private(), now())
        .await
        .unwrap();
    let open = memory
        .store(
            note("cat open"),
            origin(DataClass::Public, Trust::Trusted),
            now(),
        )
        .await
        .unwrap();
    let found = memory
        .search("cat", ClassFilter::PublicOnly, DataClass::Public, 5)
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
            now(),
        )
        .await
        .unwrap();
    let found = memory
        .search("fetched", ClassFilter::Any, DataClass::Private, 5)
        .await
        .unwrap();
    assert_eq!(found.hits[0].provenance.trust, Trust::Untrusted);
}

#[tokio::test]
async fn an_edit_is_found_and_the_old_text_is_not() {
    // Keywords only: a vector search always returns its nearest neighbors.
    let (memory, store) = memory_with(None);
    let stored = memory
        .store(note("oldword here"), private(), now())
        .await
        .unwrap();
    let edit = NoteEdit {
        body: Some("newword here".to_owned()),
        ..NoteEdit::default()
    };
    memory
        .update(stored.id, edit, ClassFilter::Any, Trust::Trusted, now())
        .await
        .unwrap();
    let search = |q: &'static str| {
        let memory = memory.clone();
        async move {
            memory
                .search(q, ClassFilter::Any, DataClass::Private, 5)
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
        .store(note("trusted text"), private(), now())
        .await
        .unwrap();
    let edit = NoteEdit {
        body: Some("written by a model".to_owned()),
        ..NoteEdit::default()
    };
    memory
        .update(stored.id, edit, ClassFilter::Any, Trust::Untrusted, now())
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
    let stored = memory.store(note("cat"), private(), now()).await.unwrap();
    let calls = fake.calls.lock().unwrap().len();
    let edit = NoteEdit {
        title: Some("new title".to_owned()),
        ..NoteEdit::default()
    };
    memory
        .update(stored.id, edit, ClassFilter::Any, Trust::Trusted, now())
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
        .store(note("gone soon"), private(), now())
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
            .search(query, ClassFilter::Any, DataClass::Private, limit)
            .await;
        assert!(matches!(result, Err(MemoryError::Invalid(_))));
    }
}
