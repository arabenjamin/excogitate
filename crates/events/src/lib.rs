//! The event sink. Telemetry and trajectories are high-volume appends that
//! are read for analysis, not served, so they stay out of the store and a
//! store never has to take them.
//!
//! The port has no update, and its one delete is retention: a whole run, all
//! its events and its index entry, once it's older than the owner keeps them.
//! While a run is kept, what happened stays what happened.

use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use protocol::{DataClass, RunId};
use serde_json::Value;

/// The kind of an event. A closed list.
///
/// Two kinds feed the run index, so their bodies have fields a sink reads: a
/// [`EventKind::RunStarted`] body has string `agent` and `harness` fields, and
/// a [`EventKind::RunEnded`] body has a string `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// Who is running, under which harness, with what.
    RunStarted,
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

/// A run, as the index shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    /// The run.
    pub run: RunId,
    /// What ran: `chat`, `agent:<name>` or `tool:<name>`.
    pub agent: String,
    /// The harness it ran under, as an id: two runs with the same id had the
    /// same instructions, tools and limits.
    pub harness: String,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it ended. A run with none is still going, or was cut short.
    pub ended_at: Option<DateTime<Utc>>,
    /// How it ended.
    pub outcome: Option<String>,
}

/// Which runs to list, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunQuery {
    /// Only this agent's runs.
    pub agent: Option<String>,
    /// Only runs under this harness.
    pub harness: Option<String>,
    /// How many to skip.
    pub offset: usize,
    /// The most to return.
    pub limit: usize,
}

/// A page of runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPage {
    /// The runs, newest first.
    pub runs: Vec<RunSummary>,
    /// True when more match.
    pub has_more: bool,
}

/// What the run index reads from an event, if it's one the index keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexEntry {
    /// A run started.
    Started {
        /// What ran.
        agent: String,
        /// Its harness id.
        harness: String,
    },
    /// A run ended.
    Ended {
        /// How.
        outcome: String,
    },
}

impl Event {
    /// What this event adds to the run index. A start or an end without its
    /// fields adds nothing, and the run's events are still kept.
    #[must_use]
    pub fn index_entry(&self) -> Option<IndexEntry> {
        let text = |field: &str| {
            self.body
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        match self.kind {
            EventKind::RunStarted => Some(IndexEntry::Started {
                agent: text("agent")?,
                harness: text("harness")?,
            }),
            EventKind::RunEnded => Some(IndexEntry::Ended {
                outcome: text("outcome")?,
            }),
            _ => None,
        }
    }
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

    /// Lists runs from the index, newest first.
    ///
    /// # Errors
    ///
    /// [`EventError`] if the sink can't be read.
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, EventError>;

    /// Deletes up to `limit` whole runs whose first event is before
    /// `before`, oldest first, and says how many went. A run is never cut
    /// in part: all its events and its index entry go together.
    ///
    /// # Errors
    ///
    /// [`EventError`] if the sink fails; what it deleted before failing is
    /// gone whole, run by run.
    async fn prune_runs(&self, before: DateTime<Utc>, limit: u32) -> Result<u32, EventError>;
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

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, EventError> {
        let all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut runs: Vec<RunSummary> = Vec::new();
        for event in all.iter() {
            match event.index_entry() {
                Some(IndexEntry::Started { agent, harness }) => runs.push(RunSummary {
                    run: event.run,
                    agent,
                    harness,
                    started_at: event.at,
                    ended_at: None,
                    outcome: None,
                }),
                Some(IndexEntry::Ended { outcome }) => {
                    if let Some(found) = runs.iter_mut().rev().find(|run| run.run == event.run) {
                        found.ended_at = Some(event.at);
                        found.outcome = Some(outcome);
                    }
                }
                None => {}
            }
        }
        let mut matching = runs
            .into_iter()
            .rev()
            .filter(|run| query.agent.as_ref().is_none_or(|agent| *agent == run.agent))
            .filter(|run| {
                query
                    .harness
                    .as_ref()
                    .is_none_or(|harness| *harness == run.harness)
            })
            .skip(query.offset);
        let runs: Vec<RunSummary> = matching.by_ref().take(query.limit).collect();
        let has_more = matching.next().is_some();
        Ok(RunPage { runs, has_more })
    }

    async fn prune_runs(&self, before: DateTime<Utc>, limit: u32) -> Result<u32, EventError> {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut first: std::collections::HashMap<RunId, DateTime<Utc>> =
            std::collections::HashMap::new();
        for event in all.iter() {
            first
                .entry(event.run)
                .and_modify(|at| *at = (*at).min(event.at))
                .or_insert(event.at);
        }
        let mut old: Vec<(DateTime<Utc>, String, RunId)> = first
            .into_iter()
            .filter(|(_, at)| *at < before)
            .map(|(run, at)| (at, run.to_string(), run))
            .collect();
        old.sort();
        old.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let gone: std::collections::HashSet<RunId> = old.iter().map(|(_, _, run)| *run).collect();
        all.retain(|event| !gone.contains(&event.run));
        Ok(u32::try_from(gone.len()).unwrap_or(u32::MAX))
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

    fn started(run: RunId, agent: &str, at: i64) -> Event {
        Event {
            run,
            at: DateTime::from_timestamp(at, 0).unwrap(),
            kind: EventKind::RunStarted,
            data_class: DataClass::Private,
            body: json!({ "agent": agent, "harness": "h1" }),
        }
    }

    fn ended(run: RunId, at: i64) -> Event {
        Event {
            run,
            at: DateTime::from_timestamp(at, 0).unwrap(),
            kind: EventKind::RunEnded,
            data_class: DataClass::Private,
            body: json!({ "outcome": "answered" }),
        }
    }

    #[tokio::test]
    async fn runs_list_newest_first_with_how_they_ended() {
        let sink = MemorySink::new();
        let (one, two, three) = (RunId::generate(), RunId::generate(), RunId::generate());
        sink.append(vec![
            started(one, "chat", 10),
            started(two, "agent:summarizer", 11),
            event(one, 1),
            ended(one, 12),
            started(three, "chat", 13),
        ])
        .await
        .unwrap();
        let query = RunQuery {
            agent: Some("chat".to_owned()),
            limit: 10,
            ..RunQuery::default()
        };
        let page = sink.list_runs(&query).await.unwrap();
        let listed: Vec<RunId> = page.runs.iter().map(|run| run.run).collect();
        assert_eq!(listed, vec![three, one]);
        assert_eq!(page.runs[1].outcome.as_deref(), Some("answered"));
        assert_eq!(page.runs[0].outcome, None);
        let first = RunQuery {
            limit: 1,
            ..RunQuery::default()
        };
        assert!(sink.list_runs(&first).await.unwrap().has_more);
    }

    #[test]
    fn a_start_without_its_fields_adds_nothing_to_the_index() {
        let mut bare = started(RunId::generate(), "chat", 1);
        bare.body = json!({ "agent": "chat" });
        assert_eq!(bare.index_entry(), None);
    }

    #[tokio::test]
    async fn an_old_run_is_pruned_whole_and_a_newer_one_kept() {
        let sink = MemorySink::new();
        let (old, new) = (RunId::generate(), RunId::generate());
        sink.append(vec![
            started(old, "chat", 10),
            started(new, "chat", 50),
            ended(old, 60),
            ended(new, 70),
        ])
        .await
        .unwrap();
        let cutoff = DateTime::from_timestamp(30, 0).unwrap();
        assert_eq!(sink.prune_runs(cutoff, 10).await.unwrap(), 1);
        assert_eq!(
            sink.read_run(old, 0, 10).await.unwrap().events,
            Vec::<Event>::new()
        );
        assert_eq!(sink.read_run(new, 0, 10).await.unwrap().events.len(), 2);
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
        assert_eq!(sink.prune_runs(cutoff, 10).await.unwrap(), 0);
    }
}
