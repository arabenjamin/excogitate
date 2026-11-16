//! The owner's feedback on runs: kept as Harness² records, one per run, and
//! read back by the picker.

use chrono::{DateTime, Utc};
use harness::{FEEDBACK_TYPE, Feedback, FeedbackRequest, Mark, Marked, feedback_id};
use store::{
    FieldFilter, FieldOp, FieldValue, PageRequest, RecordQuery, StoreError, UnitOfWork, WriteOp,
};

use super::ChangeError;
use crate::Runtime;

/// The most feedback the picker reads at once: more than a week of runs.
const MAX_READ: u32 = 1000;

impl Runtime {
    /// Keeps the owner's feedback on `run`, whose subject is `agent`,
    /// replacing any it had.
    pub(crate) async fn set_feedback(
        &self,
        run: &str,
        agent: &str,
        request: FeedbackRequest,
    ) -> Result<Feedback, ChangeError> {
        self.harness_records().await?;
        let feedback = Feedback {
            run: run.to_owned(),
            agent: agent.to_owned(),
            mark: request.mark,
            note: request.note,
            at: Utc::now(),
        };
        let record = harness::record(FEEDBACK_TYPE, feedback_id(run), &feedback, feedback.at)?;
        self.workflows
            .store
            .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
            .await?;
        Ok(feedback)
    }

    /// The runs the owner marked bad since `since`, with their notes.
    pub(crate) async fn marked_bad(&self, since: DateTime<Utc>) -> Result<Marked, StoreError> {
        self.harness_records().await?;
        let found = self
            .workflows
            .store
            .query_records(
                RecordQuery::new(FEEDBACK_TYPE.to_owned(), PageRequest::new(MAX_READ, 0)?)
                    .with_filters(vec![FieldFilter::new(
                        "at".to_owned(),
                        FieldOp::Gte,
                        FieldValue::Timestamp(since),
                    )]),
            )
            .await?;
        Ok(found
            .items
            .into_iter()
            .filter_map(|record| serde_json::from_value::<Feedback>(record.data).ok())
            .filter(|feedback| feedback.mark == Mark::Bad)
            .map(|feedback| (feedback.run, feedback.note))
            .collect())
    }
}
