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
        .claim_job("w", now, now + chrono::Duration::seconds(5))
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
