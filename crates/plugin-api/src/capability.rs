//! What a plugin can ask the brain to do. Each one is granted by name, and
//! there's deliberately no way to call another tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use store::{Direction, Page, PageRequest, Record, RecordId, RecordQuery};

use crate::{PluginCapability, ToolContext};

/// A capability call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CapabilityError {
    /// The plugin wasn't granted this capability.
    #[error("capability {0:?} wasn't granted")]
    NotGranted(PluginCapability),
    /// The request isn't allowed or isn't valid.
    #[error("refused: {0}")]
    Refused(String),
    /// The brain couldn't do it.
    #[error("failed: {0}")]
    Failed(String),
}

/// The plugin's own records, and only its own: every type and relation name
/// must carry the plugin's prefix. The brain sets each record's provenance.
#[async_trait]
pub trait RecordsCapability: Send + Sync + std::fmt::Debug {
    /// Adds or replaces a record, returning its id.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn put(
        &self,
        record_type: &str,
        id: Option<RecordId>,
        data: Value,
        context: ToolContext,
    ) -> Result<RecordId, CapabilityError>;

    /// A record, if it exists.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn get(&self, record_type: &str, id: RecordId)
    -> Result<Option<Record>, CapabilityError>;

    /// Records meeting typed filters.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn query(&self, query: RecordQuery) -> Result<Page<Record>, CapabilityError>;

    /// Removes a record and its links.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn delete(&self, record_type: &str, id: RecordId) -> Result<(), CapabilityError>;

    /// Links two records by a declared relation.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn link(
        &self,
        relation: &str,
        from: RecordId,
        to: RecordId,
    ) -> Result<(), CapabilityError>;

    /// The records a relation links to or from `id`.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
    ) -> Result<Page<Record>, CapabilityError>;
}

/// An HTTP request, which the brain makes under its own network rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method.
    pub method: String,
    /// The URL.
    pub url: String,
    /// Headers. Credentials are attached by host configuration, never here.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Option<String>,
}

/// An HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body, cut to the host's limit.
    pub body: String,
    /// Whether the body was cut.
    pub truncated: bool,
}

/// HTTP through the brain.
#[async_trait]
pub trait HttpCapability: Send + Sync + std::fmt::Debug {
    /// Makes a request. Private addresses are refused by the host.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn request(&self, request: HttpRequest) -> Result<HttpResponse, CapabilityError>;
}

/// One-shot model calls, routed and charged by the brain.
#[async_trait]
pub trait ModelCapability: Send + Sync + std::fmt::Debug {
    /// Asks a model. The brain picks the model, under the run's privacy class.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn complete(&self, prompt: &str, context: ToolContext)
    -> Result<String, CapabilityError>;
}

/// The capabilities handed to a plugin: those it declared and was granted.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// Its own records, if granted.
    pub records: Option<Arc<dyn RecordsCapability>>,
    /// HTTP, if granted.
    pub http: Option<Arc<dyn HttpCapability>>,
    /// Model calls, if granted.
    pub model: Option<Arc<dyn ModelCapability>>,
}

impl Capabilities {
    /// The records capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn records(&self) -> Result<Arc<dyn RecordsCapability>, CapabilityError> {
        self.records
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Records))
    }

    /// The HTTP capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn http(&self) -> Result<Arc<dyn HttpCapability>, CapabilityError> {
        self.http
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Http))
    }

    /// The model capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn model(&self) -> Result<Arc<dyn ModelCapability>, CapabilityError> {
        self.model
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Model))
    }
}
