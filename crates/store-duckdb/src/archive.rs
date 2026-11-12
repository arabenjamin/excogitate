//! Everything the file holds, read for an export.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use duckdb::params;
use store::{
    Archive, Link, Note, Page, PageRequest, Record, RecordId, SpendDay, StoreError, WorkingEntry,
};
use uuid::Uuid;

use crate::DuckDbStore;
use crate::codec::{NOTE_COLUMNS, RECORD_COLUMNS, WORKING_COLUMNS, note_at, record_at, working_at};
use crate::read::paged;

fn id_at(row: &duckdb::Row<'_>, index: usize) -> duckdb::Result<RecordId> {
    let text: String = row.get(index)?;
    Uuid::parse_str(&text)
        .map(RecordId::from_uuid)
        .map_err(|error| {
            duckdb::Error::FromSqlConversionFailure(
                index,
                duckdb::types::Type::Text,
                Box::new(error),
            )
        })
}

#[async_trait]
impl Archive for DuckDbStore {
    async fn list_notes(&self, page: PageRequest) -> Result<Page<Note>, StoreError> {
        self.read(move |connection| {
            let sql = format!("SELECT {NOTE_COLUMNS} FROM notes ORDER BY id");
            paged(connection, &sql, params![], page, note_at)
        })
        .await
    }

    async fn list_working(
        &self,
        now: DateTime<Utc>,
        page: PageRequest,
    ) -> Result<Page<WorkingEntry>, StoreError> {
        self.read(move |connection| {
            let sql = format!(
                "SELECT {WORKING_COLUMNS} FROM working
                 WHERE expires_at IS NULL OR expires_at > ? ORDER BY scope, key"
            );
            paged(connection, &sql, params![now], page, working_at)
        })
        .await
    }

    async fn list_all_records(&self, page: PageRequest) -> Result<Page<Record>, StoreError> {
        self.read(move |connection| {
            let sql =
                format!("SELECT {RECORD_COLUMNS} FROM records r ORDER BY r.record_type, r.id");
            paged(connection, &sql, params![], page, record_at)
        })
        .await
    }

    async fn list_links(&self, page: PageRequest) -> Result<Page<Link>, StoreError> {
        self.read(move |connection| {
            let sql =
                "SELECT relation, from_id, to_id FROM links ORDER BY relation, from_id, to_id";
            paged(connection, sql, params![], page, |row| {
                Ok(Link {
                    relation: row.get(0)?,
                    from: id_at(row, 1)?,
                    to: id_at(row, 2)?,
                })
            })
        })
        .await
    }

    async fn list_spend(&self, page: PageRequest) -> Result<Page<SpendDay>, StoreError> {
        self.read(move |connection| {
            let sql = "SELECT provider, day, usd_micros FROM spend ORDER BY provider, day";
            paged(connection, sql, params![], page, |row| {
                Ok(SpendDay {
                    provider: row.get(0)?,
                    day: row.get::<_, NaiveDate>(1)?,
                    usd_micros: row.get(2)?,
                })
            })
        })
        .await
    }
}
