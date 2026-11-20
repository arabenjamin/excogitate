//! The daily review of answered chats, as a job that can be scheduled: a
//! judge reads each of the last day's answered chats, and a checked fault
//! becomes evidence for the picker.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Duration as Span, Utc};
use dispatch::read_trajectory;
use events::RunQuery;
use harness::{
    Judges, REVIEW_TYPE, REVIEWED, Rendered, Review, Reviewed, checked_review, review_id,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use store::{
    FieldFilter, FieldOp, FieldValue, Job, PageRequest, RecordQuery, StoreError, UnitOfWork,
    WriteOp,
};
use work::{JobFailure, JobHandler};

use crate::Runtime;

/// The kind of job that reviews answered chats.
pub(crate) const REVIEW_KIND: &str = "harness_review";
/// How far back a chat is reviewed.
const REVIEW_HOURS: i64 = 24;
/// The most chats looked at for one review job.
const MAX_LOOKED_AT: usize = 200;
/// The most steps read from one chat.
const MAX_STEPS: usize = 512;
/// The longest a review job may take.
const TIME_LIMIT: Duration = Duration::from_mins(30);
/// The most reviews the picker reads at once: a week of them at up to 140 a
/// day.
const MAX_READ: u32 = 1000;

/// What a review job is given: nothing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewArgs {}

/// What a review job did.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ReviewResult {
    /// The chats it reviewed, each as it's kept.
    #[schemars(length(max = 200))]
    pub reviewed: Vec<Review>,
    /// What it spent, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// What it left alone, and why.
    #[schemars(length(max = 200), inner(length(max = 2048)))]
    pub skipped: Vec<String>,
}

impl Runtime {
    /// Reviews up to `harness.review_chats` of the last day's answered chats
    /// not yet reviewed, newest first, within `harness.review_usd_micros`.
    pub(crate) async fn review_chats(&self) -> Result<ReviewResult, String> {
        self.harness_records()
            .await
            .map_err(|error| error.to_string())?;
        let config = &self.config.harness;
        let mut result = ReviewResult {
            reviewed: Vec::new(),
            cost_usd_micros: 0,
            skipped: Vec::new(),
        };
        let most = usize::try_from(config.review_chats).unwrap_or(0);
        if most == 0 || config.review_usd_micros == 0 {
            result.skipped.push(
                "harness.review_chats and harness.review_usd_micros must both be set for a \
                 review"
                    .to_owned(),
            );
            return Ok(result);
        }
        let writer = self
            .current_line()
            .await
            .map_err(|failure| failure.to_string())?
            .writer
            .0;
        let since = Utc::now() - Span::hours(REVIEW_HOURS);
        let listed = self
            .sink
            .list_runs(&RunQuery {
                agent: Some("chat".to_owned()),
                limit: MAX_LOOKED_AT,
                ..RunQuery::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        let judges = Judges {
            gateway: &self.gateway,
            max_output_tokens: config.judge_output_tokens,
        };
        for summary in listed
            .runs
            .iter()
            .filter(|run| run.started_at >= since && run.outcome.as_deref() == Some("answered"))
        {
            if result.reviewed.len() >= most {
                break;
            }
            let run = summary.run.to_string();
            if self
                .workflows
                .store
                .get_record(REVIEW_TYPE, review_id(&run))
                .await
                .map_err(|error| error.to_string())?
                .is_some()
            {
                continue;
            }
            let left = config
                .review_usd_micros
                .saturating_sub(result.cost_usd_micros);
            if left == 0 {
                result
                    .skipped
                    .push("the review's money ran out; the rest wait for the next".to_owned());
                break;
            }
            let Some(chat) = read_trajectory(self.sink.as_ref(), summary.run, MAX_STEPS)
                .await
                .map_err(|error| error.to_string())?
            else {
                continue;
            };
            let rendered = Rendered::new(REVIEWED, &chat);
            let reply = match judges.review(&rendered, left).await {
                Ok(reply) => reply,
                Err(error) => {
                    result.cost_usd_micros = result
                        .cost_usd_micros
                        .saturating_add(error.cost_usd_micros());
                    result.skipped.push(format!("{run}: the reviewer: {error}"));
                    continue;
                }
            };
            let review = checked_review(&chat, &rendered, &writer, reply, Utc::now());
            result.cost_usd_micros = result
                .cost_usd_micros
                .saturating_add(review.cost_usd_micros);
            let record = harness::record(REVIEW_TYPE, review_id(&run), &review, review.at)
                .map_err(|error| error.to_string())?;
            self.workflows
                .store
                .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
                .await
                .map_err(|error| error.to_string())?;
            result.reviewed.push(review);
        }
        Ok(result)
    }

    /// The checked faults reviews found since `since`, by chat.
    pub(crate) async fn reviewed_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Reviewed, StoreError> {
        self.harness_records().await?;
        let found = self
            .workflows
            .store
            .query_records(
                RecordQuery::new(REVIEW_TYPE.to_owned(), PageRequest::new(MAX_READ, 0)?)
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
            .filter_map(|record| serde_json::from_value::<Review>(record.data).ok())
            .filter_map(|review| review.finding().map(|found| (review.run, found)))
            .collect())
    }
}

/// Reviews answered chats, as a job.
#[derive(Debug)]
pub(crate) struct ReviewJob(pub(crate) Runtime);

#[async_trait]
impl JobHandler for ReviewJob {
    fn kind(&self) -> &'static str {
        REVIEW_KIND
    }

    fn payload_schema(&self) -> Value {
        schema::schema_of::<ReviewArgs>()
    }

    fn time_limit(&self) -> Duration {
        TIME_LIMIT
    }

    async fn run(&self, job: &Job) -> Result<Value, JobFailure> {
        let _: ReviewArgs =
            serde_json::from_value(job.payload.clone()).map_err(|error| error.to_string())?;
        let reviewed = self.0.review_chats().await?;
        Ok(json!(reviewed))
    }
}
