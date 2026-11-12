//! What an export reads, and the write an import restores jobs with.

use chrono::NaiveDate;
use serde_json::json;
use store::{JobStatus, Link, SpendDay, Store, StoreError, UnitOfWork, WorkingEntry, WriteOp};

use crate::fixtures::{ITEM, OWNER, OWNS, at, decls, item, job, kinds, note, page, record};

fn day(n: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2027, 1, n).unwrap()
}

fn working(key: &str, expires: Option<i64>) -> WorkingEntry {
    WorkingEntry {
        scope: "run".to_owned(),
        key: key.to_owned(),
        value: json!({ "k": key }),
        expires_at: expires.map(at),
    }
}

/// Everything is listed, in a stable order, a page at a time: notes by id,
/// live working memory by key, records by type then id, links, and spend
/// by provider then day.
pub async fn the_archive_lists_everything_in_order(store: &dyn Store) {
    store.register_records(&decls()).await.unwrap();
    let (first, second) = (note("one"), note("two"));
    let owner = record(OWNER, json!({ "name": "o" }));
    let (small, large) = (item("a", 1), item("b", 2));
    let unit = UnitOfWork::new()
        .with(WriteOp::CreateNote(second.clone()))
        .with(WriteOp::CreateNote(first.clone()))
        .with(WriteOp::PutWorking(working("live", Some(100))))
        .with(WriteOp::PutWorking(working("gone", Some(5))))
        .with(WriteOp::PutWorking(working("kept", None)))
        .with(WriteOp::PutRecord(large.clone()))
        .with(WriteOp::PutRecord(owner.clone()))
        .with(WriteOp::PutRecord(small.clone()))
        .with(WriteOp::Link {
            relation: OWNS.to_owned(),
            from: owner.id,
            to: small.id,
        });
    store.apply(unit).await.unwrap();
    assert!(store.reserve_spend("q", day(2), 7, 100).await.unwrap());
    assert!(store.reserve_spend("p", day(3), 5, 100).await.unwrap());
    assert!(store.reserve_spend("p", day(1), 3, 100).await.unwrap());

    let notes = store.list_notes(page(10)).await.unwrap();
    let mut by_id = [first.id, second.id];
    by_id.sort();
    let ids: Vec<_> = notes.items.iter().map(|listed| listed.id).collect();
    assert_eq!(ids, by_id);
    assert_eq!(
        notes.items.iter().find(|n| n.id == first.id).unwrap().body,
        "one"
    );
    assert!(store.list_notes(page(1)).await.unwrap().has_more);

    let live = store.list_working(at(10), page(10)).await.unwrap();
    let keys: Vec<_> = live.items.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, ["kept", "live"], "an expired entry isn't listed");
    assert_eq!(live.items[1], working("live", Some(100)));

    let records = store.list_all_records(page(10)).await.unwrap();
    let mut items = [small.id, large.id];
    items.sort();
    let listed: Vec<_> = records
        .items
        .iter()
        .map(|found| (found.record_type.as_str(), found.id))
        .collect();
    assert_eq!(
        listed,
        [(ITEM, items[0]), (ITEM, items[1]), (OWNER, owner.id)]
    );
    assert_eq!(
        records.items.iter().find(|r| r.id == small.id),
        Some(&small)
    );

    let links = store.list_links(page(10)).await.unwrap();
    assert_eq!(
        links.items,
        [Link {
            relation: OWNS.to_owned(),
            from: owner.id,
            to: small.id
        }]
    );

    let spend = store.list_spend(page(10)).await.unwrap();
    let rows: Vec<_> = spend
        .items
        .iter()
        .map(|row| (row.provider.as_str(), row.day, row.usd_micros))
        .collect();
    assert_eq!(rows, [("p", day(1), 3), ("p", day(3), 5), ("q", day(2), 7)]);
    let one = store.list_spend(page(1)).await.unwrap();
    assert!(one.has_more);
    assert_eq!(
        one.items,
        [SpendDay {
            provider: "p".to_owned(),
            day: day(1),
            usd_micros: 3
        }]
    );
}

/// A restored job is kept exactly as it was, finished time included, so an
/// import never runs finished work again, and retention still prunes it.
pub async fn a_restored_job_keeps_how_it_ended(store: &dyn Store) {
    let mut done = job(0, 1);
    done.status = JobStatus::Succeeded;
    done.attempts = 1;
    done.result = Some(json!({ "answer": 42 }));
    done.finished_at = Some(at(5));
    store
        .apply(WriteOp::RestoreJob(done.clone()).into())
        .await
        .unwrap();
    assert_eq!(store.get_job(done.id).await.unwrap(), Some(done.clone()));
    assert!(matches!(
        store.apply(WriteOp::RestoreJob(done.clone()).into()).await,
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        store
            .claim_job("w", &kinds(), at(10), at(20))
            .await
            .unwrap(),
        None,
        "a finished job isn't claimed"
    );
    assert_eq!(store.prune_jobs(at(6), 10).await.unwrap(), 1);
    assert_eq!(store.get_job(done.id).await.unwrap(), None);
}
