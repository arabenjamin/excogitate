//! Schedules: a cron expression and the job to enqueue when it comes due.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
use store::{
    Definition, DefinitionKey, DefinitionKind, Job, JobId, JobStatus, PageRequest, Store,
    StoreError, UnitOfWork, WriteOp,
};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{Enqueued, JobTransport};

/// Marks a definition as coming from a file, so it can be removed with it.
const FILE_SOURCE: &str = "file:";
const PAGE_SIZE: u32 = 200;

fn one() -> u32 {
    1
}

fn hour() -> u32 {
    3600
}

fn yes() -> bool {
    true
}

/// When to enqueue a job, and which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// A cron expression, with an optional seconds field first.
    #[schemars(length(min = 1, max = 128))]
    pub cron: String,
    /// The IANA time zone it's written in, such as `America/Detroit`. UTC if
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
    /// How late a run may start after the brain was down. An older run is
    /// skipped.
    #[serde(default = "hour")]
    #[schemars(range(max = 604_800))]
    pub catch_up_seconds: u32,
    /// Whether it runs.
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// A schedule that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScheduleError {
    /// The cron expression isn't valid.
    #[error("{name}: not a valid cron expression: {problem}")]
    Cron {
        /// The schedule.
        name: String,
        /// What's wrong.
        problem: String,
    },
    /// The time zone isn't known.
    #[error("{name}: unknown time zone {zone}")]
    Zone {
        /// The schedule.
        name: String,
        /// The zone.
        zone: String,
    },
    /// It names a kind of job no handler runs.
    #[error("{name}: no handler for job kind {kind}")]
    UnknownKind {
        /// The schedule.
        name: String,
        /// The kind.
        kind: String,
    },
    /// The file or the payload doesn't meet its schema.
    #[error("{name}: {problem}")]
    Invalid {
        /// The schedule.
        name: String,
        /// What's wrong.
        problem: String,
    },
    /// A file couldn't be read.
    #[error("{0}")]
    File(String),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn parse_cron(name: &str, expression: &str) -> Result<Cron, ScheduleError> {
    CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(expression)
        .map_err(|error| ScheduleError::Cron {
            name: name.to_owned(),
            problem: bounded(&error.to_string()),
        })
}

fn parse_zone(name: &str, zone: Option<&str>) -> Result<Tz, ScheduleError> {
    zone.map_or(Ok(Tz::UTC), |zone| {
        zone.parse().map_err(|_| ScheduleError::Zone {
            name: name.to_owned(),
            zone: zone.to_owned(),
        })
    })
}

impl Schedule {
    /// The firing to make at `now`: the latest slot not past `now`, if it's
    /// within the catch-up window.
    ///
    /// # Errors
    ///
    /// [`ScheduleError`] if the expression or the zone isn't valid.
    pub fn due_slot(
        &self,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ScheduleError> {
        let cron = parse_cron(name, &self.cron)?;
        let zone = parse_zone(name, self.timezone.as_deref())?;
        let Ok(slot) = cron.find_previous_occurrence(&now.with_timezone(&zone), true) else {
            return Ok(None);
        };
        let slot = slot.with_timezone(&Utc);
        let oldest = now - TimeDelta::seconds(i64::from(self.catch_up_seconds));
        Ok((slot >= oldest).then_some(slot))
    }
}

/// The id of one firing. The same schedule and slot always give the same id,
/// so a restart or a second process can't fire it twice.
fn firing_id(name: &str, slot: DateTime<Utc>) -> JobId {
    let key = format!("{name}|{}", slot.to_rfc3339());
    JobId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes()))
}

async fn list_schedules(store: &dyn Store) -> Result<Vec<Definition>, StoreError> {
    let mut found = Vec::new();
    let mut offset = 0;
    loop {
        let page = store
            .list_definitions(
                DefinitionKind::Schedule,
                PageRequest::new(PAGE_SIZE, offset)?,
            )
            .await?;
        let got = u32::try_from(page.items.len()).unwrap_or(u32::MAX);
        found.extend(page.items);
        if !page.has_more || got == 0 {
            return Ok(found);
        }
        offset = offset.saturating_add(got);
    }
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
        for definition in list_schedules(self.store.as_ref()).await? {
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
    ) -> Result<bool, ScheduleError> {
        let name = &definition.key.name;
        let schedule: Schedule =
            serde_json::from_value(definition.body.clone()).map_err(|error| {
                ScheduleError::Invalid {
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

/// Where schedule files are, and the payload schema of each job kind.
#[derive(Debug, Clone)]
pub struct DefinitionFiles {
    /// The directory holding `schedules/*.yaml`. It's fine if it's missing.
    pub root: PathBuf,
    /// Each job kind's payload schema.
    pub payloads: BTreeMap<String, Value>,
}

fn file_problem(path: &Path, problem: impl std::fmt::Display) -> ScheduleError {
    ScheduleError::File(bounded(&format!("{}: {problem}", path.display())))
}

fn name_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let good = !stem.is_empty()
        && stem.len() <= 64
        && stem
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    good.then(|| stem.to_owned())
}

fn check(name: &str, schedule: &Schedule, files: &DefinitionFiles) -> Result<(), ScheduleError> {
    parse_cron(name, &schedule.cron)?;
    parse_zone(name, schedule.timezone.as_deref())?;
    let invalid = |problem: String| ScheduleError::Invalid {
        name: name.to_owned(),
        problem: bounded(&problem),
    };
    let Some(payload) = files.payloads.get(&schedule.kind) else {
        return Err(ScheduleError::UnknownKind {
            name: name.to_owned(),
            kind: schedule.kind.clone(),
        });
    };
    Validator::from_schema(payload)
        .map_err(|error| invalid(error.to_string()))?
        .check(&schedule.payload)
        .map_err(|refusal| invalid(format!("its payload: {refusal}")))
}

fn read_schedule(path: &Path) -> Result<Value, ScheduleError> {
    let text = std::fs::read_to_string(path).map_err(|error| file_problem(path, error))?;
    let raw: Value = serde_saphyr::from_str(&text).map_err(|error| file_problem(path, error))?;
    let validator = Validator::for_type::<Schedule>().map_err(|error| file_problem(path, error))?;
    validator
        .check(&raw)
        .map_err(|refusal| file_problem(path, refusal))?;
    Ok(raw)
}

/// Makes the store's file-backed schedules match the files, so each has one
/// home. A file that's gone removes its schedule. Anything wrong with any
/// file stops the sync, before anything is written.
///
/// # Errors
///
/// [`ScheduleError`] naming the file and what's wrong with it.
pub async fn sync_schedules(
    store: &dyn Store,
    files: &DefinitionFiles,
    now: DateTime<Utc>,
) -> Result<usize, ScheduleError> {
    let directory = files.root.join("schedules");
    let mut unit = UnitOfWork::new();
    let mut present = BTreeSet::new();
    if directory.is_dir() {
        let entries = std::fs::read_dir(&directory).map_err(|e| file_problem(&directory, e))?;
        for entry in entries {
            let path = entry.map_err(|e| file_problem(&directory, e))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let name = name_of(&path).ok_or_else(|| {
                file_problem(&path, "a name is lowercase letters, digits, - and _")
            })?;
            let body = read_schedule(&path)?;
            let schedule: Schedule =
                serde_json::from_value(body.clone()).map_err(|error| file_problem(&path, error))?;
            check(&name, &schedule, files)?;
            unit = unit.with(WriteOp::PutDefinition(Definition {
                key: DefinitionKey {
                    kind: DefinitionKind::Schedule,
                    name: name.clone(),
                },
                body,
                source: format!("{FILE_SOURCE}schedules/{name}.yaml"),
                updated_at: now,
            }));
            present.insert(name);
        }
    }
    for existing in list_schedules(store).await? {
        if existing.source.starts_with(FILE_SOURCE) && !present.contains(&existing.key.name) {
            unit = unit.with(WriteOp::DeleteDefinition(existing.key));
        }
    }
    store.apply(unit).await?;
    Ok(present.len())
}
