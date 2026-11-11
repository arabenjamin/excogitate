//! Getting vectors. Everything above this works without one.

use std::sync::Arc;

use async_trait::async_trait;
use model::{Gateway, GatewayError, ModelError, RoutePolicy};
use protocol::DataClass;
use schema::bounded;

/// An embedding call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EmbedError {
    /// What went wrong.
    pub message: String,
    /// Whether a shorter input might work. An over-long input usually comes
    /// back as a server error, and there's no point retrying a dead server.
    pub try_shorter: bool,
}

/// Turns text into vectors. The privacy class decides who may do it.
#[async_trait]
pub trait Embedder: Send + Sync + std::fmt::Debug {
    /// The width of its vectors.
    fn dimension(&self) -> usize;

    /// One vector per input, in order.
    ///
    /// # Errors
    ///
    /// [`EmbedError`] if the call fails.
    async fn embed(
        &self,
        inputs: &[String],
        data_class: DataClass,
    ) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// An embedder that goes through the model gateway, so routing, privacy and
/// spend caps apply.
#[derive(Debug, Clone)]
pub struct GatewayEmbedder {
    gateway: Arc<Gateway>,
    dimension: usize,
    purpose: String,
}

impl GatewayEmbedder {
    /// An embedder for vectors of this width, on behalf of `purpose`.
    #[must_use]
    pub fn new(gateway: Arc<Gateway>, dimension: usize, purpose: impl Into<String>) -> Self {
        Self {
            gateway,
            dimension,
            purpose: purpose.into(),
        }
    }
}

fn may_be_length(error: &GatewayError) -> bool {
    let GatewayError::Failed(attempts) = error else {
        return false;
    };
    attempts.iter().any(|attempt| {
        matches!(
            attempt.error,
            ModelError::Status {
                code: 400 | 413 | 422 | 500,
                ..
            }
        )
    })
}

#[async_trait]
impl Embedder for GatewayEmbedder {
    fn dimension(&self) -> usize {
        self.dimension
    }

    async fn embed(
        &self,
        inputs: &[String],
        data_class: DataClass,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let policy = RoutePolicy {
            capabilities: vec![],
            data_class,
            purpose: self.purpose.clone(),
            budget_usd_micros: None,
            embedding_dimension: Some(self.dimension),
        };
        self.gateway
            .embed(&policy, inputs)
            .await
            .map(|embedded| embedded.vectors)
            .map_err(|error| EmbedError {
                try_shorter: may_be_length(&error),
                message: bounded(&error.to_string()),
            })
    }
}
