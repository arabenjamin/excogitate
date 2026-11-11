//! Work that runs on its own.
//!
//! A [`JobTransport`] moves jobs. The store is the default, and another
//! queue can stand in for it. A [`Worker`] claims jobs and hands each to the
//! [`JobHandler`] for its kind. A [`Scheduler`] enqueues jobs when their
//! schedules come due, and needs no state of its own: a firing's job id comes
//! from the schedule and the time, so firing twice adds one job.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod schedule;
mod transport;
mod worker;

pub use schedule::{DefinitionFiles, Schedule, ScheduleError, Scheduler, sync_schedules};
pub use transport::{Enqueued, JobTransport, StoreTransport};
pub use worker::{JobHandler, Worker, WorkerError, WorkerOptions};
