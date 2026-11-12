//! The conformance suite, on a live Neo4j.
//!
//! The cases need a database to themselves, and Neo4j Community has one, so
//! they take turns: each holds a lock, empties the database and runs. The
//! database must be a scratch one. The first run marks it, and a database
//! that holds data without the mark is refused rather than emptied.
//!
//! Set `NEO4J_TEST_URI` (and `NEO4J_TEST_PASSWORD`, and `NEO4J_TEST_USER` if
//! it isn't `neo4j`) to run them. Without it they pass without running, and
//! say so, unless `NEO4J_TEST_REQUIRED` is set, as it is in CI.

use neo4rs::{Graph, query};
use tokio::sync::{Mutex, MutexGuard};

use crate::{Neo4jConfig, Neo4jStore};

static TURN: Mutex<()> = Mutex::const_new(());

fn setting(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

async fn count(graph: &Graph, text: &str) -> i64 {
    let mut rows = graph.execute(query(text)).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    row.get::<i64>("n").unwrap()
}

/// A store on the emptied scratch database, and this case's turn with it.
async fn scratch() -> Option<(Neo4jStore, MutexGuard<'static, ()>)> {
    let Some(uri) = setting("NEO4J_TEST_URI") else {
        assert!(
            setting("NEO4J_TEST_REQUIRED").is_none(),
            "NEO4J_TEST_REQUIRED is set, and NEO4J_TEST_URI isn't"
        );
        eprintln!("skipped: NEO4J_TEST_URI isn't set");
        return None;
    };
    let turn = TURN.lock().await;
    let config = Neo4jConfig {
        uri,
        user: setting("NEO4J_TEST_USER").unwrap_or_else(|| "neo4j".to_owned()),
        password: setting("NEO4J_TEST_PASSWORD").unwrap_or_default(),
        database: setting("NEO4J_TEST_DATABASE"),
        embedding_dimension: store_conformance::DIMENSION,
    };
    let graph = Graph::new(&config.uri, &config.user, &config.password)
        .await
        .unwrap();
    let marked = count(&graph, "MATCH (m:ConformanceScratch) RETURN count(m) AS n").await > 0;
    if !marked {
        let nodes = count(&graph, "MATCH (n) RETURN count(n) AS n").await;
        assert_eq!(
            nodes, 0,
            "NEO4J_TEST_URI names a database that holds data and isn't marked as scratch; \
             the suite empties its database, so it won't use this one"
        );
        graph
            .run(query("CREATE (:ConformanceScratch)"))
            .await
            .unwrap();
    }
    graph
        .run(query(
            "MATCH (n) WHERE NOT n:ConformanceScratch DETACH DELETE n",
        ))
        .await
        .unwrap();
    let store = Neo4jStore::connect(&config).await.unwrap();
    Some((store, turn))
}

store_conformance::store_conformance!(held match scratch().await {
    Some(held) => held,
    None => return,
});

#[tokio::test]
async fn another_width_is_refused() {
    let Some((store, _turn)) = scratch().await else {
        return;
    };
    drop(store);
    let config = Neo4jConfig {
        uri: setting("NEO4J_TEST_URI").unwrap(),
        user: setting("NEO4J_TEST_USER").unwrap_or_else(|| "neo4j".to_owned()),
        password: setting("NEO4J_TEST_PASSWORD").unwrap_or_default(),
        database: setting("NEO4J_TEST_DATABASE"),
        embedding_dimension: store_conformance::DIMENSION + 1,
    };
    let error = Neo4jStore::connect(&config).await.unwrap_err();
    assert!(error.to_string().contains("width"), "{error}");
}

#[test]
fn the_password_isnt_printed() {
    let config = Neo4jConfig {
        uri: "bolt://db:7687".to_owned(),
        user: "neo4j".to_owned(),
        password: "hunter2".to_owned(),
        database: None,
        embedding_dimension: 3,
    };
    assert!(!format!("{config:?}").contains("hunter2"));
}

#[tokio::test]
async fn too_many_words_are_refused_by_name() {
    use store::{ClassFilter, NoteReader, PageRequest};
    let Some((store, _turn)) = scratch().await else {
        return;
    };
    let words: Vec<String> = (0..1025).map(|n| format!("w{n}")).collect();
    let page = PageRequest::new(5, 0).unwrap();
    let error = store
        .search_text(&words.join(" "), ClassFilter::Any, page)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("1024"), "{error}");
    // Repeats count once.
    let repeated = vec!["otter"; 2000].join(" ");
    assert!(
        store
            .search_text(&repeated, ClassFilter::Any, page)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_missing_lock_node_is_made_again() {
    use store::JobQueue;
    let Some((store, _turn)) = scratch().await else {
        return;
    };
    store
        .graph
        .run(query("MATCH (l:Lock) DELETE l"))
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let claimed = store
        .claim_job(
            "w",
            &["test".to_owned()],
            now,
            now + chrono::Duration::seconds(5),
        )
        .await
        .unwrap();
    assert!(claimed.is_none());
    let mut rows = store
        .graph
        .execute(query("MATCH (l:Lock) RETURN count(l) AS n"))
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<i64>("n").unwrap(), 1);
}

/// Notes enough that the vector index answers, each with one chunk,
/// every second one public, spread around the sphere.
/// A note of class `class` with one chunk, embedded as `embedding`.
fn one_note(n: u32, class: protocol::DataClass, embedding: Vec<f32>) -> Vec<store::WriteOp> {
    use protocol::{Actor, Trust};
    use store::{Chunk, ChunkId, Note, NoteId, Provenance, WriteOp};
    let id = NoteId::generate();
    let note = Note {
        id,
        title: format!("note {n}"),
        body: format!("note {n}"),
        tags: vec![],
        source_url: None,
        content_hash: format!("hash {n}"),
        provenance: Provenance {
            data_class: class,
            trust: Trust::Trusted,
            stored_by: Actor::new("test"),
            stored_at: chrono::Utc::now(),
        },
        version: 1,
        updated_at: chrono::Utc::now(),
    };
    let chunk = Chunk {
        id: ChunkId::generate(),
        note_id: id,
        ordinal: 0,
        text: format!("chunk {n}"),
        token_count: 2,
        embedding: Some(embedding),
    };
    vec![
        WriteOp::CreateNote(note),
        WriteOp::ReplaceChunks {
            note_id: id,
            chunks: vec![chunk],
        },
    ]
}

async fn many_notes(store: &Neo4jStore, count: u32) {
    use protocol::DataClass;
    use store::{Store, UnitOfWork};
    let mut unit = UnitOfWork::new();
    for n in 0..count {
        let class = if n % 2 == 0 {
            DataClass::Public
        } else {
            DataClass::Private
        };
        let angle = f64::from(n) * 0.618_034 * std::f64::consts::TAU;
        let height = f64::from(n) / f64::from(count) * 2.0 - 1.0;
        let across = (1.0 - height * height).sqrt();
        #[expect(clippy::cast_possible_truncation, reason = "test vectors")]
        let embedding = vec![
            (across * angle.cos()) as f32,
            (across * angle.sin()) as f32,
            height as f32,
        ];
        for op in one_note(n, class, embedding) {
            unit = unit.with(op);
        }
    }
    store.apply(unit).await.unwrap();
}

#[tokio::test]
async fn the_vector_index_answers_as_the_scan_would() {
    use store::{ClassFilter, PageRequest};
    let Some((store, _turn)) = scratch().await else {
        return;
    };
    many_notes(&store, 1500).await;
    let ids = |page: &store::Page<store::ChunkHit>| -> Vec<String> {
        page.items
            .iter()
            .map(|hit| hit.chunk.id.to_string())
            .collect()
    };
    let mut answered = 0;
    for (query, classes, limit, offset) in [
        ([1.0, 0.0, 0.0], ClassFilter::Any, 10, 0),
        ([0.0, 1.0, 0.2], ClassFilter::PublicOnly, 10, 0),
        ([0.3, -0.5, 0.8], ClassFilter::Any, 5, 20),
        ([-1.0, -1.0, -1.0], ClassFilter::PublicOnly, 20, 10),
    ] {
        let values: Vec<f64> = query.into_iter().collect();
        let page = PageRequest::new(limit, offset).unwrap();
        let scanned = store
            .search_vector_scan(&values, false, classes, page)
            .await
            .unwrap();
        let Some(indexed) = store
            .search_vector_index(&values, classes, page)
            .await
            .unwrap()
        else {
            continue;
        };
        answered += 1;
        assert_eq!(ids(&indexed), ids(&scanned), "{query:?} {classes:?}");
        assert_eq!(indexed.has_more, scanned.has_more);
    }
    assert!(answered >= 3, "the index answered {answered} of 4");
    // A page deeper than the index is asked for is scanned.
    let deep = PageRequest::new(50, 1990).unwrap();
    let found = store
        .search_vector_index(&[1.0, 0.0, 0.0], ClassFilter::Any, deep)
        .await
        .unwrap();
    assert!(found.is_none());
}

/// How the vector index compares with the scan as a store grows, at a real
/// embedding width. Run by hand against a database of its own, which it
/// fills: `NEO4J_BENCH_URI` (and `NEO4J_BENCH_PASSWORD`), then
/// `cargo test -p store-neo4j -- --ignored vector_search_at_size --nocapture`.
#[tokio::test]
#[ignore = "a measurement, run by hand against a database of its own"]
async fn vector_search_at_size() {
    use protocol::DataClass;
    use store::{ClassFilter, PageRequest, Store, UnitOfWork};
    const WIDTH: usize = 1024;
    let Some(uri) = setting("NEO4J_BENCH_URI") else {
        eprintln!("skipped: NEO4J_BENCH_URI isn't set");
        return;
    };
    let config = Neo4jConfig {
        uri,
        user: "neo4j".to_owned(),
        password: setting("NEO4J_BENCH_PASSWORD").unwrap_or_default(),
        database: None,
        embedding_dimension: WIDTH,
    };
    let store = Neo4jStore::connect(&config).await.unwrap();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut vector = move || -> Vec<f32> {
        (0..WIDTH)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                #[expect(clippy::cast_precision_loss, reason = "test vectors")]
                let unit = (seed >> 11) as f32 / (1_u64 << 53) as f32;
                unit * 2.0 - 1.0
            })
            .collect()
    };
    // A database filled by an earlier run is reused; sizes can be named.
    let mut rows = store
        .graph
        .execute(query("MATCH (c:Chunk) RETURN count(c) AS n"))
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let mut held = u32::try_from(row.get::<i64>("n").unwrap()).unwrap();
    let sizes: Vec<u32> = setting("NEO4J_BENCH_SIZES")
        .unwrap_or_else(|| "1000,10000,50000".to_owned())
        .split(',')
        .map(|size| size.trim().parse().unwrap())
        .collect();
    for size in sizes {
        while held < size {
            let mut unit = UnitOfWork::new();
            for _ in 0..250 {
                for op in one_note(held, DataClass::Private, vector()) {
                    unit = unit.with(op);
                }
                held += 1;
            }
            store.apply(unit).await.unwrap();
        }
        store
            .graph
            .run(query("CALL db.awaitIndexes(600)"))
            .await
            .unwrap();
        let page = PageRequest::new(30, 0).unwrap();
        let (mut scan_ms, mut index_ms, mut same, mut answered) = (0_u128, 0_u128, 0_usize, 0);
        for _ in 0..10 {
            let query_vector: Vec<f64> = vector().into_iter().map(f64::from).collect();
            let begun = std::time::Instant::now();
            let scanned = store
                .search_vector_scan(&query_vector, false, ClassFilter::Any, page)
                .await
                .unwrap();
            scan_ms += begun.elapsed().as_millis();
            let begun = std::time::Instant::now();
            let indexed = store
                .search_vector_index(&query_vector, ClassFilter::Any, page)
                .await
                .unwrap();
            index_ms += begun.elapsed().as_millis();
            if let Some(indexed) = indexed {
                answered += 1;
                same += indexed
                    .items
                    .iter()
                    .filter(|hit| {
                        scanned
                            .items
                            .iter()
                            .any(|other| other.chunk.id == hit.chunk.id)
                    })
                    .count();
            }
        }
        eprintln!(
            "{size} chunks: scan {} ms, index {} ms a search; index answered {answered}/10, \
             {same}/{} of the scan's top 30",
            scan_ms / 10,
            index_ms / 10,
            answered * 30
        );
    }
}
