//! What a store can say went wrong.

use schema::{InvalidSchema, Refusal};

/// Why a store refused or failed. The first three are the caller's to
/// handle; the last is the store's own failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The thing named doesn't exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The write conflicts with what's stored: a duplicate id, a stale
    /// version, or a lease the caller doesn't hold.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The request itself is wrong: a bad filter, the wrong vector width, or
    /// data that fails its schema.
    #[error("invalid: {0}")]
    Invalid(String),
    /// The store failed.
    #[error("store failed: {0}")]
    Backend(String),
}

impl From<Refusal> for StoreError {
    fn from(refusal: Refusal) -> Self {
        Self::Invalid(refusal.to_string())
    }
}

impl From<InvalidSchema> for StoreError {
    fn from(error: InvalidSchema) -> Self {
        Self::Invalid(error.to_string())
    }
}
