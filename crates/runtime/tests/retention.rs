//! Retention as the server runs it: what's past its age goes, whole, and
//! nothing else does.

#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use events::{Event, EventKind, EventSink, MemorySink, RunQuery};
use model::Catalog;
use protocol::{Actor, DataClass, RunId};
use runtime::{Config, RetentionConfig, Runtime};
use serde_json::json;
use store::{Job, JobId, JobQueue, JobStatus, Store, UnitOfWork, WriteOp};
use store_memory::MemoryStore;

const MODELS: &str = "version: 1\nproviders:\n  - {name: c, base_url: 'http://127.0.0.1:9/v1', no_log: true, timeout_secs: 1}\n\
                      models:\n  - {name: c, provider: c, wire_model: x, capabilities: [chat], context_tokens: 1000, max_output_tokens: 10}\n";

fn started(run: RunId, at: DateTime<Utc>) -> Event {
    Event {
        run,
        at,
        kind: EventKind::RunStarted,
        data_class: DataClass::Private,
        body: json!({ "agent": "chat", "harness": "h" }),
    }
}

fn job(status: JobStatus, run_at: DateTime<Utc>) -> Job {
    Job {
        id: JobId::generate(),
        kind: "chat".to_owned(),
        payload: json!({}),
        priority: 0,
        status,
        requested_by: Actor::new("owner"),
        run_at,
        attempts: 0,
        max_attempts: 1,
        lease: None,
        last_error: None,
        result: None,
        finished_at: None,
    }
}

struct World {
    runtime: Runtime,
    store: Arc<MemoryStore>,
    sink: Arc<MemorySink>,
    runs: (RunId, RunId),
    jobs: (JobId, JobId, JobId),
}

/// A run and a finished job 40 days old, the same 1 day old, and a job
/// still queued from 40 days ago.
async fn world(retention: RetentionConfig, now: DateTime<Utc>) -> World {
    let store = Arc::new(MemoryStore::new(3));
    let sink = Arc::new(MemorySink::new());
    let config = Config {
        retention,
        ..Config::default()
    };
    let runtime = Runtime::assemble(
        Arc::clone(&store) as Arc<dyn Store>,
        Arc::clone(&sink) as Arc<dyn EventSink>,
        Catalog::from_yaml(MODELS, |_| None).unwrap(),
        config,
        &|_| None,
    )
    .unwrap();
    let (old, new) = (RunId::generate(), RunId::generate());
    let month = now - TimeDelta::days(40);
    let yesterday = now - TimeDelta::days(1);
    sink.append(vec![started(old, month), started(new, yesterday)])
        .await
        .unwrap();
    // Finished before finish times were kept, so they count by run_at.
    let (old_job, new_job, waiting) = (
        job(JobStatus::Succeeded, month),
        job(JobStatus::Dead, yesterday),
        job(JobStatus::Queued, month),
    );
    let jobs = (old_job.id, new_job.id, waiting.id);
    store
        .apply(
            UnitOfWork::new()
                .with(WriteOp::EnqueueJob(old_job))
                .with(WriteOp::EnqueueJob(new_job))
                .with(WriteOp::EnqueueJob(waiting)),
        )
        .await
        .unwrap();
    World {
        runtime,
        store,
        sink,
        runs: (old, new),
        jobs,
    }
}

async fn listed(sink: &MemorySink) -> Vec<RunId> {
    let page = sink
        .list_runs(&RunQuery {
            limit: 10,
            ..RunQuery::default()
        })
        .await
        .unwrap();
    page.runs.into_iter().map(|run| run.run).collect()
}

#[tokio::test]
async fn what_is_past_its_age_goes_and_nothing_else() {
    let now = Utc::now();
    let world = world(RetentionConfig::default(), now).await;
    let swept = world.runtime.sweep_retention(now).await;
    assert_eq!((swept.runs, swept.jobs), (1, 1), "{swept:?}");
    assert_eq!(swept.problems, Vec::<String>::new());
    assert_eq!(listed(&world.sink).await, [world.runs.1]);
    assert_eq!(world.store.get_job(world.jobs.0).await.unwrap(), None);
    assert!(world.store.get_job(world.jobs.1).await.unwrap().is_some());
    assert!(
        world.store.get_job(world.jobs.2).await.unwrap().is_some(),
        "a queued job is never pruned"
    );
    // A second sweep finds nothing more.
    let again = world.runtime.sweep_retention(now).await;
    assert_eq!((again.runs, again.jobs), (0, 0));
}

#[tokio::test]
async fn null_keeps_a_kind_forever() {
    let now = Utc::now();
    let forever = RetentionConfig {
        trajectory_days: None,
        job_days: None,
    };
    let world = world(forever, now).await;
    let swept = world.runtime.sweep_retention(now).await;
    assert_eq!((swept.runs, swept.jobs), (0, 0));
    assert_eq!(listed(&world.sink).await.len(), 2);
    assert!(world.store.get_job(world.jobs.0).await.unwrap().is_some());
}
