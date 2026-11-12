//! The cases. Each takes a fresh store and panics if the store is wrong.

use protocol::{Actor, DataClass, Trust};
use serde_json::json;
use store::{
    ClassFilter, Definition, DefinitionKey, DefinitionKind, Direction, FieldFilter, FieldKind,
    FieldOp, FieldValue, FilterableField, JobStatus, Record, RecordDecls, RecordQuery,
    RecordTypeDecl, Store, StoreError, Task, TaskId, TaskQuery, TaskStatus, UnitOfWork,
    WorkingEntry, WriteOp,
};

use crate::fixtures::{
    ITEM, OWNER, OWNS, at, chunk, decls, item, job, kinds, note, note_with_text, page, provenance,
    record,
};

#[allow(
    clippy::needless_pass_by_value,
    reason = "reads better at the call site"
)]
fn is_invalid<T: std::fmt::Debug>(result: Result<T, StoreError>) {
    assert!(matches!(result, Err(StoreError::Invalid(_))), "{result:?}");
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "reads better at the call site"
)]
fn is_conflict<T: std::fmt::Debug>(result: Result<T, StoreError>) {
    assert!(matches!(result, Err(StoreError::Conflict(_))), "{result:?}");
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "reads better at the call site"
)]
fn is_not_found<T: std::fmt::Debug>(result: Result<T, StoreError>) {
    assert!(matches!(result, Err(StoreError::NotFound(_))), "{result:?}");
}

async fn found_texts(store: &dyn Store, query: &str) -> Vec<String> {
    let hits = store
        .search_text(query, ClassFilter::Any, page(10))
        .await
        .unwrap();
    hits.items.into_iter().map(|h| h.chunk.text).collect()
}

// ---- notes ----

/// Every field comes back as written, with the store's version.
pub async fn note_round_trip(store: &dyn Store) {
    let mut written = note("body text");
    written.version = 99;
    store
        .apply(WriteOp::CreateNote(written.clone()).into())
        .await
        .unwrap();
    written.version = 1;
    assert_eq!(store.get_note(written.id).await.unwrap(), Some(written));
}

/// Absence is not an error.
pub async fn a_missing_note_is_none(store: &dyn Store) {
    assert_eq!(store.get_note(note("x").id).await.unwrap(), None);
}

/// An id can't be used twice.
pub async fn creating_a_duplicate_note_conflicts(store: &dyn Store) {
    let n = note("x");
    store
        .apply(WriteOp::CreateNote(n.clone()).into())
        .await
        .unwrap();
    is_conflict(store.apply(WriteOp::CreateNote(n).into()).await);
}

/// A stale writer is refused, and a good write bumps the version.
pub async fn update_checks_the_version(store: &dyn Store) {
    let mut n = note("x");
    store
        .apply(WriteOp::CreateNote(n.clone()).into())
        .await
        .unwrap();
    n.body = "changed".to_owned();
    let stale = WriteOp::UpdateNote {
        note: n.clone(),
        expected_version: 7,
    };
    is_conflict(store.apply(stale.into()).await);
    let good = WriteOp::UpdateNote {
        note: n.clone(),
        expected_version: 1,
    };
    store.apply(good.into()).await.unwrap();
    let stored = store.get_note(n.id).await.unwrap().unwrap();
    assert_eq!((stored.body.as_str(), stored.version), ("changed", 2));
}

/// The lookup behind "the same content returns the existing note", within
/// one privacy class only.
pub async fn find_note_by_hash_finds_the_copy(store: &dyn Store) {
    let n = note("x");
    store
        .apply(WriteOp::CreateNote(n.clone()).into())
        .await
        .unwrap();
    let found = store
        .find_note_by_hash(&n.content_hash, DataClass::Private)
        .await
        .unwrap();
    assert_eq!(found.map(|f| f.id), Some(n.id));
    assert_eq!(
        store
            .find_note_by_hash("other", DataClass::Private)
            .await
            .unwrap(),
        None
    );
    // The same content in the other class is neither found nor in the way.
    assert_eq!(
        store
            .find_note_by_hash(&n.content_hash, DataClass::Public)
            .await
            .unwrap(),
        None
    );
    let mut public = note("x");
    public.provenance.data_class = DataClass::Public;
    store
        .apply(WriteOp::CreateNote(public.clone()).into())
        .await
        .unwrap();
    let found = store
        .find_note_by_hash(&n.content_hash, DataClass::Public)
        .await
        .unwrap();
    assert_eq!(found.map(|f| f.id), Some(public.id));
}

/// Deleted notes leave nothing searchable behind.
pub async fn deleting_a_note_removes_its_chunks(store: &dyn Store) {
    let n = note("x");
    store
        .apply(note_with_text(&n, "findable words", None))
        .await
        .unwrap();
    store.apply(WriteOp::DeleteNote(n.id).into()).await.unwrap();
    assert_eq!(
        store.list_chunks(n.id).await.unwrap(),
        [] as [store::Chunk; 0]
    );
    assert_eq!(
        found_texts(store, "findable").await,
        [] as [std::string::String; 0]
    );
    is_not_found(store.apply(WriteOp::DeleteNote(n.id).into()).await);
}

/// Chunks come back by position, however they were written.
pub async fn chunks_list_in_order(store: &dyn Store) {
    let n = note("x");
    let chunks = vec![
        chunk(n.id, 2, "c", None),
        chunk(n.id, 0, "a", None),
        chunk(n.id, 1, "b", None),
    ];
    let unit =
        UnitOfWork::new()
            .with(WriteOp::CreateNote(n.clone()))
            .with(WriteOp::ReplaceChunks {
                note_id: n.id,
                chunks,
            });
    store.apply(unit).await.unwrap();
    let texts: Vec<String> = store
        .list_chunks(n.id)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.text)
        .collect();
    assert_eq!(texts, ["a", "b", "c"]);
}

// ---- retrieval ----

/// Default full-text settings drop digits, which lost years and versions.
pub async fn text_search_keeps_digits(store: &dyn Store) {
    store
        .apply(note_with_text(&note("a"), "released in 2026 as v3", None))
        .await
        .unwrap();
    assert_eq!(found_texts(store, "2026").await.len(), 1);
    assert_eq!(found_texts(store, "v3").await.len(), 1);
    assert_eq!(
        found_texts(store, "2027").await,
        [] as [std::string::String; 0]
    );
}

/// Case doesn't matter.
pub async fn text_search_ignores_case(store: &dyn Store) {
    store
        .apply(note_with_text(&note("a"), "Hello World", None))
        .await
        .unwrap();
    assert_eq!(found_texts(store, "hello").await.len(), 1);
    assert_eq!(found_texts(store, "WORLD").await.len(), 1);
}

/// No match is an empty page, not an error.
pub async fn text_search_with_no_match_is_empty(store: &dyn Store) {
    store
        .apply(note_with_text(&note("a"), "something", None))
        .await
        .unwrap();
    let hits = store
        .search_text("absent", ClassFilter::Any, page(5))
        .await
        .unwrap();
    assert!(hits.items.is_empty() && !hits.has_more);
}

/// An untrusted label survives the trip back through retrieval.
pub async fn hits_carry_provenance(store: &dyn Store) {
    let mut n = note("a");
    n.provenance = provenance(DataClass::Public, Trust::Untrusted);
    store
        .apply(note_with_text(&n, "labelled text", Some([1.0, 0.0, 0.0])))
        .await
        .unwrap();
    let text = store
        .search_text("labelled", ClassFilter::Any, page(5))
        .await
        .unwrap();
    assert_eq!(text.items[0].provenance, n.provenance);
    let vector = store
        .search_vector(&[1.0, 0.0, 0.0], ClassFilter::Any, page(5))
        .await
        .unwrap();
    assert_eq!(vector.items[0].provenance, n.provenance);
}

/// An incremental index can miss an update, so replacing is tested directly.
pub async fn search_follows_replaced_chunks(store: &dyn Store) {
    let n = note("a");
    store
        .apply(note_with_text(&n, "oldword here", None))
        .await
        .unwrap();
    let replace = WriteOp::ReplaceChunks {
        note_id: n.id,
        chunks: vec![chunk(n.id, 0, "newword here", None)],
    };
    store.apply(replace.into()).await.unwrap();
    assert_eq!(
        found_texts(store, "oldword").await,
        [] as [std::string::String; 0]
    );
    assert_eq!(found_texts(store, "newword").await.len(), 1);
}

/// A search limited to public data never sees private data, by either path.
pub async fn public_only_hides_private(store: &dyn Store) {
    let private = note("private one");
    let mut public = note("public one");
    public.provenance = provenance(DataClass::Public, Trust::Trusted);
    store
        .apply(note_with_text(
            &private,
            "shared word",
            Some([1.0, 0.0, 0.0]),
        ))
        .await
        .unwrap();
    store
        .apply(note_with_text(
            &public,
            "shared word",
            Some([1.0, 0.0, 0.0]),
        ))
        .await
        .unwrap();
    let check = |hits: store::Page<store::ChunkHit>| {
        assert_eq!(hits.items.len(), 1);
        assert_eq!(hits.items[0].chunk.note_id, public.id);
    };
    check(
        store
            .search_text("shared", ClassFilter::PublicOnly, page(5))
            .await
            .unwrap(),
    );
    check(
        store
            .search_vector(&[1.0, 0.0, 0.0], ClassFilter::PublicOnly, page(5))
            .await
            .unwrap(),
    );
}

/// Nearest first.
pub async fn vector_search_orders_by_similarity(store: &dyn Store) {
    let (near, far) = (note("near"), note("far"));
    store
        .apply(note_with_text(&far, "far", Some([0.0, 1.0, 0.0])))
        .await
        .unwrap();
    store
        .apply(note_with_text(&near, "near", Some([1.0, 0.1, 0.0])))
        .await
        .unwrap();
    let hits = store
        .search_vector(&[1.0, 0.0, 0.0], ClassFilter::Any, page(5))
        .await
        .unwrap();
    let order: Vec<_> = hits.items.iter().map(|h| h.chunk.note_id).collect();
    assert_eq!(order, [near.id, far.id]);
    assert!(hits.items[0].score > hits.items[1].score);
}

/// A zero vector has no direction. It must rank last, not break the query.
pub async fn a_zero_vector_is_handled(store: &dyn Store) {
    let (plain, zero) = (note("plain"), note("zero"));
    store
        .apply(note_with_text(&zero, "zero", Some([0.0, 0.0, 0.0])))
        .await
        .unwrap();
    store
        .apply(note_with_text(&plain, "plain", Some([1.0, 0.0, 0.0])))
        .await
        .unwrap();
    let hits = store
        .search_vector(&[1.0, 0.0, 0.0], ClassFilter::Any, page(5))
        .await
        .unwrap();
    let order: Vec<_> = hits.items.iter().map(|h| h.chunk.note_id).collect();
    assert_eq!(order, [plain.id, zero.id]);
    let none = store
        .search_vector(&[0.0, 0.0, 0.0], ClassFilter::Any, page(5))
        .await
        .unwrap();
    assert_eq!(none.items.len(), 2);
}

/// A query of nothing but punctuation matches nothing, and doesn't fail.
pub async fn an_empty_text_query_is_empty(store: &dyn Store) {
    store
        .apply(note_with_text(&note("a"), "words", None))
        .await
        .unwrap();
    for query in ["", "   ", "?!."] {
        let hits = store
            .search_text(query, ClassFilter::Any, page(5))
            .await
            .unwrap();
        assert_eq!(hits.items.len(), 0, "{query:?}");
    }
}

/// A search is often the first thing a prompt sees, and a vector is large.
pub async fn hits_leave_out_the_embedding(store: &dyn Store) {
    store
        .apply(note_with_text(&note("a"), "words", Some([1.0, 0.0, 0.0])))
        .await
        .unwrap();
    let hits = store
        .search_text("words", ClassFilter::Any, page(5))
        .await
        .unwrap();
    assert_eq!(hits.items[0].chunk.embedding, None);
    let hits = store
        .search_vector(&[1.0, 0.0, 0.0], ClassFilter::Any, page(5))
        .await
        .unwrap();
    assert_eq!(hits.items[0].chunk.embedding, None);
}

/// Text without an embedder still stores, and stays out of vector results.
pub async fn vector_search_skips_chunks_without_embeddings(store: &dyn Store) {
    store
        .apply(note_with_text(&note("a"), "no vector", None))
        .await
        .unwrap();
    let hits = store
        .search_vector(&[1.0, 0.0, 0.0], ClassFilter::Any, page(5))
        .await
        .unwrap();
    assert_eq!(hits.items, [] as [store::ChunkHit; 0]);
    assert_eq!(found_texts(store, "vector").await.len(), 1);
}

/// The width is a fact of the schema.
pub async fn vector_search_refuses_the_wrong_width(store: &dyn Store) {
    is_invalid(store.search_vector(&[1.0], ClassFilter::Any, page(5)).await);
}

/// A wrong width is refused on the way in, too.
pub async fn a_wrong_width_embedding_is_refused(store: &dyn Store) {
    let n = note("a");
    store
        .apply(WriteOp::CreateNote(n.clone()).into())
        .await
        .unwrap();
    let mut bad = chunk(n.id, 0, "t", None);
    bad.embedding = Some(vec![1.0]);
    is_invalid(
        store
            .apply(
                WriteOp::ReplaceChunks {
                    note_id: n.id,
                    chunks: vec![bad],
                }
                .into(),
            )
            .await,
    );
}

/// A cut result says it was cut.
pub async fn search_pages_report_more(store: &dyn Store) {
    for body in ["a", "b", "c"] {
        store
            .apply(note_with_text(&note(body), "common", None))
            .await
            .unwrap();
    }
    let hits = store
        .search_text("common", ClassFilter::Any, page(2))
        .await
        .unwrap();
    assert_eq!(hits.items.len(), 2);
    assert!(hits.has_more);
}

// ---- unit of work ----

/// One unit can touch several kinds.
pub async fn unit_of_work_spans_kinds(store: &dyn Store) {
    let n = note("a");
    let j = job(0, 0);
    let unit = note_with_text(&n, "text", None).with(WriteOp::EnqueueJob(j.clone()));
    store.apply(unit).await.unwrap();
    assert!(store.get_note(n.id).await.unwrap().is_some());
    assert_eq!(store.list_chunks(n.id).await.unwrap().len(), 1);
    assert!(store.get_job(j.id).await.unwrap().is_some());
}

/// A failure at the end undoes everything before it.
pub async fn unit_of_work_is_atomic(store: &dyn Store) {
    let existing = note("existing");
    store
        .apply(WriteOp::CreateNote(existing.clone()).into())
        .await
        .unwrap();
    let fresh = note("fresh");
    let j = job(0, 0);
    let unit = note_with_text(&fresh, "atomicword", None)
        .with(WriteOp::EnqueueJob(j.clone()))
        .with(WriteOp::DeleteNote(existing.id))
        .with(WriteOp::CreateNote(fresh.clone())); // the duplicate fails the unit
    is_conflict(store.apply(unit).await);
    assert_eq!(store.get_note(fresh.id).await.unwrap(), None);
    assert!(store.get_note(existing.id).await.unwrap().is_some());
    assert_eq!(store.get_job(j.id).await.unwrap(), None);
    assert_eq!(
        found_texts(store, "atomicword").await,
        [] as [std::string::String; 0]
    );
}

// ---- tasks and working memory ----

/// Times are compared as times; a task with no due time isn't due.
pub async fn tasks_filter_by_status_and_due_time(store: &dyn Store) {
    let task = |n: i64, status, due_at| Task {
        id: TaskId::generate(),
        title: format!("task {n}"),
        detail: String::new(),
        status,
        due_at,
        created_at: at(n),
    };
    let first = task(1, TaskStatus::Pending, Some(at(100)));
    let second = task(2, TaskStatus::Done, Some(at(200)));
    let third = task(3, TaskStatus::Pending, None);
    for t in [&third, &second, &first] {
        store
            .apply(WriteOp::PutTask(t.clone()).into())
            .await
            .unwrap();
    }
    assert_eq!(
        store.get_task(first.id).await.unwrap().as_ref(),
        Some(&first)
    );

    let pending = TaskQuery {
        status: Some(TaskStatus::Pending),
        due_by: None,
    };
    let listed = store.list_tasks(pending, page(10)).await.unwrap();
    assert_eq!(listed.items, [first.clone(), third.clone()]);

    let due = TaskQuery {
        status: None,
        due_by: Some(at(150)),
    };
    assert_eq!(
        store.list_tasks(due, page(10)).await.unwrap().items,
        [first.clone()]
    );

    store
        .apply(WriteOp::DeleteTask(first.id).into())
        .await
        .unwrap();
    assert_eq!(store.get_task(first.id).await.unwrap(), None);
    is_not_found(store.apply(WriteOp::DeleteTask(first.id).into()).await);
}

/// An expired entry reads as absent.
pub async fn working_memory_expires(store: &dyn Store) {
    let entry = WorkingEntry {
        scope: "s".to_owned(),
        key: "k".to_owned(),
        value: json!([1, 2]),
        expires_at: Some(at(10)),
    };
    store
        .apply(WriteOp::PutWorking(entry.clone()).into())
        .await
        .unwrap();
    assert_eq!(
        store.get_working("s", "k", at(9)).await.unwrap(),
        Some(entry)
    );
    assert_eq!(store.get_working("s", "k", at(10)).await.unwrap(), None);
    assert_eq!(store.get_working("other", "k", at(9)).await.unwrap(), None);
    let delete = WriteOp::DeleteWorking {
        scope: "s".to_owned(),
        key: "k".to_owned(),
    };
    store.apply(delete.into()).await.unwrap();
    assert_eq!(store.get_working("s", "k", at(9)).await.unwrap(), None);
}

// ---- jobs ----

/// `requested_by` and the payload survive.
pub async fn job_keeps_who_requested_it(store: &dyn Store) {
    let j = job(0, 0);
    store
        .apply(WriteOp::EnqueueJob(j.clone()).into())
        .await
        .unwrap();
    let stored = store.get_job(j.id).await.unwrap().unwrap();
    assert_eq!(stored.requested_by, Actor::new("requester"));
    assert_eq!(stored, j);
    is_conflict(store.apply(WriteOp::EnqueueJob(j).into()).await);
}

/// Highest priority first, then earliest.
pub async fn jobs_claim_by_priority_then_time(store: &dyn Store) {
    let (low, high_late, high_early) = (job(1, 0), job(5, 2), job(5, 1));
    for j in [&low, &high_late, &high_early] {
        store
            .apply(WriteOp::EnqueueJob(j.clone()).into())
            .await
            .unwrap();
    }
    for want in [high_early.id, high_late.id, low.id] {
        let claimed = store
            .claim_job("w", &kinds(), at(10), at(100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, want);
        assert_eq!(claimed.status, JobStatus::Claimed);
        assert_eq!(claimed.attempts, 1);
    }
    assert_eq!(
        store
            .claim_job("w", &kinds(), at(10), at(100))
            .await
            .unwrap(),
        None
    );
}

/// A worker claims only the kinds it runs, and the rest wait for theirs.
pub async fn a_job_is_claimed_only_by_a_worker_for_its_kind(store: &dyn Store) {
    let mut other = job(9, 1);
    other.kind = "elsewhere".to_owned();
    let other_id = other.id;
    let ours = job(0, 1);
    let ours_id = ours.id;
    store
        .apply(
            UnitOfWork::new()
                .with(WriteOp::EnqueueJob(other))
                .with(WriteOp::EnqueueJob(ours)),
        )
        .await
        .unwrap();
    // The other kind ranks first, and is left for a worker that runs it.
    let claimed = store
        .claim_job("w", &kinds(), at(10), at(100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, ours_id);
    assert_eq!(
        store
            .claim_job("w", &kinds(), at(10), at(100))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.claim_job("w", &[], at(10), at(100)).await.unwrap(),
        None
    );
    let theirs = store
        .claim_job("x", &["elsewhere".to_owned()], at(10), at(100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(theirs.id, other_id);
}

/// A job waits for its time.
pub async fn a_future_job_is_not_claimed(store: &dyn Store) {
    store
        .apply(WriteOp::EnqueueJob(job(0, 50)).into())
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_job("w", &kinds(), at(49), at(100))
            .await
            .unwrap(),
        None
    );
    assert!(
        store
            .claim_job("w", &kinds(), at(50), at(100))
            .await
            .unwrap()
            .is_some()
    );
}

/// Concurrent workers can't both get one job.
pub async fn a_job_is_claimed_once(store: &dyn Store) {
    store
        .apply(WriteOp::EnqueueJob(job(0, 0)).into())
        .await
        .unwrap();
    let kinds = kinds();
    let claim = |worker| store.claim_job(worker, &kinds, at(1), at(100));
    let (a, b, c, d) = tokio::join!(claim("a"), claim("b"), claim("c"), claim("d"));
    let won = [a, b, c, d]
        .into_iter()
        .filter(|r| r.as_ref().unwrap().is_some())
        .count();
    assert_eq!(won, 1);
}

/// A worker that vanished doesn't lose the job.
pub async fn a_lapsed_lease_is_claimed_again(store: &dyn Store) {
    store
        .apply(WriteOp::EnqueueJob(job(0, 0)).into())
        .await
        .unwrap();
    store
        .claim_job("first", &kinds(), at(1), at(10))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .claim_job("second", &kinds(), at(9), at(20))
            .await
            .unwrap(),
        None
    );
    let again = store
        .claim_job("second", &kinds(), at(10), at(20))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.attempts, 2);
    assert_eq!(again.lease.unwrap().worker, "second");
}

/// Only the holder can finish a job.
pub async fn completing_needs_the_lease(store: &dyn Store) {
    let j = job(0, 0);
    store
        .apply(WriteOp::EnqueueJob(j.clone()).into())
        .await
        .unwrap();
    store
        .claim_job("holder", &kinds(), at(1), at(10))
        .await
        .unwrap()
        .unwrap();
    is_conflict(store.complete_job(j.id, "other", json!(1)).await);
    store
        .complete_job(j.id, "holder", json!({ "answer": 42 }))
        .await
        .unwrap();
    assert_eq!(
        store.get_job(j.id).await.unwrap().unwrap().result,
        Some(json!({ "answer": 42 }))
    );
    let done = store.get_job(j.id).await.unwrap().unwrap();
    assert_eq!((done.status, done.lease), (JobStatus::Succeeded, None));
    is_conflict(store.complete_job(j.id, "holder", json!(1)).await);
    is_not_found(store.complete_job(job(0, 0).id, "holder", json!(1)).await);
}

/// Two jobs alike in priority and time go to the lower id first.
pub async fn a_claim_tie_goes_to_the_lowest_id(store: &dyn Store) {
    let (one, two) = (job(3, 5), job(3, 5));
    let lower = one.id.min(two.id);
    store
        .apply(
            UnitOfWork::new()
                .with(WriteOp::EnqueueJob(two))
                .with(WriteOp::EnqueueJob(one)),
        )
        .await
        .unwrap();
    let first = store
        .claim_job("w", &kinds(), at(10), at(100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, lower);
}

/// Failing needs the lease, as completing does.
pub async fn failing_needs_the_lease(store: &dyn Store) {
    let j = job(0, 0);
    store
        .apply(WriteOp::EnqueueJob(j.clone()).into())
        .await
        .unwrap();
    is_not_found(
        store
            .fail_job(job(0, 0).id, "holder", "x", None, at(5))
            .await,
    );
    store
        .claim_job("holder", &kinds(), at(1), at(10))
        .await
        .unwrap()
        .unwrap();
    is_conflict(store.fail_job(j.id, "other", "x", None, at(5)).await);
    store.complete_job(j.id, "holder", json!(1)).await.unwrap();
    is_conflict(store.fail_job(j.id, "holder", "x", None, at(5)).await);
}

/// Finished jobs are pruned by age, oldest first and at most `limit` at a
/// time; queued and claimed ones never are. A finished job with no finish
/// time, from before they were kept, counts as finished at its `run_at`.
pub async fn finished_jobs_are_pruned_by_age(store: &dyn Store) {
    let waiting = job(0, 1);
    let done = job(9, 1);
    let mut dies = job(8, 1);
    dies.max_attempts = 1;
    // Years before the fixtures' clock, and before today's: the stamped
    // jobs finish now, by the store's clock.
    let mut legacy = job(0, -200_000_000);
    legacy.status = JobStatus::Succeeded;
    let ids = (waiting.id, done.id, dies.id, legacy.id);
    store
        .apply(
            UnitOfWork::new()
                .with(WriteOp::EnqueueJob(waiting))
                .with(WriteOp::EnqueueJob(done))
                .with(WriteOp::EnqueueJob(dies))
                .with(WriteOp::EnqueueJob(legacy)),
        )
        .await
        .unwrap();
    let first = store
        .claim_job("w", &kinds(), at(10), at(100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, ids.1);
    store.complete_job(ids.1, "w", json!(1)).await.unwrap();
    let second = store
        .claim_job("w", &kinds(), at(10), at(100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.id, ids.2);
    store
        .fail_job(ids.2, "w", "broke", None, at(20))
        .await
        .unwrap();
    let finished = |id| async move { store.get_job(id).await.unwrap().unwrap().finished_at };
    assert!(finished(ids.1).await.is_some(), "a success is stamped");
    assert!(finished(ids.2).await.is_some(), "a death is stamped");
    assert_eq!(finished(ids.0).await, None);
    assert_eq!(finished(ids.3).await, None, "a new job's stamp is ignored");

    // Only the legacy job finished before this, by its run_at.
    assert_eq!(store.prune_jobs(at(-100_000_000), 10).await.unwrap(), 1);
    assert_eq!(store.get_job(ids.3).await.unwrap(), None);
    let later = chrono::Utc::now() + chrono::TimeDelta::days(1);
    assert_eq!(store.prune_jobs(later, 1).await.unwrap(), 1);
    assert_eq!(store.prune_jobs(later, 10).await.unwrap(), 1);
    assert_eq!(store.prune_jobs(later, 10).await.unwrap(), 0);
    assert_eq!(store.get_job(ids.1).await.unwrap(), None);
    assert_eq!(store.get_job(ids.2).await.unwrap(), None);
    assert!(
        store.get_job(ids.0).await.unwrap().is_some(),
        "queued stays"
    );
}

/// A holder whose lease lapsed can still finish, until someone else has the
/// job; then it can't.
pub async fn a_lapsed_holder_finishes_only_until_the_job_is_claimed_again(store: &dyn Store) {
    let early = job(0, 0);
    let retaken = job(0, 0);
    store
        .apply(WriteOp::EnqueueJob(early.clone()).into())
        .await
        .unwrap();
    store
        .claim_job("slow", &kinds(), at(1), at(10))
        .await
        .unwrap()
        .unwrap();
    // Past the lease, and nobody else has claimed it.
    store
        .complete_job(early.id, "slow", json!("late"))
        .await
        .unwrap();
    assert_eq!(
        store.get_job(early.id).await.unwrap().unwrap().status,
        JobStatus::Succeeded
    );

    store
        .apply(WriteOp::EnqueueJob(retaken.clone()).into())
        .await
        .unwrap();
    store
        .claim_job("slow", &kinds(), at(20), at(30))
        .await
        .unwrap()
        .unwrap();
    let again = store
        .claim_job("fast", &kinds(), at(30), at(40))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.id, retaken.id);
    is_conflict(store.complete_job(retaken.id, "slow", json!(1)).await);
    store
        .complete_job(retaken.id, "fast", json!(2))
        .await
        .unwrap();
}

/// Each list goes on from an offset where the page before it stopped.
pub async fn lists_page_on_from_an_offset(store: &dyn Store) {
    for name in ["c", "a", "b"] {
        store
            .apply(WriteOp::PutDefinition(definition(DefinitionKind::Agent, name, 1)).into())
            .await
            .unwrap();
    }
    let names = |offset| async move {
        let listed = store
            .list_definitions(
                DefinitionKind::Agent,
                store::PageRequest::new(2, offset).unwrap(),
            )
            .await
            .unwrap();
        let names: Vec<String> = listed.items.into_iter().map(|d| d.key.name).collect();
        (names, listed.has_more)
    };
    assert_eq!(names(0).await, (vec!["a".to_owned(), "b".to_owned()], true));
    assert_eq!(names(2).await, (vec!["c".to_owned()], false));
    assert_eq!(names(3).await, (Vec::<String>::new(), false));

    for n in 0..3 {
        store
            .apply(WriteOp::EnqueueJob(job(0, n)).into())
            .await
            .unwrap();
    }
    let second = store
        .list_jobs(None, store::PageRequest::new(2, 2).unwrap())
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(!second.has_more);
}

/// A failure retries at the time given, and the last one stays visible.
pub async fn a_failing_job_retries_then_dies(store: &dyn Store) {
    let j = job(0, 0);
    store
        .apply(WriteOp::EnqueueJob(j.clone()).into())
        .await
        .unwrap();
    store
        .claim_job("w", &kinds(), at(1), at(10))
        .await
        .unwrap()
        .unwrap();
    store
        .fail_job(j.id, "w", "first failure", None, at(30))
        .await
        .unwrap();
    let queued = store.get_job(j.id).await.unwrap().unwrap();
    assert_eq!((queued.status, queued.run_at), (JobStatus::Queued, at(30)));
    assert_eq!(
        store
            .claim_job("w", &kinds(), at(29), at(40))
            .await
            .unwrap(),
        None
    );
    store
        .claim_job("w", &kinds(), at(30), at(40))
        .await
        .unwrap()
        .unwrap();
    store
        .fail_job(
            j.id,
            "w",
            "last failure",
            Some(json!({ "left": "this" })),
            at(50),
        )
        .await
        .unwrap();
    let dead = store.get_job(j.id).await.unwrap().unwrap();
    assert_eq!(dead.status, JobStatus::Dead);
    assert_eq!(dead.last_error.as_deref(), Some("last failure"));
    assert_eq!(dead.result, Some(json!({ "left": "this" })));
    assert_eq!(
        store
            .claim_job("w", &kinds(), at(60), at(70))
            .await
            .unwrap(),
        None
    );
}

/// A job can't be retried forever by workers that vanish.
pub async fn a_lease_lapsing_on_the_last_attempt_is_dead(store: &dyn Store) {
    let j = job(0, 0);
    store
        .apply(WriteOp::EnqueueJob(j.clone()).into())
        .await
        .unwrap();
    store
        .claim_job("w", &kinds(), at(1), at(10))
        .await
        .unwrap()
        .unwrap();
    store
        .claim_job("w", &kinds(), at(10), at(20))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .claim_job("w", &kinds(), at(20), at(30))
            .await
            .unwrap(),
        None
    );
    let dead = store.get_job(j.id).await.unwrap().unwrap();
    assert_eq!(dead.status, JobStatus::Dead);
    assert!(dead.last_error.is_some());
}

/// A lease that's already over is a caller's bug.
pub async fn a_lease_must_end_after_now(store: &dyn Store) {
    is_invalid(store.claim_job("w", &kinds(), at(5), at(5)).await);
}

// ---- definitions ----

fn definition(kind: DefinitionKind, name: &str, version: u32) -> Definition {
    Definition {
        key: DefinitionKey {
            kind,
            name: name.to_owned(),
        },
        body: json!({ "v": version }),
        source: format!("file/{name}.yaml"),
        updated_at: at(i64::from(version)),
    }
}

/// Writing a definition again replaces it, so there's never a shadow copy.
pub async fn a_definition_has_one_home(store: &dyn Store) {
    let first = definition(DefinitionKind::Agent, "a", 1);
    let second = definition(DefinitionKind::Agent, "a", 2);
    store
        .apply(WriteOp::PutDefinition(first.clone()).into())
        .await
        .unwrap();
    store
        .apply(WriteOp::PutDefinition(second.clone()).into())
        .await
        .unwrap();
    assert_eq!(
        store.get_definition(&first.key).await.unwrap().as_ref(),
        Some(&second)
    );
    let all = store
        .list_definitions(DefinitionKind::Agent, page(10))
        .await
        .unwrap();
    assert_eq!(all.items, [second.clone()]);
    store
        .apply(WriteOp::DeleteDefinition(first.key.clone()).into())
        .await
        .unwrap();
    assert_eq!(store.get_definition(&first.key).await.unwrap(), None);
}

/// Kinds don't mix, and names sort.
pub async fn definitions_list_by_kind(store: &dyn Store) {
    let b = definition(DefinitionKind::Workflow, "b", 1);
    let a = definition(DefinitionKind::Workflow, "a", 1);
    let agent = definition(DefinitionKind::Agent, "c", 1);
    for d in [&b, &a, &agent] {
        store
            .apply(WriteOp::PutDefinition(d.clone()).into())
            .await
            .unwrap();
    }
    let listed = store
        .list_definitions(DefinitionKind::Workflow, page(10))
        .await
        .unwrap();
    assert_eq!(listed.items, [a.clone(), b.clone()]);
}

// ---- records ----

/// A record needs a declared type.
pub async fn an_undeclared_record_type_is_refused(store: &dyn Store) {
    is_invalid(store.apply(WriteOp::PutRecord(item("a", 1)).into()).await);
    is_invalid(store.get_record(ITEM, item("a", 1).id).await);
}

/// A record must meet its schema, and the refusal undoes its unit.
pub async fn an_invalid_record_is_refused(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    let wrong_type = record(ITEM, json!({ "name": 7, "size": 1 }));
    let unknown_key = record(ITEM, json!({ "name": "a", "size": 1, "extra": true }));
    let too_long = record(ITEM, json!({ "name": "x".repeat(21), "size": 1 }));
    for bad in [wrong_type, unknown_key, too_long] {
        is_invalid(store.apply(WriteOp::PutRecord(bad).into()).await);
    }
    let good = item("good", 1);
    let unit = UnitOfWork::new()
        .with(WriteOp::PutRecord(good.clone()))
        .with(WriteOp::PutRecord(record(ITEM, json!({}))));
    is_invalid(store.apply(unit).await);
    assert_eq!(store.get_record(ITEM, good.id).await.unwrap(), None);
}

fn type_with_schema(name: &str, schema: serde_json::Value) -> RecordTypeDecl {
    RecordTypeDecl {
        name: name.to_owned(),
        schema,
        filterable: vec![],
    }
}

/// A plugin's schema has to be as strict as the core's.
pub async fn a_loose_record_schema_is_refused(store: &dyn Store) {
    let loose = type_with_schema(
        "test.loose",
        json!({ "type": "object", "additionalProperties": false,
                "properties": { "free": { "type": "string" } } }),
    );
    is_invalid(
        store
            .register_records(&RecordDecls {
                types: vec![loose],
                relations: vec![],
            })
            .await,
    );
}

/// Names are `plugin.type`, so plugins can't collide.
pub async fn a_badly_named_type_is_refused(store: &dyn Store) {
    let mut bad = decls();
    bad.relations.clear();
    bad.types[0].name = "no_plugin_prefix".to_owned();
    is_invalid(store.register_records(&bad).await);
}

/// Registration is all or nothing.
pub async fn a_failed_registration_adds_nothing(store: &dyn Store) {
    let mut bad = decls();
    bad.relations[0].to = "test.nothing".to_owned();
    is_invalid(store.register_records(&bad).await);
    is_invalid(store.get_record(ITEM, item("a", 1).id).await);
}

/// A plugin declares on every start; a changed declaration is a conflict.
pub async fn redeclaring_is_fine_but_changing_conflicts(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    store.register_records(&decls()).await.unwrap();
    let mut changed = decls();
    changed.types[1].filterable.push(FilterableField {
        name: "name".to_owned(),
        kind: FieldKind::Text,
    });
    is_conflict(store.register_records(&changed).await);
}

fn filter(field: &str, op: FieldOp, value: FieldValue) -> FieldFilter {
    FieldFilter::new(field, op, value)
}

fn query(filters: Vec<FieldFilter>) -> RecordQuery {
    let mut query = RecordQuery::new(ITEM, page(10));
    query.filters = filters;
    query
}

async fn put(store: &dyn Store, record: &Record) {
    store
        .apply(WriteOp::PutRecord(record.clone()).into())
        .await
        .unwrap();
}

/// Equality and ranges combine, in id order.
pub async fn records_filter_by_equality_and_range(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    let (small, medium, large) = (item("same", 1), item("same", 5), item("other", 9));
    for r in [&large, &small, &medium] {
        put(store, r).await;
    }
    assert_eq!(
        store.get_record(ITEM, small.id).await.unwrap().as_ref(),
        Some(&small)
    );

    let same = filter("name", FieldOp::Eq, FieldValue::Text("same".to_owned()));
    let at_least_two = filter("size", FieldOp::Gte, FieldValue::Integer(2));
    let page_of = store
        .query_records(query(vec![same.clone()]))
        .await
        .unwrap();
    assert_eq!(page_of.items, [small.clone(), medium.clone()]);
    let both = store
        .query_records(query(vec![same, at_least_two]))
        .await
        .unwrap();
    assert_eq!(both.items, [medium.clone()]);
}

/// `+01:00` and `Z` are compared as instants, never as text.
pub async fn timestamps_compare_as_times(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    let seen = |when: &str| record(ITEM, json!({ "name": "t", "size": 1, "seen": when }));
    // 09:30+01:00 is 08:30 UTC, though as text it sorts after 09:00Z.
    let early = seen("2027-01-01T09:30:00+01:00");
    let late = seen("2027-01-01T09:10:00Z");
    put(store, &early).await;
    put(store, &late).await;
    let cutoff = chrono::DateTime::parse_from_rfc3339("2027-01-01T09:00:00Z")
        .unwrap()
        .to_utc();
    let before = filter("seen", FieldOp::Lt, FieldValue::Timestamp(cutoff));
    assert_eq!(
        store
            .query_records(query(vec![before]))
            .await
            .unwrap()
            .items,
        [early.clone()]
    );
}

/// Only declared fields can be filtered, so a store can index them.
pub async fn a_filter_on_an_undeclared_field_is_refused(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    let bad = filter("nope", FieldOp::Eq, FieldValue::Integer(1));
    is_invalid(store.query_records(query(vec![bad])).await);
}

/// A filter's value must have the field's type.
pub async fn a_filter_of_the_wrong_type_is_refused(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    let bad = filter("size", FieldOp::Eq, FieldValue::Text("1".to_owned()));
    is_invalid(store.query_records(query(vec![bad])).await);
}

async fn owner_with_items(store: &dyn Store) -> (Record, Record, Record) {
    store.register_records(&decls()).await.unwrap();
    let owner = record(OWNER, json!({ "name": "o" }));
    let (one, two) = (item("one", 1), item("two", 2));
    for r in [&owner, &one, &two] {
        put(store, r).await;
    }
    for target in [&one, &two] {
        let link = WriteOp::Link {
            relation: OWNS.to_owned(),
            from: owner.id,
            to: target.id,
        };
        store.apply(link.into()).await.unwrap();
    }
    (owner, one, two)
}

/// A relation reads from either end.
pub async fn neighbors_follow_both_directions(store: &dyn Store) {
    let (owner, one, two) = owner_with_items(store).await;
    let out = store
        .neighbors(OWNS, owner.id, Direction::Outgoing, page(1))
        .await
        .unwrap();
    assert_eq!((out.items, out.has_more), (vec![one.clone()], true));
    let back = store
        .neighbors(OWNS, two.id, Direction::Incoming, page(5))
        .await
        .unwrap();
    assert_eq!(back.items, [owner.clone()]);
    is_invalid(
        store
            .neighbors("test.nope", owner.id, Direction::Outgoing, page(5))
            .await,
    );
    let unlink = WriteOp::Unlink {
        relation: OWNS.to_owned(),
        from: owner.id,
        to: one.id,
    };
    store.apply(unlink.into()).await.unwrap();
    let left = store
        .neighbors(OWNS, owner.id, Direction::Outgoing, page(5))
        .await
        .unwrap();
    assert_eq!(left.items, [two.clone()]);
}

/// A link can't dangle.
pub async fn a_link_needs_both_records(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    let owner = record(OWNER, json!({ "name": "o" }));
    put(store, &owner).await;
    let link = WriteOp::Link {
        relation: OWNS.to_owned(),
        from: owner.id,
        to: item("x", 1).id,
    };
    is_not_found(store.apply(link.into()).await);
}

/// Deleting a record leaves no link behind, even if the id comes back.
pub async fn deleting_a_record_removes_its_links(store: &dyn Store) {
    let (owner, one, two) = owner_with_items(store).await;
    let delete = WriteOp::DeleteRecord {
        record_type: ITEM.to_owned(),
        id: one.id,
    };
    store.apply(delete.into()).await.unwrap();
    let left = store
        .neighbors(OWNS, owner.id, Direction::Outgoing, page(5))
        .await
        .unwrap();
    assert_eq!(left.items, [two.clone()]);

    // A link that was only hidden would come back with the id.
    put(store, &one).await;
    let after = store
        .neighbors(OWNS, owner.id, Direction::Outgoing, page(5))
        .await
        .unwrap();
    assert_eq!(after.items, [two.clone()]);
}

fn day(number: u32) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2027, 1, number).unwrap()
}

/// Money is set aside up to the cap, and the first day's total is readable.
pub async fn spend_stays_within_the_cap(store: &dyn Store) {
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 0);
    assert!(store.reserve_spend("p", day(1), 6, 10).await.unwrap());
    assert!(store.reserve_spend("p", day(1), 4, 10).await.unwrap());
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 10);
}

/// A refusal leaves the books as they were.
pub async fn a_refused_reservation_changes_nothing(store: &dyn Store) {
    assert!(store.reserve_spend("p", day(1), 6, 10).await.unwrap());
    assert!(!store.reserve_spend("p", day(1), 5, 10).await.unwrap());
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 6);
}

/// What wasn't used comes back, and a dearer call than reserved counts.
pub async fn settling_replaces_the_reservation(store: &dyn Store) {
    assert!(store.reserve_spend("p", day(1), 10, 10).await.unwrap());
    store.settle_spend("p", day(1), 10, 3).await.unwrap();
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 3);
    assert!(store.reserve_spend("p", day(1), 7, 10).await.unwrap());
    store.settle_spend("p", day(1), 7, 9).await.unwrap();
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 12);
}

/// Each provider and each day has its own total.
pub async fn spend_is_per_provider_and_per_day(store: &dyn Store) {
    assert!(store.reserve_spend("p", day(1), 10, 10).await.unwrap());
    assert!(store.reserve_spend("q", day(1), 10, 10).await.unwrap());
    assert!(store.reserve_spend("p", day(2), 10, 10).await.unwrap());
    store.settle_spend("p", day(2), 10, 1).await.unwrap();
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 10);
    assert_eq!(store.spent_on("q", day(1)).await.unwrap(), 10);
    assert_eq!(store.spent_on("p", day(2)).await.unwrap(), 1);

    // A call that straddles midnight settles on its first day, and that
    // mustn't touch the next day's total.
    store.settle_spend("p", day(1), 10, 2).await.unwrap();
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 2);
    assert_eq!(store.spent_on("p", day(2)).await.unwrap(), 1);
}

/// Calls made together can't each see room that only one of them can have.
pub async fn reservations_at_once_cannot_overrun(store: &dyn Store) {
    let reserve = || store.reserve_spend("p", day(1), 4, 10);
    let (a, b, c, d) = tokio::join!(reserve(), reserve(), reserve(), reserve());
    let won = [a, b, c, d]
        .into_iter()
        .filter(|r| *r.as_ref().unwrap())
        .count();
    assert_eq!(won, 2);
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 8);
}

/// An amount past what the column holds is refused, not wrapped around.
pub async fn huge_amounts_are_refused_not_wrapped(store: &dyn Store) {
    assert!(
        store
            .reserve_spend("p", day(1), u64::MAX, u64::MAX)
            .await
            .unwrap()
    );
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), u64::MAX);
    assert!(!store.reserve_spend("p", day(1), 1, u64::MAX).await.unwrap());
}

/// A settle with no reservation behind it can't make the total negative.
pub async fn settling_never_goes_below_zero(store: &dyn Store) {
    store.settle_spend("p", day(1), 100, 1).await.unwrap();
    assert_eq!(store.spent_on("p", day(1)).await.unwrap(), 1);
}

/// Jobs can be listed, so a failure can be found.
pub async fn jobs_list_newest_first_and_by_status(store: &dyn Store) {
    let (old, new, done) = (job(0, 1), job(0, 5), job(9, 3));
    for queued in [&old, &new, &done] {
        store
            .apply(WriteOp::EnqueueJob(queued.clone()).into())
            .await
            .unwrap();
    }
    let claimed = store
        .claim_job("w", &kinds(), at(10), at(20))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, done.id, "the highest priority");
    store.complete_job(done.id, "w", json!(1)).await.unwrap();

    let all = store.list_jobs(None, page(10)).await.unwrap();
    let ids: Vec<_> = all.items.iter().map(|listed| listed.id).collect();
    assert_eq!(ids, [new.id, done.id, old.id]);
    let finished = store
        .list_jobs(Some(JobStatus::Succeeded), page(10))
        .await
        .unwrap();
    assert_eq!(finished.items.len(), 1);
    assert_eq!(finished.items[0].id, done.id);
    let one = store.list_jobs(None, page(1)).await.unwrap();
    assert!(one.has_more);
}
