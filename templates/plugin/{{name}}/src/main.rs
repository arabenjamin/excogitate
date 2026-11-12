{%- set kind = name | title -%}
//! {{ name }} as its own process: an MCP server on stdio, with the same
//! manifest, tools and capabilities as when it's compiled in.

use plugin_{{ name }}::{{ kind }}Plugin;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    plugin_api::serve_plugin_stdio(&{{ kind }}Plugin).await
}
