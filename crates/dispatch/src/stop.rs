//! Stopping runs early, all at once, so each still ends with its receipt.

use std::sync::Arc;

use tokio::sync::watch;

/// Tells sessions to stop. A session holding one ends at its next step, or
/// stops waiting on a model or tool call, and finishes as
/// [`BudgetKind::Stopped`](crate::BudgetKind::Stopped). Clones share one
/// signal.
#[derive(Debug, Clone)]
pub struct StopSignal(Arc<watch::Sender<bool>>);

impl Default for StopSignal {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }
}

impl StopSignal {
    /// A signal not yet given.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops every session holding this signal.
    pub fn stop(&self) {
        self.0.send_replace(true);
    }

    /// Whether it has been given.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        *self.0.borrow()
    }

    /// Waits until it's given.
    pub async fn stopped(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|stopped| *stopped).await;
    }
}
