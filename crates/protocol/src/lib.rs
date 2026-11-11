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

/// One agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(uuid::Uuid);

impl RunId {
    /// A new, time-ordered id.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::now_v7())
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
