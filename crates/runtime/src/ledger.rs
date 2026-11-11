//! The daily spend caps, kept in the store so a restart can't reset them.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use model::SpendLedger;
use store::Store;

/// A spend ledger on the store. If the store fails, a reservation is
/// refused: a cap fails closed.
#[derive(Debug)]
pub(crate) struct StoreLedger(pub(crate) Arc<dyn Store>);

#[async_trait]
impl SpendLedger for StoreLedger {
    async fn reserve(&self, provider: &str, amount: u64, cap: u64, now: DateTime<Utc>) -> bool {
        match self
            .0
            .reserve_spend(provider, now.date_naive(), amount, cap)
            .await
        {
            Ok(reserved) => reserved,
            Err(error) => {
                eprintln!("the spend ledger failed, so the call is refused: {error}");
                false
            }
        }
    }

    async fn settle(&self, provider: &str, reserved: u64, actual: u64, now: DateTime<Utc>) {
        if let Err(error) = self
            .0
            .settle_spend(provider, now.date_naive(), reserved, actual)
            .await
        {
            eprintln!("the spend ledger couldn't settle a call for {provider}: {error}");
        }
    }
}
