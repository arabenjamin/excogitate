//! How jobs move. Only the store does this today; the port is here so that
//! another queue can replace it without touching the workers.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use store::{Job, JobId, Store, StoreError, WriteOp};

/// What became of an enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// The job is queued.
    Added,
    /// A job with that id already exists, so nothing changed.
    AlreadyThere,
}

/// Moves jobs between whoever asks for work and the workers.
#[async_trait]
pub trait JobTransport: Send + Sync + std::fmt::Debug {
    /// Queues a job.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if it can't be queued.
    async fn enqueue(&self, job: Job) -> Result<Enqueued, StoreError>;

    /// Claims the next runnable job, if there is one.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the claim fails.
    async fn claim(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError>;

    /// Marks a claimed job finished, with its result.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] if `worker` no longer holds it.
    async fn complete(&self, id: JobId, worker: &str, result: Value) -> Result<(), StoreError>;

    /// Records a failed attempt, and what it left behind. It runs again at `retry_at`, or is dead if
    /// it's out of attempts.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] if `worker` no longer holds it.
    async fn fail(
        &self,
        id: JobId,
        worker: &str,
        error: &str,
        result: Option<Value>,
        retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;
}

/// The default transport: jobs live in the store.
#[derive(Debug, Clone)]
pub struct StoreTransport(pub Arc<dyn Store>);

#[async_trait]
impl JobTransport for StoreTransport {
    async fn enqueue(&self, job: Job) -> Result<Enqueued, StoreError> {
        match self.0.apply(WriteOp::EnqueueJob(job).into()).await {
            Ok(()) => Ok(Enqueued::Added),
            Err(StoreError::Conflict(_)) => Ok(Enqueued::AlreadyThere),
            Err(error) => Err(error),
        }
    }

    async fn claim(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        self.0.claim_job(worker, now, lease_until).await
    }

    async fn complete(&self, id: JobId, worker: &str, result: Value) -> Result<(), StoreError> {
        self.0.complete_job(id, worker, result).await
    }

    async fn fail(
        &self,
        id: JobId,
        worker: &str,
        error: &str,
        result: Option<Value>,
        retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.0.fail_job(id, worker, error, result, retry_at).await
    }
}
