//! Reads of notes, search, tasks, working memory and definitions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use neo4rs::query;
use protocol::DataClass;
use store::{
    Chunk, ChunkHit, ClassFilter, Definition, DefinitionKey, DefinitionKind, DefinitionReader,
    Note, NoteId, NoteReader, Page, PageRequest, StoreError, Task, TaskId, TaskQuery, TaskReader,
    WorkingEntry,
};

use crate::codec::{
    self, CHUNK, DEFINITION, HIT_PROVENANCE, TASK, WORKING, at, data_class_name,
    definition_kind_name, get, task_status_name,
};
use crate::setup::VECTOR_INDEX;
use crate::{Neo4jStore, page_clause, page_of};

/// Lucene refuses a query of more clauses than this, its default limit.
const MAX_TERMS: usize = 1024;

/// The fewest nearest chunks asked of the vector index. A database with
/// fewer chunks than this is scanned, so a small one is ranked exactly.
/// Measured (`vector_search_at_size`): at 50,000 random chunks 1,024 wide,
/// asking for 1,000 found 296 of the scan's top 30 in 300, at 89 ms against
/// the scan's 2.2 s; asking for 124 found 199.
const MIN_NEIGHBORS: u64 = 1000;
/// The most asked for one page. A page deeper than this is found by scanning.
const MAX_NEIGHBORS: u64 = 4000;
/// How many times the page's depth is asked for, past the minimum.
const OVERSAMPLE: u64 = 10;
/// How far the index's own score, kept in 32-bit floats, may be from the
/// exact one.
const INDEX_ROUNDING: f64 = 1e-5;

/// The condition a search's class filter adds, for a note bound to `n`.
const fn class_condition(classes: ClassFilter) -> &'static str {
    match classes {
        ClassFilter::Any => "true",
        ClassFilter::PublicOnly => "n.data_class = 'public'",
    }
}

fn hit(row: &neo4rs::Row) -> Result<ChunkHit, StoreError> {
    let score: f64 = get(row, "score")?;
    Ok(ChunkHit {
        chunk: codec::chunk(row, false)?,
        provenance: codec::hit_provenance(row)?,
        // Scores only rank hits within one search, so f32 is plenty.
        #[expect(clippy::cast_possible_truncation, reason = "a score, compared by rank")]
        score: score as f32,
    })
}

#[async_trait]
impl NoteReader for Neo4jStore {
    fn embedding_dimension(&self) -> usize {
        self.dimension
    }

    async fn get_note(&self, id: NoteId) -> Result<Option<Note>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (n:Note {{id: $id}}) RETURN {}",
                    codec::note_columns()
                ))
                .param("id", id.to_string()),
            )
            .await?;
        rows.first().map(codec::note).transpose()
    }

    async fn find_note_by_hash(
        &self,
        hash: &str,
        class: DataClass,
    ) -> Result<Option<Note>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (n:Note {{content_hash: $hash, data_class: $class}}) \
                     RETURN {} ORDER BY n.id LIMIT 1",
                    codec::note_columns()
                ))
                .param("hash", hash)
                .param("class", data_class_name(class)),
            )
            .await?;
        rows.first().map(codec::note).transpose()
    }

    async fn list_chunks(&self, note_id: NoteId) -> Result<Vec<Chunk>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (:Note {{id: $id}})-[:HAS_CHUNK]->(c:Chunk) \
                     RETURN {CHUNK}, c.embedding AS embedding ORDER BY c.ordinal, c.id"
                ))
                .param("id", note_id.to_string()),
            )
            .await?;
        rows.iter().map(|row| codec::chunk(row, true)).collect()
    }

    async fn search_text(
        &self,
        text: &str,
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError> {
        let terms: Vec<String> = codec::terms(text)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if terms.len() > MAX_TERMS {
            return Err(StoreError::Invalid(format!(
                "a text search can have at most {MAX_TERMS} different words, and this has {}",
                terms.len()
            )));
        }
        if terms.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                has_more: false,
            });
        }
        // The terms are letters and digits only, so none of them is Lucene
        // syntax, and the database lowercases them as it did the index.
        let lucene = terms.join(" OR ");
        let rows = self
            .rows(
                query(&format!(
                    "CALL db.index.fulltext.queryNodes('chunk_terms', $lucene) YIELD node AS c, score \
                     MATCH (n:Note)-[:HAS_CHUNK]->(c) WHERE {} \
                     RETURN {CHUNK}, {HIT_PROVENANCE}, score ORDER BY score DESC, c.id {}",
                    class_condition(classes),
                    page_clause(page)
                ))
                .param("lucene", lucene),
            )
            .await?;
        page_of(&rows, page, hit)
    }

    async fn search_vector(
        &self,
        vector: &[f32],
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError> {
        if vector.len() != self.dimension {
            return Err(StoreError::Invalid(format!(
                "query vector must have width {}, got {}",
                self.dimension,
                vector.len()
            )));
        }
        let values: Vec<f64> = vector.iter().copied().map(f64::from).collect();
        let zero = values.iter().all(|value| *value == 0.0);
        if !zero && let Some(found) = self.search_vector_index(&values, classes, page).await? {
            return Ok(found);
        }
        self.search_vector_scan(&values, zero, classes, page).await
    }
}

impl Neo4jStore {
    /// The page from the vector index, if it can give the page the scan
    /// would: it holds every hit on it, no chunk it didn't return could tie
    /// with the last or rank above it, and the last scores above zero (a
    /// chunk whose embedding is all zeros isn't in the index, and the scan
    /// scores it 0). Otherwise `None`, and the caller scans.
    pub(crate) async fn search_vector_index(
        &self,
        values: &[f64],
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Option<Page<ChunkHit>>, StoreError> {
        let offset = usize::try_from(page.offset()).unwrap_or(usize::MAX);
        let need = offset
            .saturating_add(page.limit() as usize)
            .saturating_add(1);
        let mut asked = u64::try_from(need)
            .unwrap_or(u64::MAX)
            .saturating_mul(OVERSAMPLE)
            .max(MIN_NEIGHBORS);
        while asked <= MAX_NEIGHBORS {
            let rows = self
                .rows(
                    query(&format!(
                        "CALL db.index.vector.queryNodes('{VECTOR_INDEX}', $asked, $vector) \
                         YIELD node AS c, score AS near \
                         MATCH (n:Note)-[:HAS_CHUNK]->(c) \
                         RETURN {CHUNK}, {HIT_PROVENANCE}, 2.0 * near - 1.0 AS near, \
                           2.0 * vector.similarity.cosine(c.embedding, $vector) - 1.0 AS score, \
                           {} AS visible \
                         ORDER BY score DESC, c.id",
                        class_condition(classes)
                    ))
                    .param("asked", i64::try_from(asked).unwrap_or(i64::MAX))
                    .param("vector", values.to_vec()),
                )
                .await?;
            let returned = rows.len();
            // Fewer than asked for: the index holds them all, and a small
            // database is scanned instead.
            if u64::try_from(returned).unwrap_or(u64::MAX) < asked {
                return Ok(None);
            }
            // The index picks the chunks; each is scored as the scan scores
            // it, so the two rank alike. The index's lowest says what a
            // chunk it didn't return could score at most.
            let score = |row: &neo4rs::Row| get::<f64>(row, "score");
            let mut lowest = f64::MAX;
            for row in &rows {
                lowest = lowest.min(get::<f64>(row, "near")?);
            }
            let mut visible = Vec::with_capacity(need);
            for row in rows {
                if get::<bool>(&row, "visible")? {
                    visible.push(row);
                    if visible.len() == need {
                        break;
                    }
                }
            }
            if let Some(last) = visible.get(need - 1) {
                let last = score(last)?;
                if last > lowest + INDEX_ROUNDING && last > 0.0 {
                    return page_of(visible.get(offset..).unwrap_or_default(), page, hit).map(Some);
                }
                if last <= 0.0 {
                    return Ok(None);
                }
            }
            asked = asked.saturating_mul(4);
        }
        Ok(None)
    }

    /// Every chunk, scored. Neo4j's cosine is scaled to 0..1 and refuses a
    /// zero vector, so it's mapped back to -1..1 and a zero vector on either
    /// side scores 0: no direction, no similarity.
    pub(crate) async fn search_vector_scan(
        &self,
        values: &[f64],
        zero: bool,
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (n:Note)-[:HAS_CHUNK]->(c:Chunk) \
                     WHERE c.embedding IS NOT NULL AND {} \
                     WITH n, c, CASE WHEN $zero OR c.norm = 0.0 THEN 0.0 \
                       ELSE 2.0 * vector.similarity.cosine(c.embedding, $vector) - 1.0 END AS score \
                     RETURN {CHUNK}, {HIT_PROVENANCE}, score ORDER BY score DESC, c.id {}",
                    class_condition(classes),
                    page_clause(page)
                ))
                .param("vector", values.to_vec())
                .param("zero", zero),
            )
            .await?;
        page_of(&rows, page, hit)
    }
}

#[async_trait]
impl TaskReader for Neo4jStore {
    async fn get_task(&self, id: TaskId) -> Result<Option<Task>, StoreError> {
        let rows = self
            .rows(
                query(&format!("MATCH (t:Task {{id: $id}}) RETURN {TASK}"))
                    .param("id", id.to_string()),
            )
            .await?;
        rows.first().map(codec::task).transpose()
    }

    async fn list_tasks(
        &self,
        filter: TaskQuery,
        page: PageRequest,
    ) -> Result<Page<Task>, StoreError> {
        let mut conditions = vec!["true"];
        if filter.status.is_some() {
            conditions.push("t.status = $status");
        }
        if filter.due_by.is_some() {
            conditions.push("t.due_at IS NOT NULL AND t.due_at <= $due_by");
        }
        let mut query = query(&format!(
            "MATCH (t:Task) WHERE {} RETURN {TASK} ORDER BY t.created_at, t.id {}",
            conditions.join(" AND "),
            page_clause(page)
        ));
        if let Some(status) = filter.status {
            query = query.param("status", task_status_name(status));
        }
        if let Some(due_by) = filter.due_by {
            query = query.param("due_by", at(due_by));
        }
        let rows = self.rows(query).await?;
        page_of(&rows, page, codec::task)
    }

    async fn get_working(
        &self,
        scope: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<WorkingEntry>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (w:Working {{scope: $scope, key: $key}}) \
                     WHERE w.expires_at IS NULL OR w.expires_at > $now RETURN {WORKING}"
                ))
                .param("scope", scope)
                .param("key", key)
                .param("now", at(now)),
            )
            .await?;
        rows.first().map(codec::working).transpose()
    }
}

#[async_trait]
impl DefinitionReader for Neo4jStore {
    async fn get_definition(&self, key: &DefinitionKey) -> Result<Option<Definition>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (d:Definition {{kind: $kind, name: $name}}) RETURN {DEFINITION}"
                ))
                .param("kind", definition_kind_name(key.kind))
                .param("name", key.name.as_str()),
            )
            .await?;
        rows.first().map(codec::definition).transpose()
    }

    async fn list_definitions(
        &self,
        kind: DefinitionKind,
        page: PageRequest,
    ) -> Result<Page<Definition>, StoreError> {
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (d:Definition {{kind: $kind}}) RETURN {DEFINITION} ORDER BY d.name {}",
                    page_clause(page)
                ))
                .param("kind", definition_kind_name(kind)),
            )
            .await?;
        page_of(&rows, page, codec::definition)
    }
}
