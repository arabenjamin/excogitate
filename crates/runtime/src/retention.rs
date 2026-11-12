//! Retention: the server prunes trajectories and finished jobs past their
//! age, at start and then once an hour. It works in batches, so one sweep
//! never holds the store or the sink for long, and a sweep that couldn't
//! finish picks up on the next.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::watch;

use crate::Runtime;

/// How often the server prunes.
pub(crate) const TICK: Duration = Duration::from_secs(3600);

/// The most of one kind removed in one call.
const BATCH: u32 = 500;

/// The most calls of one kind in one sweep.
const MAX_BATCHES: u32 = 20;

/// What one sweep removed, and what stopped it, if anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Swept {
    /// Whole runs, with their trajectories.
    pub runs: u64,
    /// Finished jobs.
    pub jobs: u64,
    /// What failed. What was removed before a failure stays removed.
    pub problems: Vec<String>,
}

impl Runtime {
    /// Removes what's older than retention keeps, as of `now`: whole runs
    /// by when they started, and jobs by when they finished. A kind kept
    /// forever (`null`) is left alone.
    pub async fn sweep_retention(&self, now: DateTime<Utc>) -> Swept {
        let retention = self.config.retention;
        let mut swept = Swept::default();
        if let Some(days) = retention.trajectory_days {
            let before = now - TimeDelta::days(i64::from(days));
            for _ in 0..MAX_BATCHES {
                match self.sink.prune_runs(before, BATCH).await {
                    Ok(gone) => {
                        swept.runs = swept.runs.saturating_add(u64::from(gone));
                        if gone < BATCH {
                            break;
                        }
                    }
                    Err(error) => {
                        swept.problems.push(format!("trajectories: {error}"));
                        break;
                    }
                }
            }
        }
        if let Some(days) = retention.job_days {
            let before = now - TimeDelta::days(i64::from(days));
            for _ in 0..MAX_BATCHES {
                match self.workflows.store.prune_jobs(before, BATCH).await {
                    Ok(gone) => {
                        swept.jobs = swept.jobs.saturating_add(u64::from(gone));
                        if gone < BATCH {
                            break;
                        }
                    }
                    Err(error) => {
                        swept.problems.push(format!("jobs: {error}"));
                        break;
                    }
                }
            }
        }
        swept
    }
}

/// Sweeps at once, then every `tick`, until told to stop. Says what it
/// removed when it removed anything, and what failed whenever it fails.
pub(crate) async fn keep_pruning(
    runtime: Runtime,
    tick: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let swept = runtime.sweep_retention(Utc::now()).await;
        if swept.runs > 0 || swept.jobs > 0 {
            eprintln!(
                "retention: removed {} runs and {} finished jobs",
                swept.runs, swept.jobs
            );
        }
        for problem in &swept.problems {
            eprintln!("retention failed: {problem}");
        }
        tokio::select! {
            () = tokio::time::sleep(tick) => {}
            _ = shutdown.wait_for(|stopping| *stopping) => return,
        }
    }
}
