//! The unit of work: every write, as data.

use crate::{
    Chunk, Definition, DefinitionKey, Job, Note, NoteId, Record, RecordId, Task, TaskId,
    WorkingEntry,
};

/// One write. A closed list, so a store can't miss one.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOp {
    /// Adds a note. Its id must be new; the store sets its version to 1.
    CreateNote(Note),
    /// Replaces a note, if its stored version is `expected_version`.
    UpdateNote {
        /// The new content. Its `version` is ignored.
        note: Note,
        /// The version the caller read.
        expected_version: u64,
    },
    /// Removes a note and its chunks.
    DeleteNote(NoteId),
    /// Replaces all of a note's chunks. Search must follow at once.
    ReplaceChunks {
        /// The note, which must exist.
        note_id: NoteId,
        /// The new chunks, which all belong to it.
        chunks: Vec<Chunk>,
    },
    /// Adds or replaces a task.
    PutTask(Task),
    /// Removes a task.
    DeleteTask(TaskId),
    /// Adds or replaces a working-memory entry.
    PutWorking(WorkingEntry),
    /// Removes a working-memory entry, if it's there.
    DeleteWorking {
        /// Its scope.
        scope: String,
        /// Its key.
        key: String,
    },
    /// Adds a job. Its id must be new.
    EnqueueJob(Job),
    /// Adds or replaces a definition.
    PutDefinition(Definition),
    /// Removes a definition, if it's there.
    DeleteDefinition(DefinitionKey),
    /// Adds or replaces a record, which must meet its type's schema.
    PutRecord(Record),
    /// Removes a record and its links.
    DeleteRecord {
        /// Its type.
        record_type: String,
        /// Its id.
        id: RecordId,
    },
    /// Links two existing records by a declared relation.
    Link {
        /// The relation.
        relation: String,
        /// The record it links from.
        from: RecordId,
        /// The record it links to.
        to: RecordId,
    },
    /// Removes a link, if it's there.
    Unlink {
        /// The relation.
        relation: String,
        /// The record it links from.
        from: RecordId,
        /// The record it links to.
        to: RecordId,
    },
}

/// Writes applied together: all of them, or none.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UnitOfWork {
    /// The writes, applied in order.
    pub ops: Vec<WriteOp>,
}

impl UnitOfWork {
    /// An empty unit.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a write.
    #[must_use]
    pub fn with(mut self, op: WriteOp) -> Self {
        self.ops.push(op);
        self
    }
}

impl From<WriteOp> for UnitOfWork {
    fn from(op: WriteOp) -> Self {
        Self { ops: vec![op] }
    }
}
