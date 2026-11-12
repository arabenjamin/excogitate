//! A plugin process's sources, answered over the same connection as its
//! tools: `sources/search` and `sources/get`. Anything else goes to MCP.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::rpc::{self, INVALID_PARAMS, failure, success};
use crate::serve::context_of;
use crate::wire::{
    SOURCE_FAILED_CODE, SOURCE_GET_METHOD, SOURCE_SEARCH_METHOD, WireSourceFound, WireSourceGet,
    WireSourceGot, WireSourceItem, WireSourceSearch,
};
use crate::{McpHandler, Source, SourceQuery, ToolFailure};

/// MCP, and the plugin's sources by name.
pub(crate) struct PluginHandler {
    pub(crate) mcp: McpHandler,
    pub(crate) sources: HashMap<String, Arc<dyn Source>>,
}

fn params<T: DeserializeOwned>(message: &Value) -> Result<T, String> {
    let given = message.get("params").cloned().unwrap_or_else(|| json!({}));
    serde_json::from_value(given).map_err(|error| schema::bounded(&error.to_string()))
}

fn failed(id: &Value, error: &ToolFailure) -> Value {
    failure(id, SOURCE_FAILED_CODE, &schema::bounded(error.message()))
}

impl PluginHandler {
    fn source(&self, name: &str) -> Result<&Arc<dyn Source>, String> {
        self.sources
            .get(name)
            .ok_or_else(|| format!("no source named {name}"))
    }

    async fn search(&self, id: &Value, message: &Value) -> Value {
        let asked: WireSourceSearch = match params(message) {
            Ok(asked) => asked,
            Err(problem) => return failure(id, INVALID_PARAMS, &problem),
        };
        let source = match self.source(&asked.source) {
            Ok(source) => source,
            Err(problem) => return failure(id, INVALID_PARAMS, &problem),
        };
        let mut query = SourceQuery::new(asked.text, asked.limit);
        query.vector = asked.vector;
        let context = context_of(message.get("params").unwrap_or(&Value::Null));
        match source.search(query, context).await {
            Ok(found) => success(
                id,
                &json!(WireSourceFound {
                    hits: found.hits.into_iter().map(WireSourceItem::from).collect(),
                    cost_usd_micros: found.cost_usd_micros,
                }),
            ),
            Err(error) => failed(id, &error),
        }
    }

    async fn get(&self, id: &Value, message: &Value) -> Value {
        let asked: WireSourceGet = match params(message) {
            Ok(asked) => asked,
            Err(problem) => return failure(id, INVALID_PARAMS, &problem),
        };
        let source = match self.source(&asked.source) {
            Ok(source) => source,
            Err(problem) => return failure(id, INVALID_PARAMS, &problem),
        };
        let context = context_of(message.get("params").unwrap_or(&Value::Null));
        match source.get(&asked.id, context).await {
            Ok(got) => success(
                id,
                &json!(WireSourceGot {
                    item: got.item.map(WireSourceItem::from),
                    cost_usd_micros: got.cost_usd_micros,
                }),
            ),
            Err(error) => failed(id, &error),
        }
    }
}

#[async_trait]
impl rpc::Handler for PluginHandler {
    async fn handle(&self, message: Value) -> Option<Value> {
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            (Some(SOURCE_SEARCH_METHOD), Some(id)) => Some(self.search(id, &message).await),
            (Some(SOURCE_GET_METHOD), Some(id)) => Some(self.get(id, &message).await),
            _ => self.mcp.handle(&message).await,
        }
    }
}
