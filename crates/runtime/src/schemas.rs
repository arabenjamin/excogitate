//! The bundled tools' schemas, for `schemas/`, so a client or a plugin
//! author can read what each tool takes and returns without the source.

use serde_json::Value;
use tools::{HttpRequestTool, MemoryTool};

use crate::delegate::{DelegateArgs, DelegateResult};
use crate::mcp::{ChatArgs, ChatReply};

/// Each bundled tool by name, with its input and output schemas.
#[must_use]
pub fn bundled_tool_schemas() -> Vec<(&'static str, Value, Value)> {
    let (http_in, http_out) = HttpRequestTool::schemas();
    let (memory_in, memory_out) = MemoryTool::schemas();
    vec![
        ("http_request", http_in, http_out),
        ("memory", memory_in, memory_out),
        (
            workflow::DELEGATE_TOOL,
            schema::schema_of::<DelegateArgs>(),
            schema::schema_of::<DelegateResult>(),
        ),
        (
            "chat",
            schema::schema_of::<ChatArgs>(),
            schema::schema_of::<ChatReply>(),
        ),
    ]
}
