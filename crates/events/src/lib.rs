//! The event sink. Telemetry and trajectories are high-volume appends that
//! are read for analysis, not served, so they stay out of the store and a
//! store plugin never has to take them.
//!
//! The port has no update and no delete: what happened stays what happened.

use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use protocol::{DataClass, RunId};
use serde_json::Value;

/// The kind of an event. A closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// A message in the run's conversation.
    Message,
    /// A call to a model.
    ModelCall,
    /// A call to a tool, whether it ran or was refused.
    ToolCall,
    /// How the run ended.
    RunEnded,
}

/// One thing that happened in a run.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The run it happened in.
    pub run: RunId,
    /// When it happened.
    pub at: DateTime<Utc>,
    /// What kind of thing it was.
    pub kind: EventKind,
    /// Who may see it. A trajectory can hold private content.
    pub data_class: DataClass,
    /// The details.
    pub body: Value,
}

/// A slice of a run's events.
#[derive(Debug, Clone, PartialEq)]
pub struct EventPage {
    /// The events, in the order they were appended.
    pub events: Vec<Event>,
    /// True when the run has more events after these.
    pub has_more: bool,
}

/// A sink could not do what was asked.
#[derive(Debug, thiserror::Error)]
#[error("event sink failed: {0}")]
pub struct EventError(pub String);

/// Where events go.
#[async_trait]
pub trait EventSink: Send + Sync + std::fmt::Debug {
    /// Appends events, keeping their order.
    ///
    /// # Errors
    ///
    /// [`EventError`] if they weren't all kept. The caller reports it: a lost
    /// trajectory is a visible failure, though it doesn't stop the run.
    async fn append(&self, events: Vec<Event>) -> Result<(), EventError>;

    /// Reads up to `limit` of a run's events, after skipping `offset`.
    ///
    /// # Errors
    ///
    /// [`EventError`] if the sink can't be read.
    async fn read_run(
        &self,
        run: RunId,
        offset: usize,
        limit: usize,
    ) -> Result<EventPage, EventError>;
}

/// A sink that keeps events in memory, for tests.
#[derive(Debug, Default)]
pub struct MemorySink(Mutex<Vec<Event>>);

impl MemorySink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EventSink for MemorySink {
    async fn append(&self, events: Vec<Event>) -> Result<(), EventError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(events);
        Ok(())
    }

    async fn read_run(
        &self,
        run: RunId,
        offset: usize,
        limit: usize,
    ) -> Result<EventPage, EventError> {
        let all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut matching = all.iter().filter(|event| event.run == run).skip(offset);
        let events: Vec<Event> = matching.by_ref().take(limit).cloned().collect();
        let has_more = matching.next().is_some();
        Ok(EventPage { events, has_more })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(run: RunId, n: u32) -> Event {
        Event {
            run,
            at: DateTime::UNIX_EPOCH,
            kind: EventKind::ToolCall,
            data_class: DataClass::Private,
            body: json!({ "n": n }),
        }
    }

    #[tokio::test]
    async fn a_run_reads_back_in_order_and_alone() {
        let sink = MemorySink::new();
        let (one, two) = (RunId::generate(), RunId::generate());
        sink.append(vec![event(one, 1), event(two, 9), event(one, 2)])
            .await
            .unwrap();
        let page = sink.read_run(one, 0, 10).await.unwrap();
        assert_eq!(page.events, vec![event(one, 1), event(one, 2)]);
        assert!(!page.has_more);
    }

    #[tokio::test]
    async fn a_short_read_says_there_is_more() {
        let sink = MemorySink::new();
        let run = RunId::generate();
        sink.append(vec![event(run, 1), event(run, 2), event(run, 3)])
            .await
            .unwrap();
        let first = sink.read_run(run, 0, 2).await.unwrap();
        assert!(first.has_more);
        let rest = sink.read_run(run, 2, 2).await.unwrap();
        assert_eq!(rest.events, vec![event(run, 3)]);
        assert!(!rest.has_more);
    }
}
