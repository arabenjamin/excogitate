//! The kinds of job the brain runs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dispatch::Outcome;
use serde_json::Value;
use store::Job;
use work::JobHandler;

use crate::Brain;
use crate::mcp::{ChatArgs, ChatReply};

/// What scheduled chats are for, which decides whether they may pay.
const SCHEDULE_PURPOSE: &str = "schedule";
/// Time past a run's own budget for it to finish and be recorded.
const MARGIN: Duration = Duration::from_secs(10);

/// Runs a chat as a job. A run that doesn't end in an answer is a failed
/// job, so it can't look as though it worked.
#[derive(Debug)]
pub(crate) struct ChatJob(pub(crate) Brain);

#[async_trait]
impl JobHandler for ChatJob {
    fn kind(&self) -> &'static str {
        "chat"
    }

    fn payload_schema(&self) -> Value {
        schema::schema_of::<ChatArgs>()
    }

    fn time_limit(&self) -> Duration {
        Duration::from_millis(self.0.config.budget.max_duration_ms).saturating_add(MARGIN)
    }

    async fn run(&self, job: &Job) -> Result<Value, String> {
        let args: ChatArgs =
            serde_json::from_value(job.payload.clone()).map_err(|error| error.to_string())?;
        let result = self
            .0
            .chat(args.message, SCHEDULE_PURPOSE)
            .await
            .map_err(|failure| failure.0)?;
        if result.receipt.outcome != Outcome::Answered {
            return Err(result
                .stopped_because
                .unwrap_or_else(|| format!("the run ended as {:?}", result.receipt.outcome)));
        }
        serde_json::to_value(ChatReply::from(result)).map_err(|error| error.to_string())
    }
}

impl Brain {
    /// Every kind of job this brain runs.
    #[must_use]
    pub fn job_handlers(&self) -> Vec<Arc<dyn JobHandler>> {
        vec![Arc::new(ChatJob(self.clone()))]
    }
}
