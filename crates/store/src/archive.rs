//! Everything a store holds, read for an export: a page at a time, in an
//! order that doesn't change between pages. Plugins never reach it, and
//! records are read whatever their type, without the catalog, since a
//! store's catalog holds only what this run's plugins declared.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};

use crate::{Note, Page, PageRequest, Record, RecordId, StoreError, WorkingEntry};

/// A link between two records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The relation.
    pub relation: String,
    /// The record it links from.
    pub from: RecordId,
    /// The record it links to.
    pub to: RecordId,
}

/// What a provider spent on one UTC day, in millionths of a dollar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendDay {
    /// The provider.
    pub provider: String,
    /// The day.
    pub day: NaiveDate,
    /// What it spent or reserved.
    pub usd_micros: u64,
}

/// Reads everything, for an export.
#[async_trait]
pub trait Archive {
    /// Every note, in id order.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_notes(&self, page: PageRequest) -> Result<Page<Note>, StoreError>;

    /// Every working-memory entry that hasn't expired by `now`, by scope
    /// and then key.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_working(
        &self,
        now: DateTime<Utc>,
        page: PageRequest,
    ) -> Result<Page<WorkingEntry>, StoreError>;

    /// Every record, whatever its type, by type and then id.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_all_records(&self, page: PageRequest) -> Result<Page<Record>, StoreError>;

    /// Every link, by relation, then the record it's from, then the one it's
    /// to.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_links(&self, page: PageRequest) -> Result<Page<Link>, StoreError>;

    /// Every day's spend, by provider and then day.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_spend(&self, page: PageRequest) -> Result<Page<SpendDay>, StoreError>;
}
