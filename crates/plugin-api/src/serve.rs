//! Running a plugin as its own process: a small MCP server over stdio, one
//! JSON-RPC message per line. Everything the runtime needs is covered; nothing
//! else is implemented.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use protocol::{DataClass, RunId};
use schema::Validator;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite};

use crate::rpc::{self, INVALID_PARAMS, METHOD_NOT_FOUND, Peer, failure, success};
use crate::wire::{CONTEXT_META, WireContext};
use crate::{Tool, ToolContext};

/// The MCP protocol version spoken.
pub const MCP_VERSION: &str = "2025-06-18";

/// The longest line read in either direction. Over this, the connection ends.
pub const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;

/// A tool, and the check its arguments must pass.
struct Served {
    tool: Arc<dyn Tool>,
    input: Validator,
}

fn tool_listing(tools: &HashMap<String, Served>) -> Value {
    let mut listed: Vec<Value> = tools
        .values()
        .map(|Served { tool, .. }| {
            let definition = tool.definition();
            json!({
                "name": definition.name,
                "description": definition.description,
                "inputSchema": definition.input_schema,
                "outputSchema": definition.output_schema,
            })
        })
        .collect();
    listed.sort_by_key(|tool| tool.get("name").map(ToString::to_string));
    json!({ "tools": listed })
}

/// The call's context, as the host sent it in `_meta`. Without one, it's a
/// call from a client that isn't the host: a run of its own, private, with
/// nothing to spend.
fn context_of(params: &Value) -> ToolContext {
    let sent = params
        .get("_meta")
        .and_then(|meta| meta.get(CONTEXT_META))
        .and_then(|context| serde_json::from_value::<WireContext>(context.clone()).ok());
    match sent {
        Some(sent) => {
            let data_class = if sent.public {
                DataClass::Public
            } else {
                DataClass::Private
            };
            let mut context = ToolContext::new(
                RunId::from_uuid(sent.run),
                data_class,
                sent.budget_usd_micros,
            );
            context.call = Some(sent.call);
            context
        }
        None => ToolContext::new(RunId::generate(), DataClass::Private, 0),
    }
}

async fn call_tool(tools: &HashMap<String, Served>, id: &Value, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str);
    let Some(Served { tool, input }) = name.and_then(|name| tools.get(name)) else {
        return failure(id, INVALID_PARAMS, "no such tool");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Err(refusal) = input.check(&arguments) {
        let result = json!({
            "content": [{ "type": "text", "text": refusal.to_string() }],
            "isError": true,
        });
        return success(id, &result);
    }
    let context = context_of(params);
    let result = match tool.run(arguments, context).await {
        Ok(output) => json!({
            "content": [{ "type": "text", "text": output.value.to_string() }],
            "structuredContent": output.value,
            "isError": false,
        }),
        Err(failure) => json!({
            "content": [{ "type": "text", "text": failure.message() }],
            "isError": true,
        }),
    };
    success(id, &result)
}

/// Answers MCP messages, whichever way they arrive.
pub struct McpHandler {
    name: String,
    version: String,
    tools: HashMap<String, Served>,
}

impl std::fmt::Debug for McpHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpHandler")
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl McpHandler {
    /// A handler for `tools`. Arguments are checked against each tool's input
    /// schema before it runs.
    ///
    /// # Errors
    ///
    /// The problem, if a tool's input schema isn't a valid schema.
    pub fn new(name: &str, version: &str, tools: Vec<Arc<dyn Tool>>) -> std::io::Result<Self> {
        let tools = tools
            .into_iter()
            .map(|tool| {
                let definition = tool.definition();
                let input = Validator::from_schema(&definition.input_schema)
                    .map_err(std::io::Error::other)?;
                Ok((definition.name, Served { tool, input }))
            })
            .collect::<std::io::Result<HashMap<_, _>>>()?;
        Ok(Self {
            name: name.to_owned(),
            version: version.to_owned(),
            tools,
        })
    }

    /// The reply to one message. A notification gets none.
    pub async fn handle(&self, message: &Value) -> Option<Value> {
        // A message without an id is a notification, and gets no reply.
        let id = message.get("id")?;
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        Some(match message.get("method").and_then(Value::as_str) {
            Some("initialize") => success(
                id,
                &json!({
                    "protocolVersion": MCP_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": self.name, "version": self.version },
                }),
            ),
            Some("ping") => success(id, &json!({})),
            Some("tools/list") => success(id, &tool_listing(&self.tools)),
            Some("tools/call") => call_tool(&self.tools, id, &params).await,
            _ => failure(id, METHOD_NOT_FOUND, "no such method"),
        })
    }
}

#[async_trait]
impl rpc::Handler for McpHandler {
    async fn handle(&self, message: Value) -> Option<Value> {
        McpHandler::handle(self, &message).await
    }
}

/// Reads one line of at most [`MAX_LINE_BYTES`]. `None` means the stream
/// ended or the line was too long, and either way the connection is over.
///
/// # Errors
///
/// The read error.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = tokio::io::AsyncReadExt::take(&mut *reader, MAX_LINE_BYTES + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if read == 0 || (u64::try_from(line.len()).unwrap_or(u64::MAX) > MAX_LINE_BYTES) {
        return Ok(None);
    }
    Ok(Some(line))
}

/// Serves `tools` over any byte stream until it ends. Arguments are checked
/// against each tool's input schema before it runs.
///
/// # Errors
///
/// The I/O error, if reading or writing fails, or if a tool's input schema
/// isn't a valid schema.
pub async fn serve<R, W>(
    name: &str,
    version: &str,
    tools: Vec<Arc<dyn Tool>>,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let handler = Arc::new(McpHandler::new(name, version, tools)?);
    let (peer, io) = Peer::new(rpc::Limits::default());
    peer.run(io, reader, writer, handler).await
}

/// Serves `tools` on this process's stdin and stdout. Call it from a plugin
/// binary's `main`.
///
/// # Errors
///
/// The I/O error, if reading or writing fails.
pub async fn serve_stdio(
    name: &str,
    version: &str,
    tools: Vec<Arc<dyn Tool>>,
) -> std::io::Result<()> {
    serve(
        name,
        version,
        tools,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{Effect, ToolDefinition, ToolFailure, ToolOutput};
    use async_trait::async_trait;
    use protocol::Trust;

    #[derive(Debug)]
    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                "demo_echo".to_owned(),
                "d".to_owned(),
                json!({
                    "type": "object",
                    "properties": { "tight": { "type": "integer" } }
                }),
                json!({ "type": "object", "additionalProperties": false }),
                Effect::Read,
            )
        }

        async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
            if arguments.get("fail").is_some() {
                return Err(ToolFailure::new("nope"));
            }
            Ok(ToolOutput::new(arguments, Trust::Untrusted))
        }
    }

    async fn exchange(input: &str) -> Vec<Value> {
        let mut output = Vec::new();
        serve(
            "demo",
            "1",
            vec![Arc::new(Echo)],
            input.as_bytes(),
            &mut output,
        )
        .await
        .unwrap();
        // Requests are served at once, so replies come in any order.
        let mut replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        replies.sort_by_key(|reply| reply["id"].as_i64());
        replies
    }

    #[tokio::test]
    async fn it_handshakes_lists_and_calls() {
        let replies = exchange(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#, "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"demo_echo","arguments":{"a":1}}}"#, "\n",
        ))
        .await;
        assert_eq!(replies.len(), 3);
        assert_eq!(
            replies.first().unwrap()["result"]["protocolVersion"],
            MCP_VERSION
        );
        assert_eq!(
            replies.get(1).unwrap()["result"]["tools"][0]["name"],
            "demo_echo"
        );
        assert_eq!(
            replies.get(2).unwrap()["result"]["structuredContent"]["a"],
            1
        );
    }

    #[tokio::test]
    async fn a_failing_tool_is_an_error_result_not_a_protocol_error() {
        let replies = exchange(concat!(
            r#"{"id":1,"method":"tools/call","params":{"name":"demo_echo","arguments":{"fail":true}}}"#,
            "\n"
        ))
        .await;
        assert_eq!(replies.first().unwrap()["result"]["isError"], true);
    }

    #[tokio::test]
    async fn arguments_that_miss_the_schema_never_reach_the_tool() {
        let replies = exchange(concat!(
            r#"{"id":1,"method":"tools/call","params":{"name":"demo_echo","arguments":{"tight":"x","fail":true}}}"#,
            "\n"
        ))
        .await;
        let result = &replies.first().unwrap()["result"];
        assert_eq!(result["isError"], true);
        assert_ne!(result["content"][0]["text"], "nope", "it ran");
    }

    #[tokio::test]
    async fn ping_is_answered() {
        let replies = exchange(concat!(r#"{"id":1,"method":"ping"}"#, "\n")).await;
        assert_eq!(replies.first().unwrap()["result"], json!({}));
    }

    #[tokio::test]
    async fn bad_input_gets_an_error_and_the_server_goes_on() {
        let replies = exchange(concat!(
            "not json\n",
            r#"{"id":1,"method":"nope"}"#,
            "\n",
            r#"{"id":2,"method":"tools/call","params":{"name":"ghost"}}"#,
            "\n",
            r#"{"id":3,"method":"tools/list"}"#,
            "\n",
        ))
        .await;
        let codes: Vec<_> = replies.iter().map(|r| r["error"]["code"].clone()).collect();
        assert_eq!(
            codes[..3],
            [
                json!(rpc::PARSE_ERROR),
                json!(METHOD_NOT_FOUND),
                json!(INVALID_PARAMS)
            ]
        );
        assert!(replies.get(3).unwrap().get("result").is_some());
    }

    #[tokio::test]
    async fn an_oversized_line_ends_the_connection() {
        let huge = "x".repeat(usize::try_from(MAX_LINE_BYTES).unwrap() + 10);
        let replies = exchange(&huge).await;
        assert_eq!(replies, [] as [serde_json::Value; 0]);
    }
}
