//! search as its own process: an MCP server on stdio, with the same
//! manifest, tools and capabilities as when it's compiled in.

use plugin_search::SearchPlugin;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    plugin_api::serve_plugin_stdio(&SearchPlugin).await
}
