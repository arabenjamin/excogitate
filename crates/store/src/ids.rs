//! Ids, one type per kind, so that one can't be passed as another.

use std::fmt;

use uuid::Uuid;

macro_rules! id_type {
    ($(#[$doc:meta] $name:ident),+ $(,)?) => {$(
        #[$doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// A new, time-ordered id.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// This id from a stored or parsed value.
            #[must_use]
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// The value of this id.
            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    )+};
}

id_type! {
    /// Identifies a note.
    NoteId,
    /// Identifies a chunk of a note.
    ChunkId,
    /// Identifies a task.
    TaskId,
    /// Identifies a job.
    JobId,
    /// Identifies a plugin's record.
    RecordId,
}
