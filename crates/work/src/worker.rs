//! Workers: claim a job, run it within its time limit, record what happened.
//!
//! A job can't outlive its lease, because the handler is stopped at its time
//! limit and the lease is longer than any limit. So a lease that lapses means
//! the worker died, and the job is safe to run again.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use schema::{Validator, bounded};
use serde_json::Value;
use store::Job;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::JobTransport;

/// Runs one kind of job.
#[async_trait]
pub trait JobHandler: Send + Sync + std::fmt::Debug {
    /// The kind it runs.
    fn kind(&self) -> &str;

    /// The schema a job's payload must meet.
    fn payload_schema(&self) -> Value;

    /// The longest one job may take.
    fn time_limit(&self) -> Duration;

    /// Does the work. The payload has already met the schema.
    ///
    /// # Errors
    ///
    /// Why it failed, which is kept on the job.
    async fn run(&self, job: &Job) -> Result<Value, String>;
}

/// How workers behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerOptions {
    /// How many jobs run at once.
    pub concurrency: usize,
    /// How long to wait when there's nothing to do.
    pub poll: Duration,
    /// The wait before the first retry. It doubles each time.
    pub backoff: Duration,
    /// The longest wait before a retry.
    pub max_backoff: Duration,
    /// How much longer than the longest time limit a lease lasts.
    pub lease_margin: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            concurrency: 1,
            poll: Duration::from_secs(1),
            backoff: Duration::from_secs(30),
            max_backoff: Duration::from_secs(3600),
            lease_margin: Duration::from_secs(30),
        }
    }
}

/// Workers couldn't be set up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WorkerError {
    /// Two handlers claimed one kind.
    #[error("two handlers for job kind {0}")]
    DuplicateKind(String),
    /// A handler's payload schema isn't valid.
    #[error("job kind {kind}: {problem}")]
    BadSchema {
        /// The kind.
        kind: String,
        /// What's wrong.
        problem: String,
    },
}

#[derive(Debug)]
struct Registered {
    handler: Arc<dyn JobHandler>,
    payload: Validator,
}

/// Claims and runs jobs until told to stop.
#[derive(Debug)]
pub struct Worker {
    transport: Arc<dyn JobTransport>,
    handlers: HashMap<String, Registered>,
    options: WorkerOptions,
    lease: Duration,
    name: String,
}

impl Worker {
    /// A worker for these kinds of job. `name` tells it apart from other
    /// processes on the same queue.
    ///
    /// # Errors
    ///
    /// [`WorkerError`] if a kind is claimed twice or a schema isn't valid.
    pub fn new(
        transport: Arc<dyn JobTransport>,
        handlers: Vec<Arc<dyn JobHandler>>,
        options: WorkerOptions,
        name: impl Into<String>,
    ) -> Result<Self, WorkerError> {
        let longest = handlers
            .iter()
            .map(|handler| handler.time_limit())
            .max()
            .unwrap_or_default();
        let mut registered = HashMap::new();
        for handler in handlers {
            let kind = handler.kind().to_owned();
            let payload = Validator::from_schema(&handler.payload_schema()).map_err(|error| {
                WorkerError::BadSchema {
                    kind: kind.clone(),
                    problem: bounded(&error.to_string()),
                }
            })?;
            let entry = Registered { handler, payload };
            if registered.insert(kind.clone(), entry).is_some() {
                return Err(WorkerError::DuplicateKind(kind));
            }
        }
        Ok(Self {
            transport,
            handlers: registered,
            options,
            lease: longest.saturating_add(options.lease_margin),
            name: name.into(),
        })
    }

    /// Runs until `shutdown` turns true. Jobs under way finish first, or are
    /// picked up again once their lease lapses if the process is stopped.
    pub async fn run(self: Arc<Self>, shutdown: watch::Receiver<bool>) {
        let mut slots = Vec::with_capacity(self.options.concurrency);
        for slot in 0..self.options.concurrency.max(1) {
            let worker = Arc::clone(&self);
            let shutdown = shutdown.clone();
            slots.push(tokio::spawn(async move {
                worker.work(slot, shutdown).await;
            }));
        }
        for slot in slots {
            // A slot only ends by shutting down or panicking, and either way
            // there's nothing left for it to do.
            let _ = slot.await;
        }
    }

    async fn work(&self, slot: usize, mut shutdown: watch::Receiver<bool>) {
        let name = format!("{}-{slot}", self.name);
        while !*shutdown.borrow() {
            match self.claim_next(&name).await {
                Some(job) => self.process(&name, job).await,
                None => {
                    // A closed channel means nobody can ever say stop.
                    let _ = timeout(self.options.poll, shutdown.changed()).await;
                }
            }
        }
    }

    async fn claim_next(&self, name: &str) -> Option<Job> {
        let now = Utc::now();
        let lease = TimeDelta::from_std(self.lease).unwrap_or(TimeDelta::MAX);
        let until = now.checked_add_signed(lease)?;
        match self.transport.claim(name, now, until).await {
            Ok(job) => job,
            Err(error) => {
                eprintln!("couldn't claim a job: {error}");
                None
            }
        }
    }

    async fn process(&self, name: &str, job: Job) {
        match self.run_job(&job).await {
            Ok(result) => match self.transport.complete(job.id, name, result).await {
                Ok(()) => eprintln!("job {} ({}) succeeded", job.id, job.kind),
                Err(error) => eprintln!("job {} finished but wasn't recorded: {error}", job.id),
            },
            Err(reason) => {
                eprintln!(
                    "job {} ({}) failed on attempt {} of {}: {}",
                    job.id,
                    job.kind,
                    job.attempts,
                    job.max_attempts,
                    bounded(&reason)
                );
                self.record_failure(name, &job, &reason).await;
            }
        }
    }

    async fn run_job(&self, job: &Job) -> Result<Value, String> {
        let Some(registered) = self.handlers.get(&job.kind) else {
            return Err(format!("no handler for job kind {}", job.kind));
        };
        registered
            .payload
            .check(&job.payload)
            .map_err(|refusal| format!("the payload doesn't meet its schema: {refusal}"))?;
        let handler = Arc::clone(&registered.handler);
        let limit = handler.time_limit();
        let owned = job.clone();
        // A task of its own, so a panicking handler can't take the worker.
        let task = tokio::spawn(async move { handler.run(&owned).await });
        let abort = task.abort_handle();
        match timeout(limit, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the job panicked".to_owned()),
            Err(_) => {
                abort.abort();
                Err(format!("the job took longer than {}s", limit.as_secs()))
            }
        }
    }

    /// The wait before attempt `attempts + 1`.
    fn backoff(&self, attempts: u32) -> Duration {
        let doubled = 2_u32.saturating_pow(attempts.saturating_sub(1));
        self.options
            .backoff
            .saturating_mul(doubled)
            .min(self.options.max_backoff)
    }

    async fn record_failure(&self, name: &str, job: &Job, reason: &str) {
        let wait = TimeDelta::from_std(self.backoff(job.attempts)).unwrap_or(TimeDelta::MAX);
        let now = Utc::now();
        let retry_at = now.checked_add_signed(wait).unwrap_or(now);
        if let Err(error) = self
            .transport
            .fail(job.id, name, &bounded(reason), retry_at)
            .await
        {
            eprintln!("job {} failed but wasn't recorded: {error}", job.id);
        }
    }
}
