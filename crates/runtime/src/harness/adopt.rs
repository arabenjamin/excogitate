//! Harness² changing an agent's file on its own: the rules first, then the
//! write, the reload, the record and the notice. And the owner's way back.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use harness::{
    Adoption, Attempt, CALIBRATION_DAYS, CALIBRATION_TYPE, CHANGE_TYPE, Calibration, Change,
    ChangeStatus, Facts, HOLD_TYPE, Hold, Judges, LESSON_TYPE, Lesson, LessonStatus,
    MAX_FILE_CHARS, check_adoption, hold_id, lesson_id,
};
use serde_json::json;
use store::{
    FieldFilter, FieldOp, FieldValue, PageRequest, Record, RecordId, RecordQuery, UnitOfWork,
    WriteOp,
};
use workflow::Agent;

use crate::Runtime;

/// How long the owner's webhook has to answer.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// How many changes are read at once.
const CHANGE_PAGE: u32 = store::MAX_PAGE_LIMIT;
/// The most changes to one agent read back. Past this, adoption stops
/// rather than decide from part of the history.
const MAX_CHANGES: usize = 100_000;

/// Why a change to a file couldn't be made.
#[derive(Debug)]
pub(crate) enum ChangeError {
    /// Nothing by that id.
    NotFound(String),
    /// It can't be done in the state things are in.
    Conflict(String),
    /// Something failed.
    Internal(String),
}

impl ChangeError {
    /// What went wrong, in a sentence.
    pub(crate) fn message(self) -> String {
        match self {
            Self::NotFound(why) | Self::Conflict(why) | Self::Internal(why) => why,
        }
    }
}

impl<E: std::fmt::Display> From<E> for ChangeError {
    fn from(error: E) -> Self {
        Self::Internal(schema::bounded(&error.to_string()))
    }
}

fn text(field: &str, value: &str) -> FieldFilter {
    FieldFilter::new(
        field.to_owned(),
        FieldOp::Eq,
        FieldValue::Text(value.to_owned()),
    )
}

pub(super) fn not_adopted(reason: impl Into<String>) -> Adoption {
    Adoption {
        adopted: false,
        change_id: None,
        reason: schema::bounded(&reason.into()),
    }
}

/// What making a change needs.
struct Making<'a> {
    root: &'a Path,
    attempt_id: RecordId,
    attempt: &'a Attempt,
    candidate: &'a harness::Candidate,
    agent: &'a Agent,
    current: String,
    model: &'a str,
    live: Option<&'a (RecordId, Change)>,
    now: DateTime<Utc>,
}

/// Writes `body` to `path` all at once: a reader sees the old file or the
/// new one, never half of either, and it's on disk before this returns. The
/// staging file has a name no one can guess and is made new, and a link in
/// place of the agent's file is refused rather than followed.
fn write_whole(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    if std::fs::symlink_metadata(path).is_ok_and(|found| !found.file_type().is_file()) {
        return Err(std::io::Error::other(format!(
            "{} isn't a plain file, so it isn't replaced",
            path.display()
        )));
    }
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the agent's file has no directory"))?;
    let staging = directory.join(format!(".{}.writing", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staging, path)?;
        std::fs::File::open(directory)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written
}

fn agent_file(root: &Path, name: &str) -> PathBuf {
    root.join("agents").join(format!("{name}.yaml"))
}

impl Runtime {
    fn root(&self) -> Result<PathBuf, ChangeError> {
        self.definitions.as_deref().cloned().ok_or_else(|| {
            ChangeError::Conflict("there's no definitions directory to write".to_owned())
        })
    }

    /// Every change made to `agent`, with its id, oldest first: all of
    /// them, a page at a time, since the newest decide what may happen next.
    pub(crate) async fn changes_to(
        &self,
        agent: &str,
    ) -> Result<Vec<(RecordId, Change)>, ChangeError> {
        let mut changes = Vec::new();
        let mut offset = 0;
        loop {
            let found = self
                .workflows
                .store
                .query_records(
                    RecordQuery::new(
                        CHANGE_TYPE.to_owned(),
                        PageRequest::new(CHANGE_PAGE, offset)?,
                    )
                    .with_filters(vec![text("agent", agent)]),
                )
                .await?;
            for record in found.items {
                changes.push((record.id, serde_json::from_value(record.data)?));
            }
            if !found.has_more {
                return Ok(changes);
            }
            offset = offset.saturating_add(CHANGE_PAGE);
            if changes.len() >= MAX_CHANGES {
                return Err(ChangeError::Internal(format!(
                    "{agent} has more than {MAX_CHANGES} changes, too many to read safely"
                )));
            }
        }
    }

    pub(crate) async fn hold_on(&self, agent: &str) -> Result<Option<Hold>, ChangeError> {
        let found = self
            .workflows
            .store
            .get_record(HOLD_TYPE, hold_id(agent))
            .await?;
        Ok(found
            .map(|record| serde_json::from_value(record.data))
            .transpose()?)
    }

    fn hold_record(hold: &Hold) -> Result<Record, ChangeError> {
        Ok(harness::record(
            HOLD_TYPE,
            hold_id(&hold.agent),
            hold,
            hold.since,
        )?)
    }

    /// When `model` last passed its known-answer test, recently enough to
    /// count. If it hasn't, it takes the test now.
    async fn calibrated(
        &self,
        model: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ChangeError> {
        let since = now - chrono::Duration::days(CALIBRATION_DAYS);
        let found = self
            .workflows
            .store
            .query_records(
                RecordQuery::new(CALIBRATION_TYPE.to_owned(), PageRequest::new(1, 0)?)
                    .with_filters(vec![
                        text("model", model),
                        FieldFilter::new("passed".to_owned(), FieldOp::Eq, FieldValue::Bool(true)),
                        FieldFilter::new(
                            "at".to_owned(),
                            FieldOp::Gte,
                            FieldValue::Timestamp(since),
                        ),
                    ]),
            )
            .await?;
        if let Some(record) = found.items.first() {
            let passed: Calibration = serde_json::from_value(record.data.clone())?;
            return Ok(Some(passed.at));
        }
        let judges = Judges {
            gateway: &self.gateway,
            max_output_tokens: self.config.harness.judge_output_tokens,
        };
        let test = judges
            .calibrate(model, self.config.harness.judge_usd_micros)
            .await;
        let record = harness::record(CALIBRATION_TYPE, RecordId::generate(), &test, test.at)?;
        self.workflows
            .store
            .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
            .await?;
        Ok((test.passed && test.model == model).then_some(test.at))
    }

    /// Adopts a supported edit if it meets every rule, and says what happened.
    pub(crate) async fn adopt(&self, attempt_id: RecordId, attempt: &Attempt) -> Adoption {
        match self.try_adopt(attempt_id, attempt).await {
            Ok(adoption) => adoption,
            Err(error) => not_adopted(error.message()),
        }
    }

    async fn try_adopt(
        &self,
        attempt_id: RecordId,
        attempt: &Attempt,
    ) -> Result<Adoption, ChangeError> {
        if !self.config.harness.adopt {
            return Ok(not_adopted("adoption is turned off in config.yaml"));
        }
        let _changing = self.changing.lock().await;
        let root = self.root()?;
        let name = &attempt.request.agent;
        let Some(agent) = self.workflows.agent(name).await? else {
            return Ok(not_adopted("the agent no longer exists"));
        };
        let current = self.workflows.agent_harness(&agent);
        let now = Utc::now();
        let changes = self.changes_to(name).await?;
        let live = changes
            .iter()
            .rev()
            .find(|(_, change)| change.in_effect(&current));
        if let Some((id, change)) = live
            && change.to_harness != current
        {
            return self.held_after_edit(name, *id, change, now).await;
        }
        let hold = self.hold_on(name).await?;
        let reverted: BTreeSet<String> = changes
            .iter()
            .filter(|(_, change)| change.status == ChangeStatus::Reverted)
            .map(|(_, change)| change.to_harness.clone())
            .collect();
        let last_change = changes.iter().map(|(_, change)| change.at).max();
        let mut facts = Facts {
            attempt,
            current_harness: &current,
            hold: hold.as_ref(),
            reverted: &reverted,
            last_change,
            // Every other rule first, so a test isn't paid for in vain.
            calibrated_at: Some(now),
            now,
        };
        if let Err(why) = check_adoption(&facts) {
            return Ok(not_adopted(why));
        }
        let Some(model) = attempt.composer_model.as_deref() else {
            return Ok(not_adopted("the attempt doesn't say which model judged it"));
        };
        facts.calibrated_at = self.calibrated(model, now).await?;
        if let Err(why) = check_adoption(&facts) {
            return Ok(not_adopted(why));
        }
        let Some(candidate) = attempt.supported() else {
            return Ok(not_adopted("the evidence supports no change"));
        };
        self.make_change(Making {
            root: &root,
            attempt_id,
            attempt,
            candidate,
            agent: &agent,
            current,
            model,
            live,
            now,
        })
        .await
    }

    /// The owner edited the file after the system's last change. Their edit
    /// stands, and the agent is left alone until they say so.
    async fn held_after_edit(
        &self,
        name: &str,
        id: RecordId,
        change: &Change,
        now: DateTime<Utc>,
    ) -> Result<Adoption, ChangeError> {
        let hold = Hold {
            agent: name.to_owned(),
            since: now,
            reason: format!("the owner edited the agent's file after change {id}"),
        };
        let replaced = Change {
            status: ChangeStatus::Replaced,
            ended_at: Some(now),
            ..change.clone()
        };
        let mut unit = UnitOfWork::new()
            .with(WriteOp::PutRecord(Self::hold_record(&hold)?))
            .with(WriteOp::PutRecord(harness::record(
                CHANGE_TYPE,
                id,
                &replaced,
                change.at,
            )?));
        // The owner's edit took its place, so its lesson no longer counts.
        if let Some(retired) = self.retired_lesson(id).await? {
            unit = unit.with(retired);
        }
        self.workflows.store.apply(unit).await?;
        Ok(not_adopted(format!("the agent is held: {}", hold.reason)))
    }

    /// The write that retires a change's lesson, if it left one.
    async fn retired_lesson(&self, change_id: RecordId) -> Result<Option<WriteOp>, ChangeError> {
        let id = lesson_id(change_id);
        let Some(found) = self.workflows.store.get_record(LESSON_TYPE, id).await? else {
            return Ok(None);
        };
        let mut lesson: Lesson = serde_json::from_value(found.data)?;
        lesson.status = LessonStatus::Retired;
        Ok(Some(WriteOp::PutRecord(harness::record(
            LESSON_TYPE,
            id,
            &lesson,
            lesson.at,
        )?)))
    }

    /// Writes the agent's new file, records the change and tells the owner.
    async fn make_change(&self, making: Making<'_>) -> Result<Adoption, ChangeError> {
        let Making {
            root,
            attempt_id,
            attempt,
            candidate,
            agent,
            current,
            model,
            live,
            now,
        } = making;
        let name = &attempt.request.agent;
        let root = root.to_path_buf();
        let edited = candidate.agent(agent);
        let to_harness = self.workflows.agent_harness(&edited);
        let change_id = RecordId::generate();
        let path = agent_file(&root, name);
        let old_file = std::fs::read_to_string(&path)?;
        if old_file.chars().count() > MAX_FILE_CHARS {
            return Ok(not_adopted(
                "the agent's file is too large to keep a copy of",
            ));
        }
        let new_file = format!(
            "# Changed by Harness² on {date} (change {change_id}), from the evidence in\n\
             # attempt {attempt_id}. The file it replaced is kept with the change, and\n\
             # POST /api/v1/harness/changes/{change_id}/revert puts it back.\n{body}",
            date = now.format("%Y-%m-%d"),
            body = serde_saphyr::to_string(&edited)?,
        );
        if new_file.chars().count() > MAX_FILE_CHARS {
            return Ok(not_adopted("the new file would be too large to keep"));
        }
        let evidence = attempt
            .verdicts
            .iter()
            .filter(|checked| {
                checked.verdict.candidate == candidate.label && checked.refused.is_none()
            })
            .map(|checked| checked.verdict.cite.clone())
            .take(8)
            .collect();
        let change = Change {
            agent: name.clone(),
            attempt_id: attempt_id.to_string(),
            at: now,
            from_harness: current,
            to_harness,
            old_file,
            new_file,
            summary: candidate.change.clone(),
            evidence,
            judge_model: model.to_owned(),
            status: ChangeStatus::Pending,
            ended_at: None,
            notice: String::new(),
        };
        let lesson = Lesson {
            agent: name.clone(),
            change_id: change_id.to_string(),
            at: now,
            situation: harness::situation(&attempt.request.task, attempt.request.why.as_deref()),
            lesson: change.summary.clone(),
            evidence: change.evidence.clone(),
            status: LessonStatus::Active,
        };
        self.commit_change(&root, &path, change_id, change, live, &lesson)
            .await
    }

    /// Puts a change in effect: its record, then its file, then the record
    /// marked live and the change it replaces, then the owner told.
    async fn commit_change(
        &self,
        root: &Path,
        path: &Path,
        change_id: RecordId,
        mut change: Change,
        live: Option<&(RecordId, Change)>,
        lesson: &Lesson,
    ) -> Result<Adoption, ChangeError> {
        let now = change.at;
        // The record first: whatever stops this from here on, the edit is
        // never on disk without one.
        self.put_change(change_id, &change).await?;
        if let Err(error) = self
            .replace_file(root, path, &change.new_file, &change.old_file)
            .await
        {
            change.status = ChangeStatus::Abandoned;
            change.ended_at = Some(Utc::now());
            self.put_change(change_id, &change).await?;
            return Err(error);
        }
        change.status = ChangeStatus::Live;
        // Its lesson goes into the library with it: only an adopted change
        // teaches anything.
        let mut unit = UnitOfWork::new()
            .with(WriteOp::PutRecord(harness::record(
                CHANGE_TYPE,
                change_id,
                &change,
                now,
            )?))
            .with(WriteOp::PutRecord(harness::record(
                LESSON_TYPE,
                lesson_id(change_id),
                lesson,
                now,
            )?));
        if let Some((id, earlier)) = live {
            let replaced = Change {
                status: ChangeStatus::Replaced,
                ended_at: Some(now),
                ..earlier.clone()
            };
            unit = unit.with(WriteOp::PutRecord(harness::record(
                CHANGE_TYPE,
                *id,
                &replaced,
                earlier.at,
            )?));
        }
        self.workflows.store.apply(unit).await.map_err(|error| {
            ChangeError::Internal(format!(
                "the new file is in place, and change {change_id} is still marked pending: {error}"
            ))
        })?;
        // Last, so a slow webhook can't come between the file and its record.
        change.notice = self.notify(change_id, &change).await;
        if let Err(error) = self.put_change(change_id, &change).await {
            eprintln!(
                "change {change_id} is live, but how the owner was told wasn't kept: {}",
                error.message()
            );
        }
        Ok(Adoption {
            adopted: true,
            change_id: Some(change_id.to_string()),
            reason: "the evidence supported it and every rule was met".to_owned(),
        })
    }

    async fn put_change(&self, id: RecordId, change: &Change) -> Result<(), ChangeError> {
        let record = harness::record(CHANGE_TYPE, id, change, change.at)?;
        self.workflows
            .store
            .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
            .await?;
        Ok(())
    }

    /// Puts `body` in place of the file at `path` and reloads every
    /// definition. If they don't load, `previous` goes back.
    async fn replace_file(
        &self,
        root: &Path,
        path: &Path,
        body: &str,
        previous: &str,
    ) -> Result<(), ChangeError> {
        write_whole(path, body)?;
        if let Err(error) = self.load_definitions(root, Utc::now()).await {
            write_whole(path, previous)?;
            self.load_definitions(root, Utc::now()).await?;
            return Err(ChangeError::Internal(format!(
                "the new file didn't load, so the old one is back: {error}"
            )));
        }
        Ok(())
    }

    /// Tells the owner of a change, if they gave somewhere to send it, and
    /// says how that went.
    async fn notify(&self, id: RecordId, change: &Change) -> String {
        let Some(url) = self.config.harness.notify_url.as_deref() else {
            return "no webhook is set; the change is in GET /api/v1/harness/changes".to_owned();
        };
        let body = json!({
            "event": "harness.change",
            "change_id": id.to_string(),
            "agent": change.agent,
            "summary": change.summary,
            "revert": format!("/api/v1/harness/changes/{id}/revert"),
        });
        let sent = reqwest::Client::new()
            .post(url)
            .timeout(NOTIFY_TIMEOUT)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await;
        match sent {
            Ok(response) if response.status().is_success() => "sent to the webhook".to_owned(),
            Ok(response) => format!("the webhook answered {}", response.status()),
            Err(error) => schema::bounded(&format!("the webhook couldn't be reached: {error}")),
        }
    }

    /// Puts a change's old file back, and holds the agent so the system
    /// leaves it alone.
    pub(crate) async fn revert(&self, id: RecordId) -> Result<Change, ChangeError> {
        let _changing = self.changing.lock().await;
        let change = self.change(id).await?;
        // A pending change is in effect if its file went in; the check on
        // the agent's harness below makes sure it did.
        if !matches!(change.status, ChangeStatus::Live | ChangeStatus::Pending) {
            return Err(ChangeError::Conflict(
                "only a change in effect can be reverted".to_owned(),
            ));
        }
        let root = self.root()?;
        let current = self
            .workflows
            .agent(&change.agent)
            .await?
            .map(|agent| self.workflows.agent_harness(&agent));
        if current.as_deref() != Some(change.to_harness.as_str()) {
            return Err(ChangeError::Conflict(
                "the agent's file has changed since; edit it by hand".to_owned(),
            ));
        }
        let path = agent_file(&root, &change.agent);
        self.replace_file(&root, &path, &change.old_file, &change.new_file)
            .await?;
        let now = Utc::now();
        let reverted = Change {
            status: ChangeStatus::Reverted,
            ended_at: Some(now),
            ..change
        };
        let hold = Hold {
            agent: reverted.agent.clone(),
            since: now,
            reason: format!("the owner reverted change {id}"),
        };
        let mut unit = UnitOfWork::new()
            .with(WriteOp::PutRecord(harness::record(
                CHANGE_TYPE,
                id,
                &reverted,
                reverted.at,
            )?))
            .with(WriteOp::PutRecord(Self::hold_record(&hold)?));
        // What it taught no longer counts.
        if let Some(retired) = self.retired_lesson(id).await? {
            unit = unit.with(retired);
        }
        self.workflows.store.apply(unit).await?;
        Ok(reverted)
    }

    /// Changes, newest last, optionally to one agent or in one state.
    pub(crate) async fn changes(
        &self,
        agent: Option<&str>,
        status: Option<&str>,
        page: PageRequest,
    ) -> Result<store::Page<(RecordId, Change)>, ChangeError> {
        self.harness_records().await?;
        let mut filters = Vec::new();
        if let Some(agent) = agent {
            filters.push(text("agent", agent));
        }
        if let Some(status) = status {
            filters.push(text("status", status));
        }
        let found = self
            .workflows
            .store
            .query_records(RecordQuery::new(CHANGE_TYPE.to_owned(), page).with_filters(filters))
            .await?;
        let items = found
            .items
            .into_iter()
            .map(|record| Ok((record.id, serde_json::from_value(record.data)?)))
            .collect::<Result<_, ChangeError>>()?;
        Ok(store::Page {
            items,
            has_more: found.has_more,
        })
    }

    /// One change.
    pub(crate) async fn change(&self, id: RecordId) -> Result<Change, ChangeError> {
        self.harness_records().await?;
        let record = self
            .workflows
            .store
            .get_record(CHANGE_TYPE, id)
            .await?
            .ok_or_else(|| ChangeError::NotFound("there's no change with that id".to_owned()))?;
        Ok(serde_json::from_value(record.data)?)
    }

    /// The agents that are held.
    /// The lesson library, optionally for one agent or in one state.
    pub(crate) async fn lessons(
        &self,
        agent: Option<&str>,
        status: Option<LessonStatus>,
        page: PageRequest,
    ) -> Result<store::Page<Lesson>, ChangeError> {
        self.harness_records().await?;
        let mut filters = Vec::new();
        if let Some(agent) = agent {
            filters.push(text("agent", agent));
        }
        if let Some(status) = status {
            filters.push(harness::status_filter(status));
        }
        let found = self
            .workflows
            .store
            .query_records(RecordQuery::new(LESSON_TYPE.to_owned(), page).with_filters(filters))
            .await?;
        let items = found
            .items
            .into_iter()
            .map(|record| Ok(serde_json::from_value(record.data)?))
            .collect::<Result<_, ChangeError>>()?;
        Ok(store::Page {
            items,
            has_more: found.has_more,
        })
    }

    pub(crate) async fn holds(&self, page: PageRequest) -> Result<store::Page<Hold>, ChangeError> {
        self.harness_records().await?;
        let found = self
            .workflows
            .store
            .query_records(RecordQuery::new(HOLD_TYPE.to_owned(), page))
            .await?;
        let items = found
            .items
            .into_iter()
            .map(|record| Ok(serde_json::from_value(record.data)?))
            .collect::<Result<_, ChangeError>>()?;
        Ok(store::Page {
            items,
            has_more: found.has_more,
        })
    }

    /// Holds an agent, or releases it with `None`.
    pub(crate) async fn set_hold(
        &self,
        agent: &str,
        reason: Option<String>,
    ) -> Result<Option<Hold>, ChangeError> {
        self.harness_records().await?;
        let op = match reason {
            Some(reason) => {
                let hold = Hold {
                    agent: agent.to_owned(),
                    since: Utc::now(),
                    reason,
                };
                let op = WriteOp::PutRecord(Self::hold_record(&hold)?);
                self.workflows
                    .store
                    .apply(UnitOfWork::new().with(op))
                    .await?;
                return Ok(Some(hold));
            }
            None => WriteOp::DeleteRecord {
                record_type: HOLD_TYPE.to_owned(),
                id: hold_id(agent),
            },
        };
        self.workflows
            .store
            .apply(UnitOfWork::new().with(op))
            .await?;
        Ok(None)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("write-whole-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_file_is_replaced_whole_and_nothing_is_left_behind() {
        let root = scratch("whole");
        let path = root.join("helper.yaml");
        std::fs::write(&path, "old").unwrap();
        write_whole(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
        assert_eq!(left.len(), 1, "no staging file is left");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_place_of_the_file_is_refused_not_followed() {
        let root = scratch("link");
        let elsewhere = root.join("elsewhere.txt");
        std::fs::write(&elsewhere, "keep me").unwrap();
        let path = root.join("helper.yaml");
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        assert!(write_whole(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep me");
    }
}
