//! Everything the graph holds, read for an export.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use neo4rs::{Row, query};
use store::{
    Archive, Link, Note, Page, PageRequest, Record, RecordId, SpendDay, StoreError, WorkingEntry,
};
use uuid::Uuid;

use crate::codec::{self, WORKING, at, get};
use crate::{Neo4jStore, page_clause, page_of};

fn id(row: &Row, column: &str) -> Result<RecordId, StoreError> {
    let text: String = get(row, column)?;
    Uuid::parse_str(&text)
        .map(RecordId::from_uuid)
        .map_err(|_| StoreError::Backend(format!("stored id {text} isn't a uuid")))
}

fn link(row: &Row) -> Result<Link, StoreError> {
    Ok(Link {
        relation: get(row, "relation")?,
        from: id(row, "from")?,
        to: id(row, "to")?,
    })
}

fn spend(row: &Row) -> Result<SpendDay, StoreError> {
    let day: String = get(row, "day")?;
    let amount: String = get(row, "amount")?;
    Ok(SpendDay {
        provider: get(row, "provider")?,
        day: NaiveDate::parse_from_str(&day, "%Y-%m-%d")
            .map_err(|_| StoreError::Backend(format!("stored day {day} isn't a date")))?,
        usd_micros: amount
            .parse()
            .map_err(|_| StoreError::Backend(format!("stored spend {amount} isn't a number")))?,
    })
}

#[async_trait]
impl Archive for Neo4jStore {
    async fn list_notes(&self, page: PageRequest) -> Result<Page<Note>, StoreError> {
        let rows = self
            .rows(query(&format!(
                "MATCH (n:Note) RETURN {} ORDER BY n.id {}",
                codec::note_columns(),
                page_clause(page)
            )))
            .await?;
        page_of(&rows, page, codec::note)
    }

    async fn list_working(
        &self,
        now: DateTime<Utc>,
        page: PageRequest,
    ) -> Result<Page<WorkingEntry>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (w:Working) WHERE w.expires_at IS NULL OR w.expires_at > $now \
                     RETURN {WORKING} ORDER BY w.scope, w.key {}",
                    page_clause(page)
                ))
                .param("now", at(now)),
            )
            .await?;
        page_of(&rows, page, codec::working)
    }

    async fn list_all_records(&self, page: PageRequest) -> Result<Page<Record>, StoreError> {
        let rows = self
            .rows(query(&format!(
                "MATCH (n:Record) RETURN {} ORDER BY n.type, n.id {}",
                codec::record_columns(),
                page_clause(page)
            )))
            .await?;
        page_of(&rows, page, codec::record)
    }

    async fn list_links(&self, page: PageRequest) -> Result<Page<Link>, StoreError> {
        let rows = self
            .rows(query(&format!(
                "MATCH (a:Record)-[l:LINK]->(b:Record) \
                 RETURN l.relation AS relation, a.id AS from, b.id AS to \
                 ORDER BY relation, from, to {}",
                page_clause(page)
            )))
            .await?;
        page_of(&rows, page, link)
    }

    async fn list_spend(&self, page: PageRequest) -> Result<Page<SpendDay>, StoreError> {
        let rows = self
            .rows(query(&format!(
                "MATCH (s:Spend) RETURN s.provider AS provider, toString(s.day) AS day, \
                 s.amount AS amount ORDER BY provider, day {}",
                page_clause(page)
            )))
            .await?;
        page_of(&rows, page, spend)
    }
}
