//! The vocabulary shared by the store and the event sink. Nothing here does
//! any work.

use std::fmt;

/// Who may see a piece of data. Set by where the data came from and by
/// configuration, never by a tool argument, so a model can't widen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum DataClass {
    /// Goes only to providers that keep no logs. The default.
    #[default]
    Private,
    /// Marked public by configuration or by its source.
    Public,
}

/// Whether content may be treated as instructions. Stored with the content,
/// so the label still applies when retrieval brings it back into a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Trust {
    /// Written by the owner or by the runtime's own configuration.
    Trusted,
    /// Fetched from outside, or written by a model.
    Untrusted,
}

/// Who did something: a user, an agent or a part of the system.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Actor(String);

impl Actor {
    /// An actor with this name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The actor's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who a run reads and stores as, for private notes: the same id for the
/// same actor, so a store compares ids and never needs the name. A run with
/// no reader sees everything, as the owner's and a schedule's do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Reader(uuid::Uuid);

impl Reader {
    /// The reader for `actor`.
    #[must_use]
    pub fn of(actor: &str) -> Self {
        let seed = format!("reader|{actor}");
        Self(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            seed.as_bytes(),
        ))
    }

    /// Who a run asked for by `requested_by` reads as: only a person
    /// (`person:<id>`) reads as themselves. The owner, a schedule and the
    /// runtime read everything, so they have no reader.
    #[must_use]
    pub fn for_requester(requested_by: Option<&str>) -> Option<Self> {
        requested_by
            .filter(|actor| actor.starts_with("person:"))
            .map(Self::of)
    }

    /// A reader from a stored id.
    #[must_use]
    pub const fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
    }

    /// The id, as it's stored.
    #[must_use]
    pub const fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl fmt::Display for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Who a run's paid model calls are charged to, beyond the provider's own
/// day cap, and how much they may spend a UTC day across all providers.
/// Only a person pays: the owner, a schedule and the runtime are held by
/// the providers' caps alone, so they have no payer. The cap is read when
/// the run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Payer {
    account: uuid::Uuid,
    day_cap_usd_micros: u64,
}

impl Payer {
    /// What a spend account's name starts with, so it can't be taken for a
    /// provider's.
    pub const PREFIX: &'static str = "payer:";

    /// `actor`, allowed `day_cap_usd_micros` a day.
    #[must_use]
    pub fn new(actor: &str, day_cap_usd_micros: u64) -> Self {
        let seed = format!("payer|{actor}");
        Self {
            account: uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, seed.as_bytes()),
            day_cap_usd_micros,
        }
    }

    /// The spend account `actor`'s calls are charged to.
    #[must_use]
    pub fn account_of(actor: &str) -> String {
        Self::new(actor, 0).account()
    }

    /// The spend account, as the ledger names it: never a provider's name.
    #[must_use]
    pub fn account(&self) -> String {
        format!("{}{}", Self::PREFIX, self.account)
    }

    /// The most the payer may spend a UTC day, in millionths of a dollar.
    #[must_use]
    pub const fn day_cap_usd_micros(&self) -> u64 {
        self.day_cap_usd_micros
    }
}

/// One agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(uuid::Uuid);

impl RunId {
    /// A new, time-ordered id.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// This id from a stored or sent value.
    #[must_use]
    pub const fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
    }

    /// The value of this id.
    #[must_use]
    pub const fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Text that isn't a run id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotARunId;

impl fmt::Display for NotARunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a run id is a UUID")
    }
}

impl std::error::Error for NotARunId {}

impl std::str::FromStr for RunId {
    type Err = NotARunId;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(text).map(Self).map_err(|_| NotARunId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_is_private_unless_marked() {
        assert_eq!(DataClass::default(), DataClass::Private);
    }

    #[test]
    fn a_run_id_reads_back_from_its_text() {
        let run = RunId::generate();
        assert_eq!(run.to_string().parse::<RunId>(), Ok(run));
        assert_eq!("not-a-run".parse::<RunId>(), Err(NotARunId));
    }

    #[test]
    fn run_ids_differ() {
        assert_ne!(RunId::generate(), RunId::generate());
    }
}
