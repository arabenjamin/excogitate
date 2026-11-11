//! What each provider has spent today.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};

/// Tracks spend per provider per UTC day. Money is reserved before a call and
/// settled after it, so concurrent calls can't overrun a cap together.
#[async_trait]
pub trait SpendLedger: Send + Sync + std::fmt::Debug {
    /// Sets aside `amount` if that stays within `cap`. Returns false if it
    /// doesn't, or if the ledger can't tell: a cap fails closed.
    async fn reserve(&self, provider: &str, amount: u64, cap: u64, now: DateTime<Utc>) -> bool;

    /// Replaces a reservation with what the call really cost.
    async fn settle(&self, provider: &str, reserved: u64, actual: u64, now: DateTime<Utc>);
}

/// A ledger in memory, for tests. A restart forgets the day's spend, so the
/// brain itself keeps its ledger in the store.
#[derive(Debug, Default)]
pub struct MemoryLedger(Mutex<HashMap<(String, NaiveDate), u64>>);

impl MemoryLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SpendLedger for MemoryLedger {
    async fn reserve(&self, provider: &str, amount: u64, cap: u64, now: DateTime<Utc>) -> bool {
        // A poisoned lock means the count can't be trusted: fail closed.
        let Ok(mut spent) = self.0.lock() else {
            return false;
        };
        let day = spent
            .entry((provider.to_owned(), now.date_naive()))
            .or_default();
        match day.checked_add(amount) {
            Some(total) if total <= cap => {
                *day = total;
                true
            }
            _ => false,
        }
    }

    async fn settle(&self, provider: &str, reserved: u64, actual: u64, now: DateTime<Utc>) {
        let Ok(mut spent) = self.0.lock() else {
            return;
        };
        let day = spent
            .entry((provider.to_owned(), now.date_naive()))
            .or_default();
        *day = day.saturating_sub(reserved).saturating_add(actual);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2027, 1, day, 12, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn reservations_cannot_overrun_the_cap_together() {
        let ledger = MemoryLedger::new();
        assert!(ledger.reserve("p", 6, 10, at(1)).await);
        assert!(!ledger.reserve("p", 6, 10, at(1)).await);
        assert!(ledger.reserve("p", 4, 10, at(1)).await);
    }

    #[tokio::test]
    async fn settling_returns_what_was_not_used() {
        let ledger = MemoryLedger::new();
        assert!(ledger.reserve("p", 10, 10, at(1)).await);
        ledger.settle("p", 10, 3, at(1)).await;
        assert!(ledger.reserve("p", 7, 10, at(1)).await);
        assert!(!ledger.reserve("p", 1, 10, at(1)).await);
    }

    #[tokio::test]
    async fn the_cap_resets_each_day_and_is_per_provider() {
        let ledger = MemoryLedger::new();
        assert!(ledger.reserve("p", 10, 10, at(1)).await);
        assert!(ledger.reserve("q", 10, 10, at(1)).await);
        assert!(ledger.reserve("p", 10, 10, at(2)).await);
    }
}
