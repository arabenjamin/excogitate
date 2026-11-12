//! The job queue and the spend log. Each change takes a lock first, so two
//! claims or two reservations made at once are made one after the other.
//! Neo4j reads what's committed, so a check made after the lock sees the
//! write of whoever held it before.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use neo4rs::{Txn, query};
use serde_json::Value;
use store::{Job, JobId, JobQueue, JobStatus, Page, PageRequest, SpendLog, StoreError};

use crate::codec::{self, JOB, at, get, job_status_name, json_text, maybe_at};
use crate::setup::JOBS_LOCK;
use crate::{Neo4jStore, finish, page_clause, page_of, retrying, txn_rows};

/// Takes the queue's lock for the rest of the transaction. `MERGE`, so a
/// lock node that went missing is made again rather than silently not taken.
async fn lock_queue(txn: &mut Txn) -> Result<(), StoreError> {
    txn.run(
        query("MERGE (l:Lock {name: $name}) SET l.held = true REMOVE l.held")
            .param("name", JOBS_LOCK),
    )
    .await
    .map_err(codec::from_neo)
}

/// The job, read under the queue's lock, if `worker` holds it.
async fn held_job(txn: &mut Txn, id: JobId, worker: &str) -> Result<Job, StoreError> {
    lock_queue(txn).await?;
    let rows = txn_rows(
        txn,
        query(&format!("MATCH (j:Job {{id: $id}}) RETURN {JOB}")).param("id", id.to_string()),
    )
    .await?;
    let job = rows
        .first()
        .map(codec::job)
        .transpose()?
        .ok_or_else(|| StoreError::NotFound(format!("job {id}")))?;
    if job.status != JobStatus::Claimed
        || job
            .lease
            .as_ref()
            .is_none_or(|lease| lease.worker != worker)
    {
        return Err(StoreError::Conflict(format!(
            "job {id} isn't held by {worker}"
        )));
    }
    Ok(job)
}

async fn claim_in(
    txn: &mut Txn,
    worker: &str,
    kinds: &[String],
    now: DateTime<Utc>,
    lease_until: DateTime<Utc>,
) -> Result<Option<Job>, StoreError> {
    lock_queue(txn).await?;
    // A lease that lapsed on the last attempt has nothing left to retry.
    txn.run(
        query(
            "MATCH (j:Job {status: 'claimed'}) WHERE j.lease_until <= $now AND j.attempts >= j.max_attempts \
             SET j.status = 'dead', j.lease_worker = null, j.lease_until = null, \
             j.last_error = 'lease lapsed on the last attempt', j.finished_at = $now",
        )
        .param("now", at(now)),
    )
    .await
    .map_err(codec::from_neo)?;
    let rows = txn_rows(
        txn,
        query(&format!(
            "MATCH (j:Job) WHERE ((j.status = 'queued' AND j.run_at <= $now) \
               OR (j.status = 'claimed' AND j.lease_until <= $now)) AND j.kind IN $kinds \
             WITH j ORDER BY j.priority DESC, j.run_at, j.id LIMIT 1 \
             SET j.status = 'claimed', j.attempts = j.attempts + 1, \
               j.lease_worker = $worker, j.lease_until = $until \
             RETURN {JOB}"
        ))
        .param("now", at(now))
        .param("kinds", kinds.to_vec())
        .param("worker", worker)
        .param("until", at(lease_until)),
    )
    .await?;
    rows.first().map(codec::job).transpose()
}

#[async_trait]
impl JobQueue for Neo4jStore {
    async fn get_job(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        let rows = self
            .rows(
                query(&format!("MATCH (j:Job {{id: $id}}) RETURN {JOB}"))
                    .param("id", id.to_string()),
            )
            .await?;
        rows.first().map(codec::job).transpose()
    }

    async fn list_jobs(
        &self,
        status: Option<JobStatus>,
        page: PageRequest,
    ) -> Result<Page<Job>, StoreError> {
        let filter = if status.is_some() {
            "WHERE j.status = $status"
        } else {
            ""
        };
        let mut query = query(&format!(
            "MATCH (j:Job) {filter} RETURN {JOB} ORDER BY j.run_at DESC, j.id {}",
            page_clause(page)
        ));
        if let Some(status) = status {
            query = query.param("status", job_status_name(status));
        }
        let rows = self.rows(query).await?;
        page_of(&rows, page, codec::job)
    }

    async fn claim_job(
        &self,
        worker: &str,
        kinds: &[String],
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        if lease_until <= now {
            return Err(StoreError::Invalid("a lease must end after now".to_owned()));
        }
        retrying!({
            let mut txn = self.begin().await?;
            let outcome = claim_in(&mut txn, worker, kinds, now, lease_until).await;
            finish(txn, outcome).await
        })
    }

    async fn complete_job(&self, id: JobId, worker: &str, result: Value) -> Result<(), StoreError> {
        retrying!({
            let mut txn = self.begin().await?;
            let outcome = complete_in(&mut txn, id, worker, &result).await;
            finish(txn, outcome).await
        })
    }

    async fn fail_job(
        &self,
        id: JobId,
        worker: &str,
        error: &str,
        result: Option<Value>,
        retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        retrying!({
            let mut txn = self.begin().await?;
            let outcome = fail_in(&mut txn, id, worker, error, result.as_ref(), retry_at).await;
            finish(txn, outcome).await
        })
    }

    async fn prune_jobs(&self, before: DateTime<Utc>, limit: u32) -> Result<u32, StoreError> {
        let rows = self
            .rows(
                query(
                    "MATCH (j:Job) WHERE j.status IN ['succeeded', 'dead'] \
                       AND coalesce(j.finished_at, j.run_at) < $before \
                     WITH j ORDER BY coalesce(j.finished_at, j.run_at), j.id LIMIT $limit \
                     WITH collect(j) AS gone \
                     FOREACH (j IN gone | DETACH DELETE j) \
                     RETURN size(gone) AS pruned",
                )
                .param("before", at(before))
                .param("limit", i64::from(limit)),
            )
            .await?;
        let pruned: i64 = rows.first().map_or(Ok(0), |row| get(row, "pruned"))?;
        Ok(u32::try_from(pruned).unwrap_or(u32::MAX))
    }
}

async fn complete_in(
    txn: &mut Txn,
    id: JobId,
    worker: &str,
    result: &Value,
) -> Result<(), StoreError> {
    held_job(txn, id, worker).await?;
    txn.run(
        query(
            "MATCH (j:Job {id: $id}) SET j.status = 'succeeded', j.result = $result, \
             j.lease_worker = null, j.lease_until = null, j.finished_at = $now",
        )
        .param("id", id.to_string())
        .param("result", json_text(result))
        .param("now", at(Utc::now())),
    )
    .await
    .map_err(codec::from_neo)
}

async fn fail_in(
    txn: &mut Txn,
    id: JobId,
    worker: &str,
    error: &str,
    result: Option<&Value>,
    retry_at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let job = held_job(txn, id, worker).await?;
    let status = if job.attempts >= job.max_attempts {
        JobStatus::Dead
    } else {
        JobStatus::Queued
    };
    let run_at = if status == JobStatus::Queued {
        retry_at
    } else {
        job.run_at
    };
    txn.run(
        query(
            "MATCH (j:Job {id: $id}) SET j.status = $status, j.run_at = $run_at, \
             j.last_error = $error, j.result = $result, j.lease_worker = null, j.lease_until = null, \
             j.finished_at = $finished",
        )
        .param("id", id.to_string())
        .param("status", job_status_name(status))
        .param("run_at", at(run_at))
        .param(
            "finished",
            maybe_at((status == JobStatus::Dead).then(Utc::now)),
        )
        .param("error", error)
        .param("result", result.map(json_text)),
    )
    .await
    .map_err(codec::from_neo)
}

// ---- spend ----

/// Neo4j integers are signed 64-bit, and a day's total may be any `u64`, so
/// it's stored as decimal text and only ever changed under the day's lock.
async fn locked_spend(txn: &mut Txn, provider: &str, day: NaiveDate) -> Result<u64, StoreError> {
    let rows = txn_rows(
        txn,
        query(
            "MERGE (s:Spend {provider: $provider, day: $day}) ON CREATE SET s.amount = '0' \
             SET s.held = true REMOVE s.held RETURN s.amount AS amount",
        )
        .param("provider", provider)
        .param("day", day),
    )
    .await?;
    let text: String = rows
        .first()
        .map_or_else(|| Ok("0".to_owned()), |row| get(row, "amount"))?;
    text.parse()
        .map_err(|_| StoreError::Backend(format!("stored spend {text} isn't a number")))
}

async fn set_spend(
    txn: &mut Txn,
    provider: &str,
    day: NaiveDate,
    amount: u64,
) -> Result<(), StoreError> {
    txn.run(
        query("MATCH (s:Spend {provider: $provider, day: $day}) SET s.amount = $amount")
            .param("provider", provider)
            .param("day", day)
            .param("amount", amount.to_string()),
    )
    .await
    .map_err(codec::from_neo)
}

async fn reserve_in(
    txn: &mut Txn,
    provider: &str,
    day: NaiveDate,
    amount: u64,
    cap: u64,
) -> Result<bool, StoreError> {
    let spent = locked_spend(txn, provider, day).await?;
    match spent.checked_add(amount) {
        Some(total) if total <= cap => {
            set_spend(txn, provider, day, total).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

async fn settle_in(
    txn: &mut Txn,
    provider: &str,
    day: NaiveDate,
    reserved: u64,
    actual: u64,
) -> Result<(), StoreError> {
    let spent = locked_spend(txn, provider, day).await?;
    let total = spent.saturating_sub(reserved).saturating_add(actual);
    set_spend(txn, provider, day, total).await
}

#[async_trait]
impl SpendLog for Neo4jStore {
    async fn reserve_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        amount: u64,
        cap: u64,
    ) -> Result<bool, StoreError> {
        retrying!({
            let mut txn = self.begin().await?;
            let outcome = reserve_in(&mut txn, provider, day, amount, cap).await;
            finish(txn, outcome).await
        })
    }

    async fn settle_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        reserved: u64,
        actual: u64,
    ) -> Result<(), StoreError> {
        retrying!({
            let mut txn = self.begin().await?;
            let outcome = settle_in(&mut txn, provider, day, reserved, actual).await;
            finish(txn, outcome).await
        })
    }

    async fn spent_on(&self, provider: &str, day: NaiveDate) -> Result<u64, StoreError> {
        let rows = self
            .rows(
                query("MATCH (s:Spend {provider: $provider, day: $day}) RETURN s.amount AS amount")
                    .param("provider", provider)
                    .param("day", day),
            )
            .await?;
        let Some(row) = rows.first() else {
            return Ok(0);
        };
        let text: String = get(row, "amount")?;
        text.parse()
            .map_err(|_| StoreError::Backend(format!("stored spend {text} isn't a number")))
    }
}
