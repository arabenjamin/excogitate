//! A remote server's tool, offered as the runtime's own: its arguments are
//! checked against its tightened schema by dispatch like any tool's, its
//! result is untrusted, and a private run can't reach a server that isn't
//! marked `no_log`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dispatch::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::{DataClass, Trust};
use schema::bounded;
use serde_json::{Map, Value, json};

use crate::transport::{Connection, McpError};

/// What a server's tools share: its name, its connection, what it may be
/// sent.
#[derive(Debug)]
pub struct Server {
    /// Its name.
    pub name: String,
    /// Its connection.
    pub connection: Connection,
    /// Private runs may call it.
    pub no_log: bool,
    /// How much of a result is kept.
    pub max_result_chars: usize,
}

/// One remote tool.
#[derive(Debug)]
pub struct RemoteTool {
    server: Arc<Server>,
    remote: String,
    definition: ToolDefinition,
}

impl RemoteTool {
    /// The tool `remote` on `server`, offered as `definition`.
    #[must_use]
    pub fn new(server: Arc<Server>, remote: String, definition: ToolDefinition) -> Self {
        Self {
            server,
            remote,
            definition,
        }
    }
}

/// The text of a result's content, joined; anything but text is named.
fn text_of(content: &[Value]) -> String {
    content
        .iter()
        .map(|item| match item.get("type").and_then(Value::as_str) {
            Some("text") => item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            Some("resource") => item
                .get("resource")
                .and_then(|resource| resource.get("text"))
                .and_then(Value::as_str)
                .map_or_else(|| "[a resource]".to_owned(), str::to_owned),
            Some(other) => format!(
                "[{} content left out]",
                other.chars().take(32).collect::<String>()
            ),
            None => "[content of no type left out]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `tools/call` result as the runtime returns it, cut to `most`
/// characters, or the failure it reports.
///
/// # Errors
///
/// The server's own error text, when it says the call failed.
pub fn output_of(result: &Value, most: usize) -> Result<Value, String> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let text = text_of(content);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(if text.is_empty() {
            "it gave no reason".to_owned()
        } else {
            text
        });
    }
    let mut truncated = false;
    let text = if text.chars().count() > most {
        truncated = true;
        text.chars().take(most).collect()
    } else {
        text
    };
    let mut output = Map::new();
    output.insert("text".to_owned(), Value::String(text));
    if let Some(structured) = result.get("structuredContent") {
        if structured.to_string().chars().count() <= most {
            output.insert("structured".to_owned(), structured.clone());
        } else {
            truncated = true;
        }
    }
    output.insert("truncated".to_owned(), Value::Bool(truncated));
    Ok(Value::Object(output))
}

#[async_trait]
impl Tool for RemoteTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn effect_of(&self, _arguments: &Value) -> Effect {
        self.definition.effect
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let server = &self.server;
        if context.data_class == DataClass::Private && !server.no_log {
            return Err(ToolFailure::new(format!(
                "refused: the run is private, and the MCP server {} isn't marked no_log",
                server.name
            )));
        }
        let limit = Duration::from_millis(context.time_left_ms);
        let params = json!({"name": self.remote, "arguments": arguments});
        let result = server
            .connection
            .call("tools/call", &params, Some(limit))
            .await
            .map_err(|error: McpError| {
                ToolFailure::new(bounded(&format!("the MCP server {}: {error}", server.name)))
            })?;
        let output = output_of(&result, server.max_result_chars).map_err(|why| {
            ToolFailure::new(bounded(&format!(
                "the MCP server {}: {}",
                server.name,
                server.connection.scrub(&why)
            )))
        })?;
        Ok(ToolOutput::new(output, Trust::Untrusted))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

    use super::*;

    #[test]
    fn a_result_is_its_text_and_structure() {
        let result = json!({
            "content": [{"type": "text", "text": "two jobs"}, {"type": "image", "data": "..."}],
            "structuredContent": {"jobs": [1, 2]}
        });
        let output = output_of(&result, 1000).unwrap();
        assert_eq!(output["text"], "two jobs\n[image content left out]");
        assert_eq!(output["structured"], json!({"jobs": [1, 2]}));
        assert_eq!(output["truncated"], false);
    }

    #[test]
    fn a_long_result_is_cut_and_says_so() {
        let result = json!({"content": [{"type": "text", "text": "abcdef"}],
                            "structuredContent": {"long": "x".repeat(50)}});
        let output = output_of(&result, 4).unwrap();
        assert_eq!(output["text"], "abcd");
        assert_eq!(output.get("structured"), None);
        assert_eq!(output["truncated"], true);
    }

    #[test]
    fn an_error_result_is_a_failure_with_its_reason() {
        let result =
            json!({"isError": true, "content": [{"type": "text", "text": "job not found"}]});
        assert_eq!(output_of(&result, 100).unwrap_err(), "job not found");
        assert_eq!(
            output_of(&json!({"isError": true}), 100).unwrap_err(),
            "it gave no reason"
        );
    }
}
