//! The records capability: the store port, narrowed to one plugin's own
//! records, with provenance set by the runtime.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use plugin_api::{
    CapabilityError, RecordLink, RecordWrite, RecordsCapability, ToolContext, check_batch,
};
use protocol::{Actor, Trust};
use serde_json::Value;
use store::{
    Direction, Page, PageRequest, Provenance, Record, RecordId, RecordQuery, Store, StoreError,
    UnitOfWork, WriteOp,
};

/// One plugin's view of the store.
#[derive(Debug)]
pub struct ScopedRecords {
    store: Arc<dyn Store>,
    prefix: String,
    actor: Actor,
}

fn map_error(error: StoreError) -> CapabilityError {
    match error {
        StoreError::Backend(detail) => CapabilityError::Failed(detail),
        other => CapabilityError::Refused(other.to_string()),
    }
}

impl ScopedRecords {
    /// The view for the plugin with this name.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, plugin: &str) -> Self {
        Self {
            store,
            prefix: format!("{plugin}."),
            actor: Actor::new(format!("plugin:{plugin}")),
        }
    }

    fn own(&self, name: &str) -> Result<(), CapabilityError> {
        if name.starts_with(&self.prefix) {
            Ok(())
        } else {
            Err(CapabilityError::Refused(format!(
                "{name} doesn't belong to this plugin"
            )))
        }
    }

    async fn write(&self, op: WriteOp) -> Result<(), CapabilityError> {
        self.apply(UnitOfWork::from(op)).await
    }

    async fn apply(&self, unit: UnitOfWork) -> Result<(), CapabilityError> {
        self.store.apply(unit).await.map_err(map_error)
    }

    /// A record as this plugin stores it: untrusted, and stored by it.
    fn record(
        &self,
        record_type: String,
        id: RecordId,
        data: Value,
        context: ToolContext,
    ) -> Record {
        Record::new(
            record_type,
            id,
            data,
            Provenance {
                data_class: context.data_class,
                // A plugin's data may hold what a model wrote or it fetched.
                trust: Trust::Untrusted,
                stored_by: self.actor.clone(),
                stored_at: Utc::now(),
            },
        )
    }
}

#[async_trait]
impl RecordsCapability for ScopedRecords {
    async fn put(
        &self,
        record_type: &str,
        id: Option<RecordId>,
        data: Value,
        context: ToolContext,
    ) -> Result<RecordId, CapabilityError> {
        self.own(record_type)?;
        let id = id.unwrap_or_else(RecordId::generate);
        let record = self.record(record_type.to_owned(), id, data, context);
        self.write(WriteOp::PutRecord(record)).await?;
        Ok(id)
    }

    async fn get(
        &self,
        record_type: &str,
        id: RecordId,
        _: ToolContext,
    ) -> Result<Option<Record>, CapabilityError> {
        self.own(record_type)?;
        self.store
            .get_record(record_type, id)
            .await
            .map_err(map_error)
    }

    async fn query(
        &self,
        query: RecordQuery,
        _: ToolContext,
    ) -> Result<Page<Record>, CapabilityError> {
        self.own(&query.record_type)?;
        self.store.query_records(query).await.map_err(map_error)
    }

    async fn delete(
        &self,
        record_type: &str,
        id: RecordId,
        _: ToolContext,
    ) -> Result<(), CapabilityError> {
        self.own(record_type)?;
        self.write(WriteOp::DeleteRecord {
            record_type: record_type.to_owned(),
            id,
        })
        .await
    }

    async fn link(
        &self,
        relation: &str,
        from: RecordId,
        to: RecordId,
        _: ToolContext,
    ) -> Result<(), CapabilityError> {
        self.own(relation)?;
        self.write(WriteOp::Link {
            relation: relation.to_owned(),
            from,
            to,
        })
        .await
    }

    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
        _: ToolContext,
    ) -> Result<Page<Record>, CapabilityError> {
        self.own(relation)?;
        self.store
            .neighbors(relation, id, direction, page)
            .await
            .map_err(map_error)
    }

    async fn put_many(
        &self,
        records: Vec<RecordWrite>,
        context: ToolContext,
    ) -> Result<Vec<RecordId>, CapabilityError> {
        check_batch(records.len())?;
        // Every name is checked before anything is written.
        for write in &records {
            self.own(&write.record_type)?;
        }
        let mut unit = UnitOfWork::new();
        let mut ids = Vec::with_capacity(records.len());
        for write in records {
            let id = write.id.unwrap_or_else(RecordId::generate);
            ids.push(id);
            unit = unit.with(WriteOp::PutRecord(self.record(
                write.record_type,
                id,
                write.data,
                context,
            )));
        }
        if !ids.is_empty() {
            self.apply(unit).await?;
        }
        Ok(ids)
    }

    async fn link_many(
        &self,
        links: Vec<RecordLink>,
        _: ToolContext,
    ) -> Result<(), CapabilityError> {
        check_batch(links.len())?;
        for link in &links {
            self.own(&link.relation)?;
        }
        if links.is_empty() {
            return Ok(());
        }
        let unit = links.into_iter().fold(UnitOfWork::new(), |unit, link| {
            unit.with(WriteOp::Link {
                relation: link.relation,
                from: link.from,
                to: link.to,
            })
        });
        self.apply(unit).await
    }
}
