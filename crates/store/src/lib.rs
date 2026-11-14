//! The store port. Typed traits in the runtime's own types, only for the kinds
//! the core uses, and one generic kind ([`Record`]) for plugin data.
//!
//! Three rules shape it:
//!
//! - **No query string crosses this port.** Reads take typed filters.
//! - **Every write is a [`UnitOfWork`]**, applied whole or not at all. It's
//!   data rather than a closure, so a store with one writer task can take it
//!   as a message, and no transaction stays open across an `await`.
//! - **What a store means is defined by its conformance suite**, the
//!   `store-conformance` crate, not by these signatures alone.

mod archive;
mod catalog;
mod error;
mod ids;
mod kinds;
mod page;
mod port;
mod record;
mod unit;

pub use archive::{Archive, Link, SpendDay};
pub use catalog::RecordCatalog;
pub use error::StoreError;
pub use ids::{ChunkId, JobId, NoteId, RecordId, TaskId};
pub use kinds::{
    Chunk, ChunkHit, ClassFilter, Definition, DefinitionKey, DefinitionKind, Job, JobStatus, Lease,
    Note, NotesStored, Provenance, StoredWindow, Task, TaskQuery, TaskStatus, WorkingEntry,
};
pub use page::{MAX_PAGE_LIMIT, Page, PageRequest};
pub use port::{DefinitionReader, JobQueue, NoteReader, RecordReader, SpendLog, Store, TaskReader};
pub use record::{
    Direction, FieldFilter, FieldKind, FieldOp, FieldValue, FilterableField, Record, RecordDecls,
    RecordQuery, RecordTypeDecl, RelationDecl,
};
pub use unit::{UnitOfWork, WriteOp};
