//! Plugin records and the links between them. Checking is the shared
//! catalog's; this module only turns checked queries into Cypher.

use async_trait::async_trait;
use neo4rs::query;
use store::{
    Direction, FieldOp, Page, PageRequest, Record, RecordDecls, RecordId, RecordQuery,
    RecordReader, StoreError,
};

use crate::codec::{self, field_param, field_property};
use crate::{Neo4jStore, page_clause, page_of};

const fn operator(op: FieldOp) -> &'static str {
    match op {
        FieldOp::Eq => "=",
        FieldOp::Lt => "<",
        FieldOp::Lte => "<=",
        FieldOp::Gt => ">",
        FieldOp::Gte => ">=",
    }
}

#[async_trait]
impl RecordReader for Neo4jStore {
    async fn register_records(&self, decls: &RecordDecls) -> Result<(), StoreError> {
        self.catalog
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register(decls)
    }

    async fn get_record(
        &self,
        record_type: &str,
        id: RecordId,
    ) -> Result<Option<Record>, StoreError> {
        self.catalog().check_filters(record_type, &[])?;
        let rows = self
            .rows(
                query(&format!(
                    "MATCH (n:Record {{type: $type, id: $id}}) RETURN {}",
                    codec::record_columns()
                ))
                .param("type", record_type)
                .param("id", id.to_string()),
            )
            .await?;
        rows.first().map(codec::record).transpose()
    }

    async fn query_records(&self, request: RecordQuery) -> Result<Page<Record>, StoreError> {
        self.catalog()
            .check_filters(&request.record_type, &request.filters)?;
        // Field names and values are parameters; only the operators, from a
        // closed list, are written into the query.
        let conditions: Vec<String> = request
            .filters
            .iter()
            .enumerate()
            .map(|(index, filter)| {
                format!("n[$field{index}] {} $value{index}", operator(filter.op))
            })
            .collect();
        let condition = if conditions.is_empty() {
            "true".to_owned()
        } else {
            conditions.join(" AND ")
        };
        let mut query = query(&format!(
            "MATCH (n:Record {{type: $type}}) WHERE {condition} RETURN {} ORDER BY n.id {}",
            codec::record_columns(),
            page_clause(request.page)
        ))
        .param("type", request.record_type.as_str());
        for (index, filter) in request.filters.iter().enumerate() {
            query = query
                .param(&format!("field{index}"), field_property(&filter.field))
                .param(&format!("value{index}"), field_param(&filter.value));
        }
        let rows = self.rows(query).await?;
        page_of(&rows, request.page, codec::record)
    }

    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
    ) -> Result<Page<Record>, StoreError> {
        let decl = self.catalog().relation(relation)?.clone();
        let (pattern, start_type, target_type) = match direction {
            Direction::Outgoing => (
                "(:Record {type: $start_type, id: $id})-[:LINK {relation: $relation}]->(n:Record {type: $target_type})",
                decl.from,
                decl.to,
            ),
            Direction::Incoming => (
                "(:Record {type: $start_type, id: $id})<-[:LINK {relation: $relation}]-(n:Record {type: $target_type})",
                decl.to,
                decl.from,
            ),
        };
        let rows = self
            .rows(
                query(&format!(
                    "MATCH {pattern} RETURN {} ORDER BY n.id {}",
                    codec::record_columns(),
                    page_clause(page)
                ))
                .param("start_type", start_type)
                .param("target_type", target_type)
                .param("id", id.to_string())
                .param("relation", relation),
            )
            .await?;
        page_of(&rows, page, codec::record)
    }
}
