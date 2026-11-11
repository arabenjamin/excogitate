//! The example plugin as its own process: an MCP server on stdio, with the
//! same manifest, tools and capabilities as when it's compiled in. Its
//! records go back to the host over the connection.

use plugin_example::ExamplePlugin;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    plugin_api::serve_plugin_stdio(&ExamplePlugin).await
}
