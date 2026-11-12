//! Schedules: when they fire, and how their files reach the store.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use store::{Definition, DefinitionKey, DefinitionKind, PageRequest, Store, WriteOp};
use store_memory::MemoryStore;
use work::{
    DefinitionError, Schedule, Scheduler, StoreTransport, read_definition_files, save_definitions,
};

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2027, 3, 10, hour, minute, 0).unwrap()
}

fn schedule(cron: &str) -> Schedule {
    Schedule {
        cron: cron.to_owned(),
        timezone: None,
        kind: "chat".to_owned(),
        payload: json!({ "message": "hi" }),
        priority: 0,
        max_attempts: 1,
        catch_up_seconds: 3600,
        enabled: true,
    }
}

struct Rig {
    store: Arc<MemoryStore>,
    scheduler: Scheduler,
}

fn rig() -> Rig {
    let store = Arc::new(MemoryStore::new(3));
    let transport = Arc::new(StoreTransport(Arc::clone(&store) as _));
    let scheduler = Scheduler::new(Arc::clone(&store) as _, transport);
    Rig { store, scheduler }
}

async fn put(store: &dyn Store, name: &str, schedule: &Schedule, source: &str) {
    let definition = Definition {
        key: DefinitionKey {
            kind: DefinitionKind::Schedule,
            name: name.to_owned(),
        },
        body: serde_json::to_value(schedule).unwrap(),
        source: source.to_owned(),
        updated_at: at(0, 0),
    };
    store
        .apply(WriteOp::PutDefinition(definition).into())
        .await
        .unwrap();
}

#[test]
fn the_slot_is_the_latest_one_not_past_now() {
    let daily = schedule("0 8 * * *");
    assert_eq!(daily.due_slot("s", at(8, 30)).unwrap(), Some(at(8, 0)));
    assert_eq!(daily.due_slot("s", at(8, 0)).unwrap(), Some(at(8, 0)));
}

#[test]
fn a_slot_too_old_is_skipped() {
    let daily = schedule("0 8 * * *");
    assert_eq!(daily.due_slot("s", at(9, 0)).unwrap(), Some(at(8, 0)));
    assert_eq!(daily.due_slot("s", at(9, 1)).unwrap(), None);
}

#[test]
fn a_schedule_runs_in_its_own_time_zone() {
    let mut daily = schedule("0 8 * * *");
    daily.timezone = Some("America/New_York".to_owned());
    // 8:00 in New York on this date is 13:00 UTC, because DST starts the 14th.
    assert_eq!(daily.due_slot("s", at(13, 5)).unwrap(), Some(at(13, 0)));
    assert_eq!(daily.due_slot("s", at(8, 5)).unwrap(), None);
}

#[test]
fn bad_schedules_are_named() {
    let error = schedule("not cron").due_slot("s", at(1, 0)).unwrap_err();
    assert!(matches!(error, DefinitionError::Cron { .. }), "{error}");
    let mut zoned = schedule("0 8 * * *");
    zoned.timezone = Some("Mars/Olympus".to_owned());
    let error = zoned.due_slot("s", at(1, 0)).unwrap_err();
    assert!(matches!(error, DefinitionError::Zone { .. }), "{error}");
}

#[tokio::test]
async fn it_fires_once_per_slot_however_often_it_ticks() {
    let rig = rig();
    put(rig.store.as_ref(), "daily", &schedule("0 8 * * *"), "t").await;
    assert_eq!(rig.scheduler.tick(at(8, 1)).await.unwrap(), ["daily"]);
    assert_eq!(rig.scheduler.tick(at(8, 2)).await.unwrap(), [] as [&str; 0]);
    assert_eq!(
        rig.scheduler.tick(at(8, 59)).await.unwrap(),
        [] as [&str; 0]
    );
}

#[tokio::test]
async fn a_restart_or_a_second_scheduler_cant_fire_it_twice() {
    let rig = rig();
    put(rig.store.as_ref(), "daily", &schedule("0 8 * * *"), "t").await;
    assert_eq!(rig.scheduler.tick(at(8, 1)).await.unwrap(), ["daily"]);
    let transport = Arc::new(StoreTransport(Arc::clone(&rig.store) as _));
    let second = Scheduler::new(Arc::clone(&rig.store) as _, transport);
    assert_eq!(second.tick(at(8, 1)).await.unwrap(), [] as [&str; 0]);
}

#[tokio::test]
async fn each_slot_fires_and_the_job_says_who_asked() {
    let rig = rig();
    let mut hourly = schedule("0 * * * *");
    hourly.max_attempts = 3;
    hourly.priority = 7;
    put(rig.store.as_ref(), "hourly", &hourly, "t").await;
    assert_eq!(rig.scheduler.tick(at(8, 0)).await.unwrap(), ["hourly"]);
    assert_eq!(rig.scheduler.tick(at(9, 0)).await.unwrap(), ["hourly"]);
    let job = store::JobQueue::claim_job(
        rig.store.as_ref(),
        "w",
        &["chat".to_owned()],
        at(9, 1),
        at(10, 0),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(job.requested_by.as_str(), "schedule:hourly");
    assert_eq!(job.max_attempts, 3);
    assert_eq!(job.priority, 7);
    assert_eq!(job.payload, json!({ "message": "hi" }));
}

#[tokio::test]
async fn a_disabled_schedule_does_not_fire() {
    let rig = rig();
    let mut off = schedule("0 8 * * *");
    off.enabled = false;
    put(rig.store.as_ref(), "off", &off, "t").await;
    assert_eq!(rig.scheduler.tick(at(8, 1)).await.unwrap(), [] as [&str; 0]);
}

#[tokio::test]
async fn one_bad_schedule_does_not_stop_the_others() {
    let rig = rig();
    put(rig.store.as_ref(), "a_bad", &schedule("not cron"), "t").await;
    put(rig.store.as_ref(), "good", &schedule("0 8 * * *"), "t").await;
    assert_eq!(rig.scheduler.tick(at(8, 1)).await.unwrap(), ["good"]);
}

/// A directory that's gone when the test is.
struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("sched-test-{label}-{}", uuid_like()));
        std::fs::create_dir_all(path.join("schedules")).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.0.join("schedules").join(name), text).unwrap();
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.0.join("schedules").join(name)).unwrap();
    }

    fn root(&self) -> &std::path::Path {
        &self.0
    }
}

fn payloads() -> BTreeMap<String, Value> {
    let payload = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["message"],
        "properties": { "message": { "type": "string", "maxLength": 100 } }
    });
    BTreeMap::from([("chat".to_owned(), payload)])
}

/// What the runtime does at startup: read, check everything, then save.
async fn sync(store: &dyn Store, root: &std::path::Path) -> Result<usize, DefinitionError> {
    let files = read_definition_files(root, &[(DefinitionKind::Schedule, "schedules")])?;
    for file in &files {
        Schedule::from_file(file, &payloads())?;
    }
    save_definitions(store, &files, &[DefinitionKind::Schedule], at(0, 0)).await
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn uuid_like() -> String {
    format!(
        "{:?}-{}",
        std::thread::current().id(),
        Utc::now().timestamp_nanos_opt().unwrap()
    )
    .replace(['(', ')'], "")
}

const GOOD: &str = "cron: '0 8 * * *'\nkind: chat\npayload: {message: hello}\n";

async fn names(store: &dyn Store) -> Vec<(String, String)> {
    store
        .list_definitions(DefinitionKind::Schedule, PageRequest::new(50, 0).unwrap())
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|d: Definition| (d.key.name, d.source))
        .collect()
}

#[tokio::test]
async fn files_reach_the_store_and_say_where_they_came_from() {
    let dir = Temp::new("load");
    dir.write("morning.yaml", GOOD);
    dir.write("ignored.yaml.disabled", "this isn't read");
    let store = MemoryStore::new(3);
    assert_eq!(sync(&store, dir.root()).await.unwrap(), 1);
    assert_eq!(
        names(&store).await,
        [(
            "morning".to_owned(),
            "file:schedules/morning.yaml".to_owned()
        )]
    );
}

#[tokio::test]
async fn a_removed_file_removes_its_schedule_and_nothing_else() {
    let dir = Temp::new("remove");
    dir.write("morning.yaml", GOOD);
    let store = MemoryStore::new(3);
    sync(&store, dir.root()).await.unwrap();
    put(&store, "from_rest", &schedule("0 8 * * *"), "api").await;
    dir.remove("morning.yaml");
    sync(&store, dir.root()).await.unwrap();
    assert_eq!(
        names(&store).await,
        [("from_rest".to_owned(), "api".to_owned())]
    );
}

#[tokio::test]
async fn no_directory_means_no_schedules() {
    let store = MemoryStore::new(3);
    let root = PathBuf::from("/nonexistent/place");
    assert_eq!(sync(&store, &root).await.unwrap(), 0);
}

async fn refused(text: &str) -> String {
    let dir = Temp::new("bad");
    dir.write("good.yaml", GOOD);
    dir.write("bad.yaml", text);
    let store = MemoryStore::new(3);
    let error = sync(&store, dir.root()).await.unwrap_err();
    assert_eq!(
        names(&store).await,
        [] as [(String, String); 0],
        "nothing written"
    );
    error.to_string()
}

#[tokio::test]
async fn a_bad_file_stops_the_sync_and_says_which() {
    let cases: [(&str, &str); 5] = [
        ("cron: 'nope'\nkind: chat\npayload: {message: a}\n", "cron"),
        ("cron: '0 8 * * *'\nkind: nobody\npayload: {}\n", "nobody"),
        (
            "cron: '0 8 * * *'\nkind: chat\npayload: {wrong: 1}\n",
            "payload",
        ),
        (
            "cron: '0 8 * * *'\nkind: chat\npayload: {message: a}\nsurprise: 1\n",
            "bad.yaml",
        ),
        (
            "cron: '0 8 * * *'\ntimezone: Mars/Base\nkind: chat\npayload: {message: a}\n",
            "Mars",
        ),
    ];
    for (text, expected) in cases {
        let error = refused(text).await;
        assert!(error.contains(expected), "{expected}: {error}");
    }
}

#[tokio::test]
async fn a_file_cant_take_the_name_of_something_from_elsewhere() {
    let dir = Temp::new("homes");
    dir.write("morning.yaml", GOOD);
    let store = MemoryStore::new(3);
    put(&store, "morning", &schedule("0 9 * * *"), "api").await;
    let error = sync(&store, dir.root()).await.unwrap_err();
    assert!(matches!(error, DefinitionError::TwoHomes { .. }), "{error}");
    assert_eq!(
        names(&store).await,
        [("morning".to_owned(), "api".to_owned())]
    );
}

#[test]
fn the_example_file_is_valid() {
    let raw: Value = serde_saphyr::from_str(include_str!(
        "../../../definitions/schedules/morning-brief.yaml.example"
    ))
    .unwrap();
    let parsed: Schedule = serde_json::from_value(raw).unwrap();
    assert_eq!(parsed.kind, "chat");
    assert!(parsed.due_slot("example", at(8, 0)).is_ok());
}

#[tokio::test]
async fn a_catch_up_window_shorter_than_the_tick_is_refused() {
    let dir = Temp::new("catch-up");
    dir.write("morning.yaml", &format!("{GOOD}catch_up_seconds: 0\n"));
    let store = MemoryStore::new(3);
    let error = sync(&store, dir.root()).await.unwrap_err();
    assert!(error.to_string().contains("catch_up_seconds"), "{error}");
}
