//! Harness² changing an agent's file on its own: the rules first, then the
//! write, the reload, the record and the notice. And the owner's way back.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use harness::{
    Adoption, Attempt, CALIBRATION_DAYS, CALIBRATION_TYPE, CHANGE_TYPE, Calibration, Change,
    ChangeStatus, Facts, HOLD_TYPE, Hold, Judges, MAX_FILE_CHARS, check_adoption, hold_id,
};
use serde_json::json;
use store::{
    FieldFilter, FieldOp, FieldValue, PageRequest, Record, RecordId, RecordQuery, UnitOfWork,
    WriteOp,
};
use workflow::Agent;

use crate::Brain;

/// How long the owner's webhook has to answer.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// The most changes to one agent read back.
const MAX_CHANGES: u32 = 200;

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

impl<E: std::fmt::Display> From<E> for ChangeError {
    fn from(error: E) -> Self {
        Self::Internal(schema::bounded(&error.to_string()))
    }
}

fn text(field: &str, value: &str) -> FieldFilter {
    FieldFilter {
        field: field.to_owned(),
        op: FieldOp::Eq,
        value: FieldValue::Text(value.to_owned()),
    }
}

fn not_adopted(reason: impl Into<String>) -> Adoption {
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
/// new one, never half of either.
fn write_whole(path: &Path, body: &str) -> std::io::Result<()> {
    let staging = path.with_extension("yaml.writing");
    std::fs::write(&staging, body)?;
    std::fs::rename(&staging, path)
}

fn agent_file(root: &Path, name: &str) -> PathBuf {
    root.join("agents").join(format!("{name}.yaml"))
}

impl Brain {
    fn root(&self) -> Result<PathBuf, ChangeError> {
        self.definitions.as_deref().cloned().ok_or_else(|| {
            ChangeError::Conflict("there's no definitions directory to write".to_owned())
        })
    }

    /// Every change made to `agent`, with its id, oldest first.
    pub(crate) async fn changes_to(
        &self,
        agent: &str,
    ) -> Result<Vec<(RecordId, Change)>, ChangeError> {
        let found = self
            .workflows
            .store
            .query_records(RecordQuery {
                record_type: CHANGE_TYPE.to_owned(),
                filters: vec![text("agent", agent)],
                page: PageRequest::new(MAX_CHANGES, 0)?,
            })
            .await?;
        found
            .items
            .into_iter()
            .map(|record| Ok((record.id, serde_json::from_value(record.data)?)))
            .collect()
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
            .query_records(RecordQuery {
                record_type: CALIBRATION_TYPE.to_owned(),
                filters: vec![
                    text("model", model),
                    FieldFilter {
                        field: "passed".to_owned(),
                        op: FieldOp::Eq,
                        value: FieldValue::Bool(true),
                    },
                    FieldFilter {
                        field: "at".to_owned(),
                        op: FieldOp::Gte,
                        value: FieldValue::Timestamp(since),
                    },
                ],
                page: PageRequest::new(1, 0)?,
            })
            .await?;
        if let Some(record) = found.items.first() {
            let passed: Calibration = serde_json::from_value(record.data.clone())?;
            return Ok(Some(passed.at));
        }
        let judges = Judges {
            gateway: &self.gateway,
            max_output_tokens: self.config.harness.judge_output_tokens,
        };
        let test = judges.calibrate(self.config.harness.judge_usd_micros).await;
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
            Err(
                ChangeError::NotFound(why)
                | ChangeError::Conflict(why)
                | ChangeError::Internal(why),
            ) => not_adopted(why),
        }
    }

    async fn try_adopt(
        &self,
        attempt_id: RecordId,
        attempt: &Attempt,
    ) -> Result<Adoption, ChangeError> {
        if !self.config.harness.adopt {
            return Ok(not_adopted("adoption is turned off in brain.yaml"));
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
            .find(|(_, change)| change.status == ChangeStatus::Live);
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
        self.workflows
            .store
            .apply(
                UnitOfWork::new()
                    .with(WriteOp::PutRecord(Self::hold_record(&hold)?))
                    .with(WriteOp::PutRecord(harness::record(
                        CHANGE_TYPE,
                        id,
                        &replaced,
                        change.at,
                    )?)),
            )
            .await?;
        Ok(not_adopted(format!("the agent is held: {}", hold.reason)))
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
        let edited = Agent {
            system_prompt: candidate.system_prompt.clone(),
            ..agent.clone()
        };
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
        self.replace_file(&root, &path, &new_file, &old_file)
            .await?;
        let evidence = attempt
            .verdicts
            .iter()
            .filter(|checked| {
                checked.verdict.candidate == candidate.label && checked.refused.is_none()
            })
            .map(|checked| checked.verdict.cite.clone())
            .take(8)
            .collect();
        let mut change = Change {
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
            status: ChangeStatus::Live,
            ended_at: None,
            notice: String::new(),
        };
        change.notice = self.notify(change_id, &change).await;
        let mut unit = UnitOfWork::new().with(WriteOp::PutRecord(harness::record(
            CHANGE_TYPE,
            change_id,
            &change,
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
        self.workflows.store.apply(unit).await?;
        Ok(Adoption {
            adopted: true,
            change_id: Some(change_id.to_string()),
            reason: "the evidence supported it and every rule was met".to_owned(),
        })
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
        if change.status != ChangeStatus::Live {
            return Err(ChangeError::Conflict(
                "only a live change can be reverted".to_owned(),
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
        self.workflows
            .store
            .apply(
                UnitOfWork::new()
                    .with(WriteOp::PutRecord(harness::record(
                        CHANGE_TYPE,
                        id,
                        &reverted,
                        reverted.at,
                    )?))
                    .with(WriteOp::PutRecord(Self::hold_record(&hold)?)),
            )
            .await?;
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
            .query_records(RecordQuery {
                record_type: CHANGE_TYPE.to_owned(),
                filters,
                page,
            })
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
    pub(crate) async fn holds(&self, page: PageRequest) -> Result<store::Page<Hold>, ChangeError> {
        self.harness_records().await?;
        let found = self
            .workflows
            .store
            .query_records(RecordQuery {
                record_type: HOLD_TYPE.to_owned(),
                filters: Vec::new(),
                page,
            })
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
