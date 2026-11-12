//! Schedules: a cron expression and the job to enqueue when it comes due.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds};
use protocol::Actor;
use schema::{Validator, bounded};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::{Definition, DefinitionKind, Job, JobId, JobStatus, Store, StoreError};
use tokio::sync::watch;
use uuid::Uuid;

use crate::definitions::{DefinitionFile, list_definitions};
use crate::{DefinitionError, Enqueued, JobTransport};

fn one() -> u32 {
    1
}

fn hour() -> u32 {
    3600
}

fn yes() -> bool {
    true
}

/// The shortest catch-up window: twice the runtime's scheduler tick of five
/// seconds, so every slot is seen at least once.
pub const MIN_CATCH_UP_SECONDS: u32 = 10;

/// When to enqueue a job, and which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// A cron expression, with an optional seconds field first.
    #[schemars(length(min = 1, max = 128))]
    pub cron: String,
    /// The IANA time zone it's written in, such as `America/New_York`. UTC if
    /// left out.
    #[schemars(length(min = 1, max = 64))]
    pub timezone: Option<String>,
    /// The kind of job.
    #[schemars(length(min = 1, max = 64))]
    pub kind: String,
    /// The job's input, checked against what that kind accepts.
    #[schemars(extend("x-extension" = "checked against the job kind's payload schema"))]
    pub payload: Value,
    /// Higher runs first.
    #[serde(default)]
    pub priority: i32,
    /// How many times it may be tried. One by default, because a retry can
    /// spend money or repeat what the first attempt did.
    #[serde(default = "one")]
    #[schemars(range(min = 1, max = 10))]
    pub max_attempts: u32,
    /// How late a run may start after the runtime was down. An older run is
    /// skipped. At least [`MIN_CATCH_UP_SECONDS`], so a slot can't fall
    /// between two of the scheduler's looks.
    #[serde(default = "hour")]
    #[schemars(range(min = MIN_CATCH_UP_SECONDS, max = 604_800))]
    pub catch_up_seconds: u32,
    /// Whether it runs.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn parse_cron(name: &str, expression: &str) -> Result<Cron, DefinitionError> {
    CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(expression)
        .map_err(|error| DefinitionError::Cron {
            name: name.to_owned(),
            problem: bounded(&error.to_string()),
        })
}

fn parse_zone(name: &str, zone: Option<&str>) -> Result<Tz, DefinitionError> {
    zone.map_or(Ok(Tz::UTC), |zone| {
        zone.parse().map_err(|_| DefinitionError::Zone {
            name: name.to_owned(),
            zone: zone.to_owned(),
        })
    })
}

impl Schedule {
    /// A schedule from its file, checked: its shape, its cron and zone, and
    /// that the job kind exists and its payload fits. `payloads` holds each
    /// job kind's payload schema.
    ///
    /// # Errors
    ///
    /// [`DefinitionError`] naming the schedule and what's wrong with it.
    pub fn from_file(
        file: &DefinitionFile,
        payloads: &BTreeMap<String, Value>,
    ) -> Result<Self, DefinitionError> {
        let name = file.name.as_str();
        let invalid = |problem: String| DefinitionError::Invalid {
            name: name.to_owned(),
            problem: bounded(&problem),
        };
        let validator = Validator::for_type::<Self>().map_err(|e| invalid(e.to_string()))?;
        let schedule: Self = validator
            .parse(file.body.clone())
            .map_err(|refusal| invalid(format!("{}: {refusal}", file.path.display())))?;
        parse_cron(name, &schedule.cron)?;
        parse_zone(name, schedule.timezone.as_deref())?;
        let Some(payload) = payloads.get(&schedule.kind) else {
            return Err(DefinitionError::UnknownKind {
                name: name.to_owned(),
                kind: schedule.kind,
            });
        };
        Validator::from_schema(payload)
            .map_err(|error| invalid(error.to_string()))?
            .check(&schedule.payload)
            .map_err(|refusal| invalid(format!("its payload: {refusal}")))?;
        Ok(schedule)
    }

    /// The firing to make at `now`: the latest slot not past `now`, if it's
    /// within the catch-up window.
    ///
    /// # Errors
    ///
    /// [`DefinitionError`] if the expression or the zone isn't valid.
    pub fn due_slot(
        &self,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, DefinitionError> {
        let cron = parse_cron(name, &self.cron)?;
        let zone = parse_zone(name, self.timezone.as_deref())?;
        let Ok(slot) = cron.find_previous_occurrence(&now.with_timezone(&zone), true) else {
            return Ok(None);
        };
        let slot = slot.with_timezone(&Utc);
        // One kept from before the minimum existed still gets a full tick.
        let window = self.catch_up_seconds.max(MIN_CATCH_UP_SECONDS);
        let oldest = now - TimeDelta::seconds(i64::from(window));
        Ok((slot >= oldest).then_some(slot))
    }
}

/// The id of one firing. The same schedule and slot always give the same id,
/// so a restart or a second process can't fire it twice.
fn firing_id(name: &str, slot: DateTime<Utc>) -> JobId {
    let key = format!("{name}|{}", slot.to_rfc3339());
    JobId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes()))
}

/// Enqueues jobs as their schedules come due.
#[derive(Debug, Clone)]
pub struct Scheduler {
    store: Arc<dyn Store>,
    transport: Arc<dyn JobTransport>,
}

impl Scheduler {
    /// A scheduler over these schedules and this queue.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, transport: Arc<dyn JobTransport>) -> Self {
        Self { store, transport }
    }

    /// Fires whatever is due at `now`, and returns the names of what it
    /// fired. One bad schedule doesn't stop the others.
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the schedules can't be read.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<Vec<String>, StoreError> {
        let mut fired = Vec::new();
        for definition in list_definitions(self.store.as_ref(), DefinitionKind::Schedule).await? {
            let name = definition.key.name.clone();
            match self.fire(&definition, now).await {
                Ok(true) => fired.push(name),
                Ok(false) => {}
                Err(error) => eprintln!("schedule {name}: {error}"),
            }
        }
        Ok(fired)
    }

    async fn fire(
        &self,
        definition: &Definition,
        now: DateTime<Utc>,
    ) -> Result<bool, DefinitionError> {
        let name = &definition.key.name;
        let schedule: Schedule =
            serde_json::from_value(definition.body.clone()).map_err(|error| {
                DefinitionError::Invalid {
                    name: name.clone(),
                    problem: bounded(&error.to_string()),
                }
            })?;
        if !schedule.enabled {
            return Ok(false);
        }
        let Some(slot) = schedule.due_slot(name, now)? else {
            return Ok(false);
        };
        let job = Job {
            id: firing_id(name, slot),
            kind: schedule.kind,
            payload: schedule.payload,
            priority: schedule.priority,
            status: JobStatus::Queued,
            requested_by: Actor::new(format!("schedule:{name}")),
            run_at: now,
            attempts: 0,
            max_attempts: schedule.max_attempts,
            lease: None,
            last_error: None,
            result: None,
        };
        Ok(self.transport.enqueue(job).await? == Enqueued::Added)
    }

    /// Ticks every `interval` until `shutdown` turns true.
    pub async fn run(self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        while !*shutdown.borrow() {
            if let Err(error) = self.tick(Utc::now()).await {
                eprintln!("the scheduler couldn't read its schedules: {error}");
            }
            // A closed channel means nobody can ever say stop.
            let _ = tokio::time::timeout(interval, shutdown.changed()).await;
        }
    }
}
