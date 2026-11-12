//! Memory reading plugins' sources beside its own notes.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
#![allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use memory::{
    ChunkConfig, EmbedError, EmbedScope, Embedder, Embedding, Found, LakeSearch, LakeSource,
    Memory, MemoryError, NewNote, NoteOrigin, SourceMeter,
};
use plugin_api::{
    CapabilityUse, MAX_ITEM_TEXT, PluginCapability, Source, SourceFound, SourceItem, SourceQuery,
    ToolContext, ToolFailure,
};
use protocol::{Actor, DataClass, RunId, Trust};
use store::ClassFilter;
use store_memory::MemoryStore;

/// Embeds everything as one vector, made by the model `emb`.
#[derive(Debug)]
struct Emb;

#[async_trait]
impl Embedder for Emb {
    fn dimension(&self) -> usize {
        3
    }

    async fn embed(&self, inputs: &[String], _: EmbedScope) -> Result<Embedding, EmbedError> {
        Ok(Embedding {
            vectors: inputs.iter().map(|_| vec![1.0, 0.0, 0.0]).collect(),
            cost_usd_micros: 2,
            model: Some("emb".to_owned()),
        })
    }
}

/// A source that answers with what it's given, and records what it's asked.
#[derive(Debug, Default)]
struct Lake {
    name: &'static str,
    hits: Vec<SourceItem>,
    fail: Option<&'static str>,
    delay: Duration,
    cost: u64,
    asked: Mutex<Vec<SourceQuery>>,
}

#[async_trait]
impl Source for Lake {
    fn name(&self) -> &str {
        self.name
    }

    async fn search(&self, query: SourceQuery, _: ToolContext) -> Result<SourceFound, ToolFailure> {
        self.asked.lock().unwrap().push(query);
        tokio::time::sleep(self.delay).await;
        match self.fail {
            Some(why) => Err(ToolFailure::new(why).with_cost(self.cost)),
            None => Ok(SourceFound::new(self.hits.clone()).with_cost(self.cost)),
        }
    }

    async fn get(&self, id: &str, _: ToolContext) -> Result<SourceFound, ToolFailure> {
        let found = self.hits.iter().filter(|item| item.id == id).cloned();
        Ok(SourceFound::new(found.collect()).with_cost(self.cost))
    }
}

fn item(id: &str, text: &str) -> SourceItem {
    SourceItem::new(id, id.to_uppercase(), text)
}

fn lake(source: &Arc<Lake>, public: bool) -> LakeSource {
    LakeSource {
        id: format!("demo.{}", source.name),
        public,
        words: true,
        vectors_from: BTreeSet::new(),
        dimension: None,
        time_limit: Duration::from_secs(5),
        source: Arc::clone(source) as Arc<dyn Source>,
        meter: None,
    }
}

const SCOPE: EmbedScope = EmbedScope {
    data_class: DataClass::Private,
    budget_usd_micros: None,
};

async fn memory(sources: Vec<LakeSource>) -> Memory {
    let memory = Memory::new(
        Arc::new(MemoryStore::new(3)),
        Some(Arc::new(Emb)),
        ChunkConfig::default(),
    );
    let note = NewNote {
        title: "Cats".to_owned(),
        body: "cats sleep most of the day".to_owned(),
        tags: Vec::new(),
        source_url: None,
    };
    let origin = NoteOrigin {
        data_class: DataClass::Private,
        trust: Trust::Trusted,
        stored_by: Actor::new("owner"),
    };
    memory.store(note, origin, None, Utc::now()).await.unwrap();
    memory.attach_sources(sources).unwrap();
    memory
}

fn context(data_class: DataClass) -> ToolContext {
    ToolContext::new(RunId::generate(), data_class, 1000)
}

fn everything(data_class: DataClass) -> LakeSearch<'static> {
    LakeSearch {
        context: context(data_class),
        chosen: None,
    }
}

const fn classes(data_class: DataClass) -> ClassFilter {
    match data_class {
        DataClass::Private => ClassFilter::Any,
        DataClass::Public => ClassFilter::PublicOnly,
    }
}

async fn search(
    memory: &Memory,
    data_class: DataClass,
    lake: LakeSearch<'_>,
) -> memory::LakeResults {
    memory
        .search_lake("cats", classes(data_class), SCOPE, 10, None, Some(lake))
        .await
        .unwrap()
}

fn items(found: &memory::LakeResults) -> Vec<(String, String, DataClass)> {
    found
        .hits
        .iter()
        .filter_map(|hit| match hit {
            Found::Item(item) => Some((item.source.clone(), item.id.clone(), item.data_class)),
            Found::Note(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn items_are_ranked_with_notes_and_say_where_they_came_from() {
    let source = Arc::new(Lake {
        name: "lake",
        hits: vec![item("a", "cats purr"), item("b", "cats climb")],
        cost: 7,
        ..Lake::default()
    });
    let memory = memory(vec![lake(&source, false)]).await;

    let found = search(&memory, DataClass::Private, everything(DataClass::Private)).await;

    let notes = found
        .hits
        .iter()
        .filter(|hit| matches!(hit, Found::Note(_)))
        .count();
    assert_eq!(notes, 1);
    assert_eq!(
        items(&found),
        [
            ("demo.lake".to_owned(), "a".to_owned(), DataClass::Private),
            ("demo.lake".to_owned(), "b".to_owned(), DataClass::Private),
        ]
    );
    assert_eq!(found.cost_usd_micros, 9, "the embedding and the source");
    assert_eq!(found.sources_skipped, []);
    let scores: Vec<f32> = found
        .hits
        .iter()
        .map(|hit| match hit {
            Found::Note(note) => note.score,
            Found::Item(item) => item.score,
        })
        .collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "{scores:?}"
    );
}

#[tokio::test]
async fn an_item_from_a_source_not_declared_public_is_private() {
    let claims_public = item("a", "cats").with_data_class(DataClass::Public);
    let source = Arc::new(Lake {
        name: "lake",
        hits: vec![claims_public],
        ..Lake::default()
    });
    let memory = memory(vec![lake(&source, false)]).await;

    let public = search(&memory, DataClass::Public, everything(DataClass::Public)).await;
    assert_eq!(items(&public), [], "a public run sees none of it");
    let private = search(&memory, DataClass::Private, everything(DataClass::Private)).await;
    assert_eq!(items(&private)[0].2, DataClass::Private);
}

#[tokio::test]
async fn a_public_source_keeps_each_items_class() {
    let source = Arc::new(Lake {
        name: "lake",
        hits: vec![
            item("open", "cats").with_data_class(DataClass::Public),
            item("closed", "cats"),
        ],
        ..Lake::default()
    });
    let memory = memory(vec![lake(&source, true)]).await;

    let public = search(&memory, DataClass::Public, everything(DataClass::Public)).await;

    assert_eq!(
        items(&public),
        [("demo.lake".to_owned(), "open".to_owned(), DataClass::Public)]
    );
}

#[tokio::test]
async fn a_source_that_fails_or_is_slow_is_skipped_and_named() {
    let broken = Arc::new(Lake {
        name: "broken",
        fail: Some("the lake is down"),
        cost: 3,
        ..Lake::default()
    });
    let slow = Arc::new(Lake {
        name: "slow",
        hits: vec![item("late", "cats")],
        delay: Duration::from_secs(5),
        ..Lake::default()
    });
    let mut slow_lake = lake(&slow, false);
    slow_lake.time_limit = Duration::from_millis(50);
    let memory = memory(vec![lake(&broken, false), slow_lake]).await;

    let found = search(&memory, DataClass::Private, everything(DataClass::Private)).await;

    let skipped: Vec<(&str, &str)> = found
        .sources_skipped
        .iter()
        .map(|skipped| (skipped.source.as_str(), skipped.why.as_str()))
        .collect();
    assert_eq!(
        skipped,
        [
            ("demo.broken", "the lake is down"),
            ("demo.slow", "it took longer than 50 ms"),
        ]
    );
    assert_eq!(found.hits.len(), 1, "the note is still found");
    assert_eq!(found.cost_usd_micros, 5, "a failure's cost still counts");
}

#[tokio::test]
async fn the_query_vector_reaches_only_a_source_that_takes_it() {
    let takes = Arc::new(Lake {
        name: "takes",
        hits: vec![item("near", "cats").with_similarity(0.9)],
        ..Lake::default()
    });
    let words = Arc::new(Lake {
        name: "words",
        hits: vec![item("claims", "cats").with_similarity(0.99)],
        ..Lake::default()
    });
    let vectors_only = Arc::new(Lake {
        name: "elsewhere",
        ..Lake::default()
    });
    let mut taking = lake(&takes, false);
    taking.vectors_from = BTreeSet::from(["emb".to_owned()]);
    taking.dimension = Some(3);
    let mut elsewhere = lake(&vectors_only, false);
    elsewhere.words = false;
    elsewhere.vectors_from = BTreeSet::from(["other".to_owned()]);
    elsewhere.dimension = Some(3);
    let memory = memory(vec![taking, lake(&words, false), elsewhere]).await;

    let found = memory
        .search_lake(
            "cats",
            ClassFilter::Any,
            SCOPE,
            10,
            Some(0.5),
            Some(everything(DataClass::Private)),
        )
        .await
        .unwrap();

    assert_eq!(
        takes.asked.lock().unwrap()[0].vector,
        Some(vec![1.0, 0.0, 0.0])
    );
    assert_eq!(words.asked.lock().unwrap()[0].vector, None);
    assert_eq!(vectors_only.asked.lock().unwrap().len(), 0, "never called");
    assert_eq!(found.sources_skipped.len(), 1);
    assert_eq!(found.sources_skipped[0].source, "demo.elsewhere");
    // A similarity only counts from a source sent the vector; the note was
    // found by vector too.
    let kept: Vec<String> = items(&found).into_iter().map(|(_, id, _)| id).collect();
    assert_eq!(kept, ["near"]);
    assert_eq!(found.not_close_enough, 1);
}

#[tokio::test]
async fn a_run_reads_only_the_sources_chosen_for_it() {
    let chosen = Arc::new(Lake {
        name: "chosen",
        hits: vec![item("a", "cats")],
        ..Lake::default()
    });
    let other = Arc::new(Lake {
        name: "other",
        hits: vec![item("b", "cats")],
        ..Lake::default()
    });
    let memory = memory(vec![lake(&chosen, false), lake(&other, false)]).await;
    let names = ["demo.chosen".to_owned()];
    let narrow = LakeSearch {
        context: context(DataClass::Private),
        chosen: Some(&names),
    };

    let found = search(&memory, DataClass::Private, narrow).await;

    assert_eq!(items(&found).len(), 1);
    assert_eq!(other.asked.lock().unwrap().len(), 0);
    let none = LakeSearch {
        context: context(DataClass::Private),
        chosen: Some(&[]),
    };
    assert_eq!(items(&search(&memory, DataClass::Private, none).await), []);
}

#[tokio::test]
async fn an_item_is_opened_whole_if_the_run_may_see_it() {
    let long = "x".repeat(MAX_ITEM_TEXT + 10);
    let source = Arc::new(Lake {
        name: "lake",
        hits: vec![item("long", &long), item("short", "cats")],
        cost: 4,
        ..Lake::default()
    });
    let memory = memory(vec![lake(&source, false)]).await;

    let opened = memory
        .open(
            "demo.lake",
            "long",
            ClassFilter::Any,
            everything(DataClass::Private),
        )
        .await
        .unwrap();
    assert!(opened.truncated);
    assert_eq!(opened.item.unwrap().text.chars().count(), MAX_ITEM_TEXT);
    assert_eq!(opened.cost_usd_micros, 4);

    let hidden = memory
        .open(
            "demo.lake",
            "short",
            ClassFilter::PublicOnly,
            everything(DataClass::Public),
        )
        .await
        .unwrap();
    assert_eq!(hidden.item, None, "a private item, to a public run");

    let names: [String; 0] = [];
    let not_chosen = LakeSearch {
        context: context(DataClass::Private),
        chosen: Some(&names),
    };
    let refused = memory
        .open("demo.lake", "short", ClassFilter::Any, not_chosen)
        .await
        .unwrap_err();
    assert!(matches!(refused, MemoryError::Invalid(_)), "{refused}");
}

#[tokio::test]
async fn sources_are_attached_once_with_ids_of_their_own() {
    let one = Arc::new(Lake {
        name: "one",
        ..Lake::default()
    });
    let memory = Memory::new(Arc::new(MemoryStore::new(3)), None, ChunkConfig::default());
    assert!(
        memory
            .attach_sources(vec![lake(&one, false), lake(&one, false)])
            .is_err()
    );
    memory.attach_sources(vec![lake(&one, false)]).unwrap();
    assert!(memory.attach_sources(Vec::new()).is_err());
    assert_eq!(memory.source_ids(), ["demo.one"]);
}

/// A plugin's meter, shared by its sources: what they spent and did.
#[derive(Debug, Default)]
struct Meter(Mutex<(u64, Vec<CapabilityUse>)>);

impl SourceMeter for Meter {
    fn take(&self, _: RunId) -> (u64, Vec<CapabilityUse>) {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// A source whose plugin makes a model call costing `cost` for each search,
/// recorded on the plugin's meter, not reported by the source.
#[derive(Debug)]
struct Spender {
    name: &'static str,
    cost: u64,
    delay: Duration,
    meter: Arc<Meter>,
    saw: Mutex<Vec<(u64, u32, bool)>>,
}

#[async_trait]
impl Source for Spender {
    fn name(&self) -> &str {
        self.name
    }

    async fn search(
        &self,
        _: SourceQuery,
        context: ToolContext,
    ) -> Result<SourceFound, ToolFailure> {
        self.saw.lock().unwrap().push((
            context.budget_usd_micros,
            context.tool_calls_left,
            context.reading,
        ));
        {
            let mut tally = self.meter.0.lock().unwrap();
            tally.0 += self.cost;
            let used = CapabilityUse::new(PluginCapability::Model, "a call", true);
            tally.1.push(used.with_cost(self.cost));
        }
        tokio::time::sleep(self.delay).await;
        Ok(SourceFound::new(vec![item(self.name, "cats")]))
    }

    async fn get(&self, _: &str, _: ToolContext) -> Result<SourceFound, ToolFailure> {
        Ok(SourceFound::default())
    }
}

#[tokio::test]
async fn sources_spend_in_turn_and_what_their_plugins_did_goes_on_the_search() {
    let meter = Arc::new(Meter::default());
    let spender = |name, cost, delay| {
        Arc::new(Spender {
            name,
            cost,
            delay,
            meter: Arc::clone(&meter),
            saw: Mutex::new(Vec::new()),
        })
    };
    let (first, second) = (
        spender("first", 100, Duration::ZERO),
        spender("second", 30, Duration::ZERO),
    );
    let slow = spender("slow", 7, Duration::from_secs(5));
    let lakes: Vec<LakeSource> = [&first, &second, &slow]
        .into_iter()
        .map(|source| LakeSource {
            id: format!("demo.{}", source.name),
            public: false,
            words: true,
            vectors_from: BTreeSet::new(),
            dimension: None,
            time_limit: Duration::from_millis(50),
            source: Arc::clone(source) as Arc<dyn Source>,
            meter: Some(Arc::clone(&meter) as Arc<dyn SourceMeter>),
        })
        .collect();
    let memory = memory(lakes).await;
    let lake = LakeSearch {
        context: context(DataClass::Private).with_tool_calls_left(10),
        chosen: None,
    };

    let found = search(&memory, DataClass::Private, lake).await;

    // Each is told what's left after the embedding (2) and the ones before
    // it, as a read.
    assert_eq!(*first.saw.lock().unwrap(), [(998, 10, true)]);
    assert_eq!(*second.saw.lock().unwrap(), [(898, 9, true)]);
    assert_eq!(*slow.saw.lock().unwrap(), [(868, 8, true)]);
    // The one cut off for time is still charged for what it did.
    assert_eq!(found.cost_usd_micros, 2 + 100 + 30 + 7);
    assert_eq!(found.uses.len(), 3);
    assert_eq!(found.sources_skipped.len(), 1);
}
