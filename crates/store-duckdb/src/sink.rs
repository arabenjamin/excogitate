//! The default event sink: an append-only table in its own `DuckDB` file, so
//! high-volume telemetry never competes with the store's writer.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use duckdb::{Connection, params};
use events::{
    Event, EventError, EventKind, EventPage, EventSink, IndexEntry, RunPage, RunQuery, RunSummary,
    Visibility,
};
use protocol::{DataClass, RunId};
use serde_json::Value;

fn sink_error(error: impl std::fmt::Display) -> EventError {
    EventError(schema::bounded(&error.to_string()))
}

const fn kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::RunStarted => "run_started",
        EventKind::Message => "message",
        EventKind::ModelCall => "model_call",
        EventKind::ToolCall => "tool_call",
        EventKind::Decision => "decision",
        EventKind::RunEnded => "run_ended",
    }
}

fn kind_from(name: &str) -> Result<EventKind, EventError> {
    match name {
        "run_started" => Ok(EventKind::RunStarted),
        "message" => Ok(EventKind::Message),
        "model_call" => Ok(EventKind::ModelCall),
        "tool_call" => Ok(EventKind::ToolCall),
        "decision" => Ok(EventKind::Decision),
        "run_ended" => Ok(EventKind::RunEnded),
        other => Err(EventError(format!("unknown event kind {other}"))),
    }
}

#[derive(Debug)]
struct Inner {
    connection: Connection,
    next_sequence: i64,
}

/// Events in a `DuckDB` file.
#[derive(Debug, Clone)]
pub struct DuckDbSink(Arc<Mutex<Inner>>);

impl DuckDbSink {
    /// Opens or creates the sink. `None` keeps it in memory, for tests.
    ///
    /// # Errors
    ///
    /// [`EventError`] if the file can't be opened.
    pub fn open(path: Option<&Path>) -> Result<Self, EventError> {
        let connection = match path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        }
        .map_err(sink_error)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS events (
                     sequence BIGINT NOT NULL, run VARCHAR NOT NULL, occurred_at TIMESTAMP NOT NULL,
                     kind VARCHAR NOT NULL, data_class VARCHAR NOT NULL, body VARCHAR NOT NULL);
                 CREATE TABLE IF NOT EXISTS runs (
                     sequence BIGINT NOT NULL, run VARCHAR NOT NULL, agent VARCHAR NOT NULL,
                     harness VARCHAR NOT NULL, started_at TIMESTAMP NOT NULL,
                     ended_at TIMESTAMP, outcome VARCHAR);
                 -- Who asked for a run, kept since 0.24; a log from before
                 -- gains the column, and its runs have none.
                 ALTER TABLE runs ADD COLUMN IF NOT EXISTS requested_by VARCHAR;
                 -- Whether only whoever asked, and the owner, may see it.
                 ALTER TABLE runs ADD COLUMN IF NOT EXISTS private BOOLEAN;",
            )
            .map_err(sink_error)?;
        let next_sequence = connection
            .query_row(
                "SELECT coalesce(max(sequence), 0) + 1 FROM events",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sink_error)?;
        Ok(Self(Arc::new(Mutex::new(Inner {
            connection,
            next_sequence,
        }))))
    }
}

fn append_all(inner: &mut Inner, events: &[Event]) -> Result<(), EventError> {
    inner
        .connection
        .execute_batch("BEGIN")
        .map_err(sink_error)?;
    let mut sequence = inner.next_sequence;
    let result = events.iter().try_for_each(|event| {
        let data_class = match event.data_class {
            DataClass::Private => "private",
            DataClass::Public => "public",
        };
        sequence += 1;
        inner
            .connection
            .execute(
                "INSERT INTO events VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    sequence,
                    event.run.to_string(),
                    event.at,
                    kind_name(event.kind),
                    data_class,
                    event.body.to_string(),
                ],
            )
            .map(|_| ())?;
        index(&inner.connection, sequence, event)
    });
    match result {
        Ok(()) => {
            inner
                .connection
                .execute_batch("COMMIT")
                .map_err(sink_error)?;
            inner.next_sequence = sequence;
            Ok(())
        }
        Err(error) => {
            // The first error is the one worth reporting.
            let _ = inner.connection.execute_batch("ROLLBACK");
            Err(sink_error(error))
        }
    }
}

/// Keeps the run index, in the same transaction as the event.
fn index(connection: &Connection, sequence: i64, event: &Event) -> duckdb::Result<()> {
    match event.index_entry() {
        Some(IndexEntry::Started {
            agent,
            harness,
            requested_by,
            private,
        }) => connection
            .execute(
                "INSERT INTO runs (sequence, run, agent, harness, started_at, requested_by, private)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    sequence,
                    event.run.to_string(),
                    agent,
                    harness,
                    event.at,
                    requested_by,
                    private
                ],
            )
            .map(|_| ()),
        Some(IndexEntry::Ended { outcome }) => connection
            .execute(
                "UPDATE runs SET ended_at = ?, outcome = ? WHERE run = ?",
                params![event.at, outcome, event.run.to_string()],
            )
            .map(|_| ()),
        None => Ok(()),
    }
}

/// The filters every run listing shares, in SQL, then whether a run is shown:
/// the same parameters, in the same order, for the page and for the count of
/// what was hidden.
const RUN_FILTERS: &str =
    "(?::VARCHAR IS NULL OR agent = ?) AND (?::VARCHAR IS NULL OR harness = ?)
     AND (?::VARCHAR IS NULL OR requested_by = ?)";
const RUN_SHOWN: &str = "(? = 'all' OR NOT coalesce(private, false)
     OR (? = 'own' AND requested_by = ?))";

/// A visibility as the SQL above takes it: its mode and its actor.
fn visibility(visible: &Visibility) -> (&'static str, Option<&str>) {
    match visible {
        Visibility::All => ("all", None),
        Visibility::Own(actor) => ("own", Some(actor.as_str())),
        Visibility::Shared => ("shared", None),
    }
}

type RunRow = (
    String,
    String,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
    Option<bool>,
);

fn list_page(inner: &Inner, query: &RunQuery) -> Result<RunPage, EventError> {
    let limit = i64::try_from(query.limit).map_err(sink_error)?;
    let offset = i64::try_from(query.offset).map_err(sink_error)?;
    let (mode, actor) = visibility(&query.visible);
    let mut statement = inner
        .connection
        .prepare(&format!(
            "SELECT run, agent, harness, started_at, ended_at, outcome, requested_by, private
             FROM runs WHERE {RUN_FILTERS} AND {RUN_SHOWN}
             ORDER BY sequence DESC LIMIT ? OFFSET ?"
        ))
        .map_err(sink_error)?;
    let rows = statement
        .query_map(
            params![
                query.agent,
                query.agent,
                query.harness,
                query.harness,
                query.requested_by,
                query.requested_by,
                mode,
                mode,
                actor,
                limit + 1,
                offset
            ],
            |row| -> duckdb::Result<RunRow> {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .map_err(sink_error)?;
    let mut runs = Vec::new();
    for row in rows {
        let (run, agent, harness, started_at, ended_at, outcome, requested_by, private) =
            row.map_err(sink_error)?;
        runs.push(RunSummary {
            run: run.parse().map_err(sink_error)?,
            agent,
            harness,
            requested_by,
            private: private.unwrap_or(false),
            started_at,
            ended_at,
            outcome,
        });
    }
    let has_more = runs.len() > query.limit;
    runs.truncate(query.limit);
    let hidden: i64 = inner
        .connection
        .query_row(
            &format!("SELECT count(*) FROM runs WHERE {RUN_FILTERS} AND NOT {RUN_SHOWN}"),
            params![
                query.agent,
                query.agent,
                query.harness,
                query.harness,
                query.requested_by,
                query.requested_by,
                mode,
                mode,
                actor
            ],
            |row| row.get(0),
        )
        .map_err(sink_error)?;
    Ok(RunPage {
        runs,
        has_more,
        hidden: u64::try_from(hidden).unwrap_or(0),
    })
}

fn read_page(inner: &Inner, run: RunId, offset: i64, limit: i64) -> Result<EventPage, EventError> {
    let mut statement = inner
        .connection
        .prepare(
            "SELECT occurred_at, kind, data_class, body FROM events WHERE run = ?
             ORDER BY sequence LIMIT ? OFFSET ?",
        )
        .map_err(sink_error)?;
    let rows = statement
        .query_map(params![run.to_string(), limit + 1, offset], |row| {
            Ok((
                row.get::<_, DateTime<Utc>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sink_error)?;
    let mut events = Vec::new();
    for row in rows {
        let (at, kind, data_class, body) = row.map_err(sink_error)?;
        events.push(Event {
            run,
            at,
            kind: kind_from(&kind)?,
            data_class: if data_class == "public" {
                DataClass::Public
            } else {
                DataClass::Private
            },
            body: serde_json::from_str::<Value>(&body).map_err(sink_error)?,
        });
    }
    let has_more = events.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    events.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(EventPage { events, has_more })
}

#[async_trait]
impl EventSink for DuckDbSink {
    async fn append(&self, events: Vec<Event>) -> Result<(), EventError> {
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            append_all(
                &mut inner.lock().unwrap_or_else(PoisonError::into_inner),
                &events,
            )
        })
        .await
        .map_err(sink_error)?
    }

    async fn read_run(
        &self,
        run: RunId,
        offset: usize,
        limit: usize,
    ) -> Result<EventPage, EventError> {
        let (offset, limit) = (
            i64::try_from(offset).map_err(sink_error)?,
            i64::try_from(limit).map_err(sink_error)?,
        );
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            read_page(
                &inner.lock().unwrap_or_else(PoisonError::into_inner),
                run,
                offset,
                limit,
            )
        })
        .await
        .map_err(sink_error)?
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, EventError> {
        let inner = Arc::clone(&self.0);
        let query = query.clone();
        tokio::task::spawn_blocking(move || {
            list_page(
                &inner.lock().unwrap_or_else(PoisonError::into_inner),
                &query,
            )
        })
        .await
        .map_err(sink_error)?
    }

    async fn prune_runs(&self, before: DateTime<Utc>, limit: u32) -> Result<u32, EventError> {
        let limit = i64::from(limit);
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            prune(
                &mut inner.lock().unwrap_or_else(PoisonError::into_inner),
                before,
                limit,
            )
        })
        .await
        .map_err(sink_error)?
    }
}

/// Deletes the oldest whole runs started before `before`: their events and
/// index rows in one transaction, so no run is left in part.
fn prune(inner: &mut Inner, before: DateTime<Utc>, limit: i64) -> Result<u32, EventError> {
    let transaction = inner.connection.transaction().map_err(sink_error)?;
    let runs: Vec<String> = {
        let mut statement = transaction
            .prepare(
                "SELECT run FROM events GROUP BY run HAVING min(occurred_at) < ?
                 ORDER BY min(occurred_at), run LIMIT ?",
            )
            .map_err(sink_error)?;
        statement
            .query_map(params![before, limit], |row| row.get::<_, String>(0))
            .map_err(sink_error)?
            .collect::<Result<_, _>>()
            .map_err(sink_error)?
    };
    for run in &runs {
        transaction
            .execute("DELETE FROM events WHERE run = ?", params![run])
            .map_err(sink_error)?;
        transaction
            .execute("DELETE FROM runs WHERE run = ?", params![run])
            .map_err(sink_error)?;
    }
    transaction.commit().map_err(sink_error)?;
    Ok(u32::try_from(runs.len()).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(run: RunId, n: i64) -> Event {
        Event {
            run,
            at: DateTime::from_timestamp(1_800_000_000 + n, 0).unwrap(),
            kind: EventKind::ToolCall,
            data_class: DataClass::Public,
            body: json!({ "n": n }),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_reads_back_in_order_and_alone() {
        let sink = DuckDbSink::open(None).unwrap();
        let (one, two) = (RunId::generate(), RunId::generate());
        sink.append(vec![event(one, 1), event(two, 9), event(one, 2)])
            .await
            .unwrap();
        let page = sink.read_run(one, 0, 10).await.unwrap();
        assert_eq!(page.events, vec![event(one, 1), event(one, 2)]);
        assert!(!page.has_more);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_short_read_says_there_is_more() {
        let sink = DuckDbSink::open(None).unwrap();
        let run = RunId::generate();
        sink.append(vec![event(run, 1), event(run, 2), event(run, 3)])
            .await
            .unwrap();
        assert!(sink.read_run(run, 0, 2).await.unwrap().has_more);
        let rest = sink.read_run(run, 2, 2).await.unwrap();
        assert_eq!(rest.events, vec![event(run, 3)]);
        assert!(!rest.has_more);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_survive_a_reopen() {
        let directory = std::env::temp_dir().join(format!("sink-{}", RunId::generate()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.duckdb");
        let run = RunId::generate();
        {
            let sink = DuckDbSink::open(Some(&path)).unwrap();
            sink.append(vec![event(run, 1)]).await.unwrap();
        }
        let sink = DuckDbSink::open(Some(&path)).unwrap();
        sink.append(vec![event(run, 2)]).await.unwrap();
        let page = sink.read_run(run, 0, 10).await.unwrap();
        assert_eq!(page.events, vec![event(run, 1), event(run, 2)]);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    fn marker(run: RunId, kind: EventKind, at: i64, body: Value) -> Event {
        Event {
            run,
            at: DateTime::from_timestamp(at, 0).unwrap(),
            kind,
            data_class: DataClass::Private,
            body,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_run_index_lists_newest_first_and_survives_a_reopen() {
        let directory = std::env::temp_dir().join(format!("sink-{}", RunId::generate()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.duckdb");
        let (one, two) = (RunId::generate(), RunId::generate());
        let start = |run, agent: &str, at| {
            marker(
                run,
                EventKind::RunStarted,
                at,
                json!({ "agent": agent, "harness": "h1" }),
            )
        };
        {
            let sink = DuckDbSink::open(Some(&path)).unwrap();
            sink.append(vec![
                start(one, "chat", 1_800_000_000),
                start(two, "agent:summarizer", 1_800_000_001),
                marker(
                    one,
                    EventKind::RunEnded,
                    1_800_000_002,
                    json!({ "outcome": "answered" }),
                ),
            ])
            .await
            .unwrap();
        }
        let sink = DuckDbSink::open(Some(&path)).unwrap();
        let all = sink
            .list_runs(&RunQuery {
                limit: 10,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        let listed: Vec<RunId> = all.runs.iter().map(|run| run.run).collect();
        assert_eq!(listed, vec![two, one]);
        assert_eq!(all.runs[1].outcome.as_deref(), Some("answered"));
        assert_eq!(
            all.runs[1].ended_at,
            DateTime::from_timestamp(1_800_000_002, 0)
        );
        let chats = sink
            .list_runs(&RunQuery {
                agent: Some("chat".to_owned()),
                limit: 10,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(chats.runs.len(), 1);
        let none = sink
            .list_runs(&RunQuery {
                harness: Some("other".to_owned()),
                limit: 10,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(none.runs, vec![]);
        assert!(!none.has_more);
        let first = sink
            .list_runs(&RunQuery {
                limit: 1,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        assert!(first.has_more);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn who_asked_is_kept_and_filtered_and_an_older_log_gains_it() {
        let directory = std::env::temp_dir().join(format!("sink-{}", RunId::generate()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.duckdb");
        // A log as 0.23 made it: the run index has no requested_by.
        let old = RunId::generate();
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE events (
                         sequence BIGINT NOT NULL, run VARCHAR NOT NULL,
                         occurred_at TIMESTAMP NOT NULL, kind VARCHAR NOT NULL,
                         data_class VARCHAR NOT NULL, body VARCHAR NOT NULL);
                     CREATE TABLE runs (
                         sequence BIGINT NOT NULL, run VARCHAR NOT NULL, agent VARCHAR NOT NULL,
                         harness VARCHAR NOT NULL, started_at TIMESTAMP NOT NULL,
                         ended_at TIMESTAMP, outcome VARCHAR);",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO runs VALUES (1, ?, 'chat', 'h1', '2027-01-15 08:00:00', NULL, NULL)",
                    params![old.to_string()],
                )
                .unwrap();
        }
        let sink = DuckDbSink::open(Some(&path)).unwrap();
        let asked = RunId::generate();
        sink.append(vec![marker(
            asked,
            EventKind::RunStarted,
            1_900_000_000,
            json!({ "agent": "chat", "harness": "h1", "requested_by": "person:sam" }),
        )])
        .await
        .unwrap();
        let all = sink
            .list_runs(&RunQuery {
                limit: 10,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        let seen: Vec<(RunId, Option<&str>)> = all
            .runs
            .iter()
            .map(|run| (run.run, run.requested_by.as_deref()))
            .collect();
        assert_eq!(seen, vec![(asked, Some("person:sam")), (old, None)]);
        let sams = sink
            .list_runs(&RunQuery {
                requested_by: Some("person:sam".to_owned()),
                limit: 10,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(sams.runs.len(), 1);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_private_run_shows_to_whoever_asked_and_is_counted_for_others() {
        let sink = DuckDbSink::open(None).unwrap();
        let (sams, shared) = (RunId::generate(), RunId::generate());
        sink.append(vec![
            marker(
                sams,
                EventKind::RunStarted,
                1_900_000_000,
                json!({ "agent": "chat", "harness": "h", "requested_by": "person:sam",
                        "private": true }),
            ),
            marker(
                shared,
                EventKind::RunStarted,
                1_900_000_001,
                json!({ "agent": "chat", "harness": "h", "requested_by": "person:ada" }),
            ),
        ])
        .await
        .unwrap();
        let seen = |visible: Visibility| {
            let sink = &sink;
            async move {
                let page = sink
                    .list_runs(&RunQuery {
                        limit: 10,
                        visible,
                        ..RunQuery::default()
                    })
                    .await
                    .unwrap();
                let runs: Vec<RunId> = page.runs.iter().map(|run| run.run).collect();
                (runs, page.hidden)
            }
        };
        assert_eq!(seen(Visibility::All).await, (vec![shared, sams], 0));
        assert_eq!(
            seen(Visibility::Own("person:sam".to_owned())).await,
            (vec![shared, sams], 0)
        );
        assert_eq!(
            seen(Visibility::Own("person:ada".to_owned())).await,
            (vec![shared], 1)
        );
        assert_eq!(seen(Visibility::Shared).await, (vec![shared], 1));
        let all = sink
            .list_runs(&RunQuery {
                limit: 10,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        assert!(all.runs.iter().any(|run| run.run == sams && run.private));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_old_run_is_pruned_whole_oldest_first() {
        let sink = DuckDbSink::open(None).unwrap();
        let (oldest, old, new) = (RunId::generate(), RunId::generate(), RunId::generate());
        let start = |run, at| {
            marker(
                run,
                EventKind::RunStarted,
                at,
                json!({ "agent": "chat", "harness": "h1" }),
            )
        };
        sink.append(vec![
            start(oldest, 1_800_000_000),
            start(old, 1_800_000_010),
            event(oldest, 20),
            start(new, 1_800_000_100),
        ])
        .await
        .unwrap();
        let cutoff = DateTime::from_timestamp(1_800_000_050, 0).unwrap();
        // One at a time, the oldest first.
        assert_eq!(sink.prune_runs(cutoff, 1).await.unwrap(), 1);
        assert_eq!(
            sink.read_run(oldest, 0, 10).await.unwrap().events,
            Vec::<Event>::new()
        );
        assert_eq!(sink.read_run(old, 0, 10).await.unwrap().events.len(), 1);
        assert_eq!(sink.prune_runs(cutoff, 10).await.unwrap(), 1);
        assert_eq!(sink.prune_runs(cutoff, 10).await.unwrap(), 0);
        let listed = sink
            .list_runs(&RunQuery {
                limit: 10,
                ..RunQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(
            listed.runs.iter().map(|run| run.run).collect::<Vec<_>>(),
            [new]
        );
    }
}
