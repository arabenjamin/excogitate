//! Workers against the in-memory store.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use protocol::Actor;
use serde_json::{Value, json};
use store::{Job, JobId, JobQueue, JobStatus};
use store_memory::MemoryStore;
use tokio::sync::watch;
use work::{
    Enqueued, JobHandler, JobTransport, StoreTransport, Worker, WorkerError, WorkerOptions,
};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Does {
    Succeed,
    Fail,
    Panic,
    Hang,
    /// Takes most of its time limit.
    Slow,
}

#[derive(Debug)]
struct Fixture {
    kind: &'static str,
    does: Does,
    runs: AtomicUsize,
}

impl Fixture {
    fn new(does: Does) -> Arc<Self> {
        Arc::new(Self {
            kind: "test",
            does,
            runs: AtomicUsize::new(0),
        })
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl JobHandler for Fixture {
    fn kind(&self) -> &str {
        self.kind
    }

    fn payload_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "n": { "type": "integer" } }
        })
    }

    fn time_limit(&self) -> Duration {
        Duration::from_millis(200)
    }

    async fn run(&self, job: &Job) -> Result<Value, work::JobFailure> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        match self.does {
            Does::Succeed => Ok(json!({ "got": job.payload })),
            Does::Fail => Err(work::JobFailure {
                message: "it broke".to_owned(),
                result: Some(json!({ "got": "halfway" })),
            }),
            Does::Panic => panic!("boom"),
            Does::Slow => {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok(json!({}))
            }
            Does::Hang => {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(json!({}))
            }
        }
    }
}

fn quick() -> WorkerOptions {
    WorkerOptions {
        concurrency: 1,
        poll: Duration::from_millis(5),
        backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
        lease_margin: Duration::from_millis(500),
    }
}

fn job(kind: &str, payload: Value, max_attempts: u32) -> Job {
    Job {
        id: JobId::generate(),
        kind: kind.to_owned(),
        payload,
        priority: 0,
        status: JobStatus::Queued,
        requested_by: Actor::new("test"),
        run_at: Utc::now(),
        attempts: 0,
        max_attempts,
        lease: None,
        last_error: None,
        result: None,
    }
}

struct Rig {
    store: Arc<MemoryStore>,
    transport: Arc<StoreTransport>,
    stop: watch::Sender<bool>,
    running: tokio::task::JoinHandle<()>,
}

impl Rig {
    fn start(handlers: Vec<Arc<dyn JobHandler>>, options: WorkerOptions) -> Self {
        let store = Arc::new(MemoryStore::new(3));
        let transport = Arc::new(StoreTransport(Arc::clone(&store) as _));
        let worker =
            Arc::new(Worker::new(Arc::clone(&transport) as _, handlers, options, "w").unwrap());
        let (stop, shutdown) = watch::channel(false);
        let running = tokio::spawn(worker.run(shutdown));
        Self {
            store,
            transport,
            stop,
            running,
        }
    }

    async fn add(&self, job: &Job) {
        assert_eq!(
            self.transport.enqueue(job.clone()).await.unwrap(),
            Enqueued::Added
        );
    }

    /// Waits for a job to reach a final status, and returns it.
    async fn settled(&self, id: JobId) -> Job {
        for _ in 0..600 {
            let job = self.store.get_job(id).await.unwrap().unwrap();
            if matches!(job.status, JobStatus::Succeeded | JobStatus::Dead) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("job {id} never settled");
    }

    async fn stop(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.running)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_runs_and_keeps_its_result() {
    let handler = Fixture::new(Does::Succeed);
    let rig = Rig::start(vec![handler.clone()], quick());
    let queued = job("test", json!({ "n": 1 }), 1);
    rig.add(&queued).await;
    let done = rig.settled(queued.id).await;
    assert_eq!(done.status, JobStatus::Succeeded);
    assert_eq!(done.result, Some(json!({ "got": { "n": 1 } })));
    assert_eq!(handler.runs(), 1);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_job_retries_and_then_says_why_it_died() {
    let handler = Fixture::new(Does::Fail);
    let rig = Rig::start(vec![handler.clone()], quick());
    let queued = job("test", json!({}), 3);
    rig.add(&queued).await;
    let done = rig.settled(queued.id).await;
    assert_eq!(done.status, JobStatus::Dead);
    assert_eq!(done.last_error.as_deref(), Some("it broke"));
    assert_eq!(done.result, Some(json!({ "got": "halfway" })));
    assert_eq!(handler.runs(), 3);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_nothing_handles_dies_and_says_so() {
    let rig = Rig::start(vec![Fixture::new(Does::Succeed)], quick());
    let queued = job("nobody", json!({}), 1);
    rig.add(&queued).await;
    let done = rig.settled(queued.id).await;
    assert_eq!(done.status, JobStatus::Dead);
    assert!(done.last_error.unwrap().contains("nobody"));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_payload_never_reaches_the_handler() {
    let handler = Fixture::new(Does::Succeed);
    let rig = Rig::start(vec![handler.clone()], quick());
    let queued = job("test", json!({ "surprise": true }), 1);
    rig.add(&queued).await;
    let done = rig.settled(queued.id).await;
    assert_eq!(done.status, JobStatus::Dead);
    assert!(done.last_error.unwrap().contains("schema"));
    assert_eq!(handler.runs(), 0);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_job_does_not_take_the_worker_with_it() {
    let panics = Fixture::new(Does::Panic);
    let rig = Rig::start(vec![panics.clone()], quick());
    let first = job("test", json!({}), 1);
    let second = job("test", json!({}), 1);
    rig.add(&first).await;
    rig.add(&second).await;
    assert!(
        rig.settled(first.id)
            .await
            .last_error
            .unwrap()
            .contains("panicked")
    );
    assert!(
        rig.settled(second.id)
            .await
            .last_error
            .unwrap()
            .contains("panicked")
    );
    assert_eq!(panics.runs(), 2);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_job_is_stopped_at_its_time_limit() {
    let rig = Rig::start(vec![Fixture::new(Does::Hang)], quick());
    let queued = job("test", json!({}), 1);
    let started = std::time::Instant::now();
    rig.add(&queued).await;
    let done = rig.settled(queued.id).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the limit is 200ms"
    );
    assert_eq!(done.status, JobStatus::Dead);
    assert!(done.last_error.unwrap().contains("longer than"));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn many_workers_run_each_job_once() {
    let handler = Fixture::new(Does::Succeed);
    let options = WorkerOptions {
        concurrency: 8,
        ..quick()
    };
    let rig = Rig::start(vec![handler.clone()], options);
    let jobs: Vec<Job> = (0..40).map(|_| job("test", json!({}), 1)).collect();
    for queued in &jobs {
        rig.add(queued).await;
    }
    for queued in &jobs {
        assert_eq!(rig.settled(queued.id).await.status, JobStatus::Succeeded);
    }
    assert_eq!(handler.runs(), 40);
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_waits_longer_each_time() {
    let handler = Fixture::new(Does::Fail);
    let options = WorkerOptions {
        backoff: Duration::from_secs(200),
        max_backoff: Duration::from_secs(100),
        ..quick()
    };
    let rig = Rig::start(vec![handler.clone()], options);
    let queued = job("test", json!({}), 5);
    rig.add(&queued).await;
    for _ in 0..200 {
        if handler.runs() == 1
            && rig.store.get_job(queued.id).await.unwrap().unwrap().status == JobStatus::Queued
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let waiting = rig.store.get_job(queued.id).await.unwrap().unwrap();
    let wait = waiting.run_at - Utc::now();
    assert!((95..=100).contains(&wait.num_seconds()), "{wait}");
    assert_eq!(handler.runs(), 1, "it waits rather than running again");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn after_stop_nothing_new_is_claimed() {
    let handler = Fixture::new(Does::Succeed);
    let rig = Rig::start(vec![handler.clone()], quick());
    rig.stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut { rig.running })
        .await
        .unwrap()
        .unwrap();
    rig.transport
        .enqueue(job("test", json!({}), 1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(handler.runs(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_job_keeps_its_lease_until_it_finishes() {
    let handler = Fixture::new(Does::Slow);
    let options = WorkerOptions {
        concurrency: 4,
        lease_margin: Duration::from_millis(100),
        ..quick()
    };
    let rig = Rig::start(vec![handler.clone()], options);
    let queued = job("test", json!({}), 3);
    rig.add(&queued).await;
    assert_eq!(rig.settled(queued.id).await.status, JobStatus::Succeeded);
    assert_eq!(handler.runs(), 1, "nobody else may claim it meanwhile");
    rig.stop().await;
}

#[tokio::test]
async fn enqueueing_the_same_job_twice_changes_nothing() {
    let transport = StoreTransport(Arc::new(MemoryStore::new(3)));
    let queued = job("test", json!({}), 1);
    assert_eq!(
        transport.enqueue(queued.clone()).await.unwrap(),
        Enqueued::Added
    );
    assert_eq!(
        transport.enqueue(queued).await.unwrap(),
        Enqueued::AlreadyThere
    );
}

#[test]
fn one_kind_has_one_handler() {
    let store = Arc::new(MemoryStore::new(3));
    let transport = Arc::new(StoreTransport(store as _));
    let error = Worker::new(
        transport,
        vec![Fixture::new(Does::Succeed), Fixture::new(Does::Fail)],
        quick(),
        "w",
    )
    .unwrap_err();
    assert_eq!(error, WorkerError::DuplicateKind("test".to_owned()));
}
