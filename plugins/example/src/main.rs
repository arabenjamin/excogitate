//! The example plugin as its own process: an MCP server on stdio.

use std::sync::Arc;

use plugin_api::{Tool, serve_stdio};
use plugin_example::{SERVICE_MANIFEST, WordCount};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(WordCount::from_manifest(SERVICE_MANIFEST)?)];
    serve_stdio("example", env!("CARGO_PKG_VERSION"), tools).await?;
    Ok(())
}
