//! The read queries. Each runs on its own connection, against a snapshot.

use duckdb::{Connection, Row, params};
use store::{
    Chunk, ChunkHit, ClassFilter, Direction, Page, PageRequest, Record, RecordCatalog, RecordId,
    RecordQuery, StoreError,
};

use crate::codec::{
    CHUNK_COLUMNS, RECORD_COLUMNS, backend, chunk_at, hit_provenance_at, record_at,
};

/// Runs a query asking for one row more than the page, to see if there's more.
pub(crate) fn paged<T>(
    connection: &Connection,
    sql: &str,
    parameters: &[&dyn duckdb::ToSql],
    page: PageRequest,
    map: impl FnMut(&Row<'_>) -> duckdb::Result<T>,
) -> Result<Page<T>, StoreError> {
    let limit = i64::from(page.limit()) + 1;
    let offset = i64::from(page.offset());
    let mut all: Vec<&dyn duckdb::ToSql> = parameters.to_vec();
    all.push(&limit);
    all.push(&offset);
    let mut statement = connection
        .prepare(&format!("{sql} LIMIT ? OFFSET ?"))
        .map_err(backend)?;
    let rows = statement.query_map(all.as_slice(), map).map_err(backend)?;
    let mut items = rows.collect::<Result<Vec<T>, _>>().map_err(backend)?;
    let has_more = items.len() > page.limit() as usize;
    items.truncate(page.limit() as usize);
    Ok(Page { items, has_more })
}

pub(crate) fn optional<T>(result: duckdb::Result<T>) -> Result<Option<T>, StoreError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(backend(error)),
    }
}

pub(crate) fn chunks_of(connection: &Connection, note: &str) -> Result<Vec<Chunk>, StoreError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {CHUNK_COLUMNS}, v.embedding FROM chunks c
             LEFT JOIN chunk_vectors v ON v.chunk_id = c.id
             WHERE c.note_id = ? ORDER BY c.ordinal"
        ))
        .map_err(backend)?;
    let rows = statement
        .query_map(params![note], |row| chunk_at(row, Some(5)))
        .map_err(backend)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(backend)
}

const PROVENANCE: &str = "n.data_class, n.trust, n.stored_by, n.stored_at";

fn hit_at(row: &Row<'_>) -> duckdb::Result<ChunkHit> {
    Ok(ChunkHit {
        chunk: chunk_at(row, None)?,
        provenance: hit_provenance_at(row)?,
        score: row.get(9)?,
    })
}

const fn public_only(classes: ClassFilter) -> bool {
    matches!(classes, ClassFilter::PublicOnly)
}

pub(crate) fn search_text(
    connection: &Connection,
    query: &str,
    classes: ClassFilter,
    page: PageRequest,
) -> Result<Page<ChunkHit>, StoreError> {
    let sql = format!(
        "SELECT {CHUNK_COLUMNS}, {PROVENANCE}, c.score::FLOAT FROM
           (SELECT *, fts_main_chunks.match_bm25(id, ?) AS score FROM chunks) c
         JOIN notes n ON n.id = c.note_id
         WHERE c.score IS NOT NULL AND (NOT ? OR n.data_class = 'public')
         ORDER BY c.score DESC, c.id"
    );
    paged(
        connection,
        &sql,
        params![query, public_only(classes)],
        page,
        hit_at,
    )
}

pub(crate) fn search_vector(
    connection: &Connection,
    vector: &duckdb::types::Value,
    dimension: usize,
    classes: ClassFilter,
    page: PageRequest,
) -> Result<Page<ChunkHit>, StoreError> {
    // A zero vector has no direction, so its distance is not a number.
    let sql = format!(
        "SELECT {CHUNK_COLUMNS}, {PROVENANCE}, s.score::FLOAT FROM
           (SELECT chunk_id, CASE WHEN isnan(d) THEN 0 ELSE 1 - d END AS score FROM
              (SELECT chunk_id, array_cosine_distance(embedding, ?::FLOAT[{dimension}]) AS d
               FROM chunk_vectors)) s
         JOIN chunks c ON c.id = s.chunk_id
         JOIN notes n ON n.id = c.note_id
         WHERE NOT ? OR n.data_class = 'public'
         ORDER BY s.score DESC, c.id"
    );
    paged(
        connection,
        &sql,
        params![vector, public_only(classes)],
        page,
        hit_at,
    )
}

/// Filters run here rather than in SQL, so they mean exactly what the catalog
/// says, on every store. The scan stops as soon as the page is full.
pub(crate) fn query_records(
    connection: &Connection,
    catalog: &RecordCatalog,
    query: &RecordQuery,
) -> Result<Page<Record>, StoreError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM records r WHERE r.record_type = ? ORDER BY r.id"
        ))
        .map_err(backend)?;
    let rows = statement
        .query_map(params![query.record_type], record_at)
        .map_err(backend)?;
    let wanted = query.page.offset() as usize + query.page.limit() as usize + 1;
    let mut matching = Vec::new();
    for row in rows {
        let record = row.map_err(backend)?;
        if catalog.matches(&record, &query.filters) {
            matching.push(record);
            if matching.len() == wanted {
                break;
            }
        }
    }
    Ok(Page::from_sorted(matching, query.page))
}

pub(crate) fn neighbors(
    connection: &Connection,
    relation: &str,
    target_type: &str,
    id: RecordId,
    direction: Direction,
    page: PageRequest,
) -> Result<Page<Record>, StoreError> {
    let (near, far) = match direction {
        Direction::Outgoing => ("from_id", "to_id"),
        Direction::Incoming => ("to_id", "from_id"),
    };
    let sql = format!(
        "SELECT {RECORD_COLUMNS} FROM links l
         JOIN records r ON r.id = l.{far} AND r.record_type = ?
         WHERE l.relation = ? AND l.{near} = ? ORDER BY r.id"
    );
    paged(
        connection,
        &sql,
        params![target_type, relation, id.to_string()],
        page,
        record_at,
    )
}
