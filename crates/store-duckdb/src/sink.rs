//! The default event sink: an append-only table in its own `DuckDB` file, so
//! high-volume telemetry never competes with the store's writer.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use duckdb::{Connection, params};
use events::{Event, EventError, EventKind, EventPage, EventSink};
use protocol::{DataClass, RunId};
use serde_json::Value;

fn sink_error(error: impl std::fmt::Display) -> EventError {
    EventError(schema::bounded(&error.to_string()))
}

const fn kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Message => "message",
        EventKind::ModelCall => "model_call",
        EventKind::ToolCall => "tool_call",
        EventKind::RunEnded => "run_ended",
    }
}

fn kind_from(name: &str) -> Result<EventKind, EventError> {
    match name {
        "message" => Ok(EventKind::Message),
        "model_call" => Ok(EventKind::ModelCall),
        "tool_call" => Ok(EventKind::ToolCall),
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
                     kind VARCHAR NOT NULL, data_class VARCHAR NOT NULL, body VARCHAR NOT NULL);",
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
            .map(|_| ())
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
}
