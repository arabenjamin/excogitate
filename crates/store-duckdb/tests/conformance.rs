//! The store conformance suite, and what's specific to `DuckDB`.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::path::PathBuf;

use protocol::RunId;
use store::{ClassFilter, NoteReader, PageRequest, SpendLog, Store, UnitOfWork, WriteOp};
use store_duckdb::{DuckDbConfig, DuckDbStore, OpenError};

/// Where `scripts/fetch_duckdb_extensions.sh` puts the extensions.
fn extension_dir() -> PathBuf {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.cache/duckdb-extensions");
    assert!(
        directory.exists(),
        "run scripts/fetch_duckdb_extensions.sh first"
    );
    directory
}

fn config(path: Option<PathBuf>, embedding_dimension: usize) -> DuckDbConfig {
    DuckDbConfig {
        path,
        extension_dir: extension_dir(),
        embedding_dimension,
    }
}

mod conformance {
    use super::*;

    store_conformance::store_conformance!(
        DuckDbStore::open(&config(None, store_conformance::DIMENSION)).unwrap()
    );
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("store-{}", RunId::generate()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("store.duckdb")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Leaving a temp directory behind harms nothing.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn note_unit(text: &str) -> (store::Note, UnitOfWork) {
    use chrono::Utc;
    use protocol::{Actor, DataClass, Trust};
    let note = store::Note {
        id: store::NoteId::generate(),
        title: "t".to_owned(),
        body: text.to_owned(),
        tags: vec![],
        source_url: None,
        content_hash: format!("hash-{text}"),
        provenance: store::Provenance {
            data_class: DataClass::Private,
            trust: Trust::Trusted,
            stored_by: Actor::new("test"),
            stored_at: Utc::now(),
        },
        version: 1,
        updated_at: Utc::now(),
    };
    let chunk = store::Chunk {
        id: store::ChunkId::generate(),
        note_id: note.id,
        ordinal: 0,
        text: text.to_owned(),
        token_count: 3,
        embedding: Some(vec![1.0, 0.0, 0.0]),
    };
    let unit = UnitOfWork::new()
        .with(WriteOp::CreateNote(note.clone()))
        .with(WriteOp::ReplaceChunks {
            note_id: note.id,
            chunks: vec![chunk],
        });
    (note, unit)
}

fn page() -> PageRequest {
    PageRequest::new(10, 0).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_and_search_survive_a_reopen() {
    let directory = TempDir::new();
    let store = DuckDbStore::open(&config(Some(directory.file()), 3)).unwrap();
    let (note, unit) = note_unit("durableword in 2026");
    store.apply(unit).await.unwrap();
    store.close();

    let store = DuckDbStore::open(&config(Some(directory.file()), 3)).unwrap();
    assert_eq!(
        store.get_note(note.id).await.unwrap().map(|n| n.body),
        Some(note.body)
    );
    let hits = store
        .search_text("durableword", ClassFilter::Any, page())
        .await
        .unwrap();
    assert_eq!(hits.items.len(), 1);
    let vectors = store
        .search_vector(&[1.0, 0.0, 0.0], ClassFilter::Any, page())
        .await
        .unwrap();
    assert_eq!(vectors.items.len(), 1);

    // The full-text index still follows writes after a reopen.
    let (_, later) = note_unit("laterword");
    store.apply(later).await.unwrap();
    let hits = store
        .search_text("laterword", ClassFilter::Any, page())
        .await
        .unwrap();
    assert_eq!(hits.items.len(), 1);
    store.close();
}

/// Dropping the store, not just closing it, finishes the writer's last
/// checkpoint before it returns. Otherwise a process can exit while the
/// writer still holds the file, and the store can't be reopened at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_store_has_let_go_of_its_file() {
    let directory = TempDir::new();
    for round in 0..10 {
        let store = DuckDbStore::open(&config(Some(directory.file()), 3)).unwrap();
        let (_, unit) = note_unit(&format!("round {round}"));
        store.apply(unit).await.unwrap();
        drop(store);
    }
    let store = DuckDbStore::open(&config(Some(directory.file()), 3)).unwrap();
    let hits = store
        .search_text("round", ClassFilter::Any, page())
        .await
        .unwrap();
    assert_eq!(hits.items.len(), 10);
}

/// Set in the child process of the crash test.
const CRASH_FILE: &str = "STORE_TEST_CRASH_FILE";

/// Printed by the crash test's child once its writes are committed.
const CRASH_READY: &str = "crash-child-ready";

fn crash_day() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2027, 1, 1).unwrap()
}

/// Writes two notes, says so, and waits to be killed. Runs only as the child
/// of `committed_writes_survive_a_crash`.
#[tokio::test(flavor = "multi_thread")]
async fn crash_child() {
    let Ok(path) = std::env::var(CRASH_FILE) else {
        return;
    };
    let store = DuckDbStore::open(&config(Some(PathBuf::from(path)), 3)).unwrap();
    for text in ["crashone", "crashtwo"] {
        store.apply(note_unit(text).1).await.unwrap();
    }
    assert!(store.reserve_spend("p", crash_day(), 7, 10).await.unwrap());
    println!("{CRASH_READY}");
    std::thread::sleep(std::time::Duration::from_secs(60));
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_writes_survive_a_crash() {
    use std::io::BufRead;

    let directory = TempDir::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--nocapture"])
        .env(CRASH_FILE, directory.file())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let ready = std::io::BufReader::new(stdout)
        .lines()
        .any(|line| line.is_ok_and(|line| line.contains(CRASH_READY)));
    assert!(ready, "the child never finished its writes");
    // SIGKILL: no chance to clean up, and unlike abort() it leaves no core dump.
    child.kill().unwrap();
    child.wait().unwrap();

    let store = DuckDbStore::open(&config(Some(directory.file()), 3)).unwrap();
    for word in ["crashone", "crashtwo"] {
        let hits = store
            .search_text(word, ClassFilter::Any, page())
            .await
            .unwrap();
        assert_eq!(hits.items.len(), 1, "{word}");
    }
    let vectors = store
        .search_vector(&[1.0, 0.0, 0.0], ClassFilter::Any, page())
        .await
        .unwrap();
    assert_eq!(vectors.items.len(), 2);
    // A crash can't reset a daily cap, and the reservation still counts.
    assert_eq!(store.spent_on("p", crash_day()).await.unwrap(), 7);
    assert!(!store.reserve_spend("p", crash_day(), 4, 10).await.unwrap());
    // Search still follows new writes after the log was replayed.
    store.apply(note_unit("aftercrash").1).await.unwrap();
    let hits = store
        .search_text("aftercrash", ClassFilter::Any, page())
        .await
        .unwrap();
    assert_eq!(hits.items.len(), 1);
    store.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_different_vector_width_is_refused() {
    let directory = TempDir::new();
    DuckDbStore::open(&config(Some(directory.file()), 3))
        .unwrap()
        .close();
    let error = DuckDbStore::open(&config(Some(directory.file()), 4)).unwrap_err();
    assert!(matches!(error, OpenError::Incompatible(_)), "{error}");
}

#[test]
fn a_missing_extension_names_where_it_looked() {
    let error = DuckDbStore::open(&DuckDbConfig {
        path: None,
        extension_dir: PathBuf::from("/nowhere/at/all"),
        embedding_dimension: 3,
    })
    .unwrap_err();
    assert!(
        matches!(&error, OpenError::Extension(m) if m.contains("/nowhere")),
        "{error}"
    );
}

#[test]
fn an_absurd_width_is_refused() {
    assert!(matches!(
        DuckDbStore::open(&config(None, 0)),
        Err(OpenError::Invalid(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn export_writes_parquet() {
    let directory = TempDir::new();
    let store = DuckDbStore::open(&config(Some(directory.file()), 3)).unwrap();
    let (_, unit) = note_unit("exported");
    store.apply(unit).await.unwrap();
    let target = directory.0.join("export");
    store.export(&target).await.unwrap();
    assert!(target.join("notes.parquet").exists());
    store.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_reads_and_writes_all_land() {
    let store = std::sync::Arc::new(DuckDbStore::open(&config(None, 3)).unwrap());
    let mut tasks = Vec::new();
    for n in 0..40 {
        let store = std::sync::Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let (note, unit) = note_unit(&format!("shared word{n}"));
            store.apply(unit).await.unwrap();
            store
                .search_text("shared", ClassFilter::Any, page())
                .await
                .unwrap();
            assert!(store.get_note(note.id).await.unwrap().is_some());
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let hits = store
        .search_text(
            "shared",
            ClassFilter::Any,
            PageRequest::new(100, 0).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hits.items.len(), 40);
}

#[test]
fn the_fetch_script_matches_the_linked_library() {
    let script = include_str!("../../../scripts/fetch_duckdb_extensions.sh");
    let connection = duckdb_version();
    assert!(
        script.contains(&format!("DUCKDB_VERSION={connection}")),
        "{connection}"
    );
}

fn duckdb_version() -> String {
    duckdb::Connection::open_in_memory()
        .unwrap()
        .query_row("SELECT version()", [], |row| row.get(0))
        .unwrap()
}
