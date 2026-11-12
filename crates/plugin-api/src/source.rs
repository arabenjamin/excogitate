//! Sources: what a plugin offers memory to read. A source holds items
//! somewhere the runtime doesn't, such as a data lake, and answers searches
//! of them. Memory search fuses its hits with the store's by rank.
//!
//! A source only reads. Everything it returns is untrusted, and the runtime
//! drops any item the run may not see, whatever the source says.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use protocol::DataClass;

use crate::{ToolContext, ToolFailure};

/// The most hits one search asks a source for.
pub const MAX_SOURCE_HITS: u32 = 50;

/// The longest item id.
pub const MAX_ITEM_ID: usize = 256;

/// The longest title the runtime keeps. A longer one is cut.
pub const MAX_ITEM_TITLE: usize = 256;

/// The longest text a search hit keeps. A longer one is cut, and `get` has
/// the whole of it.
pub const MAX_HIT_TEXT: usize = 8000;

/// The longest text an item read whole keeps. A longer one is cut, and
/// says so.
pub const MAX_ITEM_TEXT: usize = 100_000;

/// The longest URL the runtime keeps. A longer one is left out.
pub const MAX_ITEM_URL: usize = 2048;

/// A search of a source.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SourceQuery {
    /// What to look for, in words.
    pub text: String,
    /// The query's vector, sent only when it was made by a model the
    /// source declares, at its dimension. A source searched without one
    /// searches by words.
    pub vector: Option<Vec<f32>>,
    /// The most hits wanted, at most [`MAX_SOURCE_HITS`]. More are dropped.
    pub limit: u32,
}

impl SourceQuery {
    /// A search for `text`, by words, for at most `limit` hits.
    #[must_use]
    pub fn new(text: impl Into<String>, limit: u32) -> Self {
        Self {
            text: text.into(),
            vector: None,
            limit: limit.min(MAX_SOURCE_HITS),
        }
    }

    /// The same search, with the query's vector.
    #[must_use]
    pub fn with_vector(mut self, vector: Vec<f32>) -> Self {
        self.vector = Some(vector);
        self
    }
}

/// One item of a source: a search hit, or an item read whole.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SourceItem {
    /// Its id within the source, at most [`MAX_ITEM_ID`] characters.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Its text: for a hit, the part that matched; read whole, all of it.
    pub text: String,
    /// Its privacy class. The runtime trusts it only from a source declared
    /// public; anything else is private.
    pub data_class: DataClass,
    /// Where it came from, if it has an address.
    pub source_url: Option<String>,
    /// When it last changed, if the source knows.
    pub updated_at: Option<DateTime<Utc>>,
    /// How close it is in meaning to the query, from -1 to 1, if it was
    /// found with the query's vector. Kept only when the vector was sent.
    pub similarity: Option<f32>,
}

impl SourceItem {
    /// An item, private until said otherwise.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            text: text.into(),
            data_class: DataClass::Private,
            source_url: None,
            updated_at: None,
            similarity: None,
        }
    }

    /// The same item, with its privacy class.
    #[must_use]
    pub const fn with_data_class(mut self, data_class: DataClass) -> Self {
        self.data_class = data_class;
        self
    }

    /// The same item, with where it came from.
    #[must_use]
    pub fn with_source_url(mut self, url: impl Into<String>) -> Self {
        self.source_url = Some(url.into());
        self
    }

    /// The same item, with when it last changed.
    #[must_use]
    pub const fn with_updated_at(mut self, at: DateTime<Utc>) -> Self {
        self.updated_at = Some(at);
        self
    }

    /// The same item, with how close it is to the query's vector.
    #[must_use]
    pub const fn with_similarity(mut self, similarity: f32) -> Self {
        self.similarity = Some(similarity);
        self
    }
}

/// What a search of a source found.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct SourceFound {
    /// The hits, best first. Their order is their rank; no score is
    /// compared across sources.
    pub hits: Vec<SourceItem>,
    /// What the search cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
}

impl SourceFound {
    /// These hits, which cost nothing.
    #[must_use]
    pub const fn new(hits: Vec<SourceItem>) -> Self {
        Self {
            hits,
            cost_usd_micros: 0,
        }
    }

    /// The same hits, with what finding them cost.
    #[must_use]
    pub const fn with_cost(mut self, cost_usd_micros: u64) -> Self {
        self.cost_usd_micros = cost_usd_micros;
        self
    }
}

/// One item read whole, or none, and what reading it cost.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct SourceGot {
    /// The item, with the id asked for, or `None` if there's no such item.
    pub item: Option<SourceItem>,
    /// What reading it cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
}

impl SourceGot {
    /// This item, or none, which cost nothing.
    #[must_use]
    pub const fn new(item: Option<SourceItem>) -> Self {
        Self {
            item,
            cost_usd_micros: 0,
        }
    }

    /// The same, with what reading it cost.
    #[must_use]
    pub const fn with_cost(mut self, cost_usd_micros: u64) -> Self {
        self.cost_usd_micros = cost_usd_micros;
        self
    }
}

/// Something memory reads, offered by a plugin and declared in its
/// manifest's `sources`.
#[async_trait]
pub trait Source: Send + Sync + std::fmt::Debug {
    /// Its name, as the manifest declares it.
    fn name(&self) -> &str;

    /// Searches it. `context.data_class` is the run's: a public run may see
    /// only public items, and the runtime drops any other.
    ///
    /// # Errors
    ///
    /// [`ToolFailure`] if it couldn't search. Memory search goes on without
    /// it and says so.
    async fn search(
        &self,
        query: SourceQuery,
        context: ToolContext,
    ) -> Result<SourceFound, ToolFailure>;

    /// One item, whole, or `None` if there's no such item, with what
    /// reading it cost. An item whose id isn't `id` is taken as none.
    ///
    /// # Errors
    ///
    /// [`ToolFailure`] if it couldn't read it.
    async fn get(&self, id: &str, context: ToolContext) -> Result<SourceGot, ToolFailure>;
}
