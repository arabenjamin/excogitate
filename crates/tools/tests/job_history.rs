//! The `job_history` tool, over an in-memory store.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::sync::Arc;

use chrono::{Duration, Utc};
use plugin_api::{Effect, Tool, ToolContext};
use protocol::{Actor, DataClass, RunId, Trust};
use schema::Validator;
use serde_json::{Value, json};
use store::{Job, JobId, JobStatus, Store, UnitOfWork, WriteOp};
use store_memory::MemoryStore;
use tools::JobHistoryTool;

fn job(workflow: &str, hours_ago: i64, status: JobStatus, error: Option<&str>) -> Job {
    let run_at = Utc::now() - Duration::hours(hours_ago);
    let finished = matches!(status, JobStatus::Succeeded | JobStatus::Dead);
    Job {
        id: JobId::generate(),
        kind: "workflow".to_owned(),
        payload: json!({ "workflow": workflow, "inputs": { "secret": "the owner's résumé" } }),
        priority: 0,
        status,
        requested_by: Actor::new("schedule:daily"),
        run_at,
        attempts: u32::from(finished),
        max_attempts: 1,
        lease: None,
        last_error: error.map(str::to_owned),
        result: finished.then(|| json!({ "found": "private things" })),
        finished_at: finished.then_some(run_at + Duration::minutes(2)),
    }
}

async fn tool() -> JobHistoryTool {
    let store = Arc::new(MemoryStore::new(3));
    let mut work = UnitOfWork::new();
    for made in [
        job(
            "niche-scan",
            30,
            JobStatus::Dead,
            Some("the search engine refused"),
        ),
        job("media-watch", 3, JobStatus::Succeeded, None),
        job("squares-report", 1, JobStatus::Succeeded, None),
    ] {
        work = work.with(WriteOp::RestoreJob(made));
    }
    store.apply(work).await.unwrap();
    JobHistoryTool::new(store as Arc<dyn Store>)
}

async fn call(tool: &JobHistoryTool, args: Value, data_class: DataClass) -> (Value, Trust) {
    let definition = tool.definition();
    Validator::from_schema(&definition.input_schema)
        .unwrap()
        .check(&args)
        .unwrap();
    let output = tool
        .run(args, ToolContext::new(RunId::generate(), data_class, 1000))
        .await
        .unwrap();
    Validator::from_schema(&definition.output_schema)
        .unwrap()
        .check(&output.value)
        .unwrap();
    (output.value, output.trust)
}

#[tokio::test]
async fn it_lists_the_latest_first_and_filters_by_status() {
    let tool = tool().await;
    assert_eq!(tool.definition().effect, Effect::Read);
    let (all, trust) = call(&tool, json!({ "limit": 2 }), DataClass::Private).await;
    assert_eq!(trust, Trust::Untrusted);
    assert_eq!(all["has_more"], true);
    let names: Vec<&str> = all["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["workflow"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["squares-report", "media-watch"]);
    assert_eq!(all["jobs"][0]["status"], "succeeded");
    assert_eq!(all["jobs"][0]["requested_by"], "schedule:daily");

    let (dead, _) = call(&tool, json!({ "status": "dead" }), DataClass::Private).await;
    let dead = dead["jobs"].as_array().unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0]["workflow"], "niche-scan");
    assert_eq!(dead[0]["last_error"], "the search engine refused");
}

#[tokio::test]
async fn it_never_shows_inputs_or_results_and_a_public_run_no_errors() {
    let tool = tool().await;
    let (private, _) = call(&tool, json!({}), DataClass::Private).await;
    let text = private.to_string();
    assert!(
        !text.contains("résumé") && !text.contains("private things"),
        "{text}"
    );
    let (public, _) = call(&tool, json!({ "status": "dead" }), DataClass::Public).await;
    assert_eq!(public["jobs"][0]["last_error"], Value::Null);
    assert_eq!(public["jobs"][0]["workflow"], "niche-scan");
}
