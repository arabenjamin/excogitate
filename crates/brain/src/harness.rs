//! Harness² in the brain: attempts run as jobs, and are kept as records.

use std::time::Duration;

use async_trait::async_trait;
use harness::{ATTEMPT_TYPE, Attempt, Judges, Loop, Request, Settings, Status};
use serde_json::{Value, json};
use store::{
    FieldFilter, FieldOp, FieldValue, Page, PageRequest, RecordId, RecordQuery, StoreError,
    UnitOfWork, WriteOp,
};
use work::{JobFailure, JobHandler};

use crate::Brain;

/// The kind of job that runs an attempt.
pub(crate) const HARNESS_KIND: &str = "harness";

/// Runs one attempt to improve an agent, and keeps it. An attempt that
/// couldn't finish is a failed job, with the attempt as its result.
#[derive(Debug)]
pub(crate) struct HarnessJob(pub(crate) Brain);

impl Brain {
    /// Makes sure the store knows Harness²'s record type.
    pub(crate) async fn harness_records(&self) -> Result<(), StoreError> {
        self.workflows
            .store
            .register_records(&harness::declarations())
            .await
    }

    /// Runs and keeps an attempt. Returns its id and the attempt.
    pub(crate) async fn improve(&self, request: Request) -> Result<(RecordId, Attempt), String> {
        self.harness_records()
            .await
            .map_err(|error| error.to_string())?;
        let config = &self.config.harness;
        let settings = Settings {
            reads: config.probe_reads.iter().cloned().collect(),
            judge_usd_micros: config.judge_usd_micros,
        };
        let looped = Loop {
            engine: &self.workflows,
            judges: Judges {
                gateway: &self.gateway,
                max_output_tokens: config.judge_output_tokens,
            },
            settings: &settings,
        };
        let attempt = looped.improve(request).await;
        let id = RecordId::generate();
        let record = harness::record_of(id, &attempt).map_err(|error| error.to_string())?;
        self.workflows
            .store
            .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
            .await
            .map_err(|error| error.to_string())?;
        Ok((id, attempt))
    }

    /// Kept attempts, by id, optionally only those that ended `status`.
    pub(crate) async fn attempts(
        &self,
        status: Option<&str>,
        page: PageRequest,
    ) -> Result<Page<(RecordId, Value)>, StoreError> {
        self.harness_records().await?;
        let filters = status
            .map(|status| FieldFilter {
                field: "status".to_owned(),
                op: FieldOp::Eq,
                value: FieldValue::Text(status.to_owned()),
            })
            .into_iter()
            .collect();
        let found = self
            .workflows
            .store
            .query_records(RecordQuery {
                record_type: ATTEMPT_TYPE.to_owned(),
                filters,
                page,
            })
            .await?;
        Ok(Page {
            items: found
                .items
                .into_iter()
                .map(|record| (record.id, record.data))
                .collect(),
            has_more: found.has_more,
        })
    }

    /// One kept attempt.
    pub(crate) async fn attempt(&self, id: RecordId) -> Result<Option<Value>, StoreError> {
        self.harness_records().await?;
        Ok(self
            .workflows
            .store
            .get_record(ATTEMPT_TYPE, id)
            .await?
            .map(|record| record.data))
    }
}

#[async_trait]
impl JobHandler for HarnessJob {
    fn kind(&self) -> &'static str {
        HARNESS_KIND
    }

    fn payload_schema(&self) -> Value {
        schema::schema_of::<Request>()
    }

    fn time_limit(&self) -> Duration {
        Duration::from_secs(u64::from(self.0.config.harness.time_limit_seconds))
    }

    async fn run(&self, job: &store::Job) -> Result<Value, JobFailure> {
        let request: Request =
            serde_json::from_value(job.payload.clone()).map_err(|error| error.to_string())?;
        let (id, attempt) = self.0.improve(request).await?;
        let result = json!({ "attempt_id": id.to_string(), "attempt": attempt });
        if attempt.status == Status::Failed {
            return Err(JobFailure {
                message: attempt
                    .problems
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "the attempt failed".to_owned()),
                result: Some(result),
            });
        }
        Ok(result)
    }
}
