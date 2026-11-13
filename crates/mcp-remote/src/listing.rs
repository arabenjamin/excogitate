//! Each server's last good listing of the tools taken from it, kept in the
//! store, so a server that's down when the runtime starts still has its
//! tools registered (their calls fail, naming it, until it's back) and the
//! definitions that name them still load.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Trust};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use store::{
    FieldOp, FieldValue, PageRequest, Provenance, Record, RecordDecls, RecordId, RecordQuery,
    Store, UnitOfWork, WriteOp,
};

/// The record type: the runtime's own, so no plugin may be named `mcp_client`.
pub const LISTING: &str = "mcp_client.listing";

/// One tool as the server listed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listed {
    /// Its name on the server.
    pub name: String,
    /// What the server says it does.
    pub description: String,
    /// The server's input schema, as it sent it.
    pub input_schema: Value,
}

/// A server's listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    /// The server.
    pub server: String,
    /// When it was listed.
    pub listed_at: DateTime<Utc>,
    /// The tools taken from it.
    pub tools: Vec<Listed>,
}

fn decls() -> Result<RecordDecls, String> {
    serde_json::from_value(json!({
        "types": [{
            "name": LISTING,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["server", "listed_at", "tools"],
                "properties": {
                    "server": {"type": "string", "maxLength": 24},
                    "listed_at": {"type": "string", "format": "date-time"},
                    "tools": {
                        "type": "array",
                        "maxItems": 128,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["name", "description", "input_schema"],
                            "properties": {
                                "name": {"type": "string", "maxLength": 128},
                                "description": {"type": "string", "maxLength": 4096},
                                "input_schema": {"x-extension": "the remote server's own schema"}
                            }
                        }
                    }
                }
            },
            "filterable": [{"name": "server", "kind": "text"}]
        }],
        "relations": []
    }))
    .map_err(|error| error.to_string())
}

/// The listings, through the store.
#[derive(Debug, Clone)]
pub struct Listings {
    store: Arc<dyn Store>,
}

impl Listings {
    /// Declares the record type and returns the listings.
    ///
    /// # Errors
    ///
    /// When the store refuses the type.
    pub async fn open(store: Arc<dyn Store>) -> Result<Self, String> {
        store
            .register_records(&decls()?)
            .await
            .map_err(|error| format!("declaring {LISTING}: {error}"))?;
        Ok(Self { store })
    }

    async fn found(&self, server: &str) -> Result<Option<Record>, String> {
        let page = PageRequest::new(1, 0).map_err(|error| error.to_string())?;
        let query = RecordQuery::new(LISTING, page).with_filter(
            "server",
            FieldOp::Eq,
            FieldValue::Text(server.to_owned()),
        );
        let found = self
            .store
            .query_records(query)
            .await
            .map_err(|error| error.to_string())?;
        Ok(found.items.into_iter().next())
    }

    /// The last listing kept for `server`.
    ///
    /// # Errors
    ///
    /// When the store fails or what it holds can't be read.
    pub async fn get(&self, server: &str) -> Result<Option<Listing>, String> {
        self.found(server)
            .await?
            .map(|record| {
                serde_json::from_value(record.data)
                    .map_err(|error| format!("the kept listing of {server}: {error}"))
            })
            .transpose()
    }

    /// Keeps `listing` in place of the last one.
    ///
    /// # Errors
    ///
    /// When the store fails or refuses it.
    pub async fn put(&self, listing: &Listing) -> Result<(), String> {
        let id = self
            .found(&listing.server)
            .await?
            .map_or_else(RecordId::generate, |record| record.id);
        let data = serde_json::to_value(listing).map_err(|error| error.to_string())?;
        let record = Record::new(
            LISTING.to_owned(),
            id,
            data,
            Provenance {
                data_class: DataClass::Private,
                trust: Trust::Untrusted,
                stored_by: Actor::new("mcp-remote"),
                stored_at: Utc::now(),
            },
        );
        self.store
            .apply(UnitOfWork::new().with(WriteOp::PutRecord(record)))
            .await
            .map_err(|error| format!("keeping the listing of {}: {error}", listing.server))
    }
}
