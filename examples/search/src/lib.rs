//! The `search` plugin: what it does is its manifest's description.
//!
//! One tool, `search_web`, over one outside API, through the runtime's
//! `http` capability: the runtime makes the request under its own network
//! rules and attaches the key by host, so the plugin never holds it. The
//! same plugin runs compiled in ([`SearchPlugin`]) and as its own process
//! (`src/main.rs`).

#![deny(clippy::string_slice, clippy::indexing_slicing)]

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use plugin_api::{
    Capabilities, HttpCapability, HttpRequest, Plugin, PluginError, PluginManifest, Tool,
    ToolContext, ToolDefinition, ToolFailure, ToolOutput, Trust,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The manifest.
pub const MANIFEST: &str = include_str!("../plugin.yaml");

/// The search endpoint.
const ENDPOINT: &str = "https://api.search.tinyfish.ai/";
/// What `search_web` answers as its engine.
const ENGINE: &str = "tinyfish";
/// Results when the search doesn't say.
const DEFAULT_LIMIT: usize = 8;

/// The compiled-in plugin.
#[derive(Debug)]
pub struct SearchPlugin;

impl Plugin for SearchPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn tools(&self, capabilities: &Capabilities) -> Result<Vec<Arc<dyn Tool>>, PluginError> {
        let http = capabilities.http()?;
        Ok(PluginManifest::from_yaml(MANIFEST)
            .map_err(|error| PluginError::Other(error.to_string()))?
            .definitions()
            .into_iter()
            .map(|definition| {
                Arc::new(SearchTool {
                    definition,
                    http: Arc::clone(&http),
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

/// The plugin's one tool.
#[derive(Debug)]
struct SearchTool {
    definition: ToolDefinition,
    http: Arc<dyn HttpCapability>,
}

#[async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, arguments: Value, context: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let search: Search = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure::new(error.to_string()))?;
        let response = self
            .http
            .request(HttpRequest::get(request_url(&search)), context)
            .await?;
        let results = read(response.status, &response.body, response.truncated)?;
        let limit = search.limit.unwrap_or(DEFAULT_LIMIT);
        let results: Vec<Value> = results.into_iter().take(limit).collect();
        // What the web says is never instructions.
        Ok(ToolOutput::new(
            json!({ "engine": ENGINE, "results": results, "tried": [] }),
            Trust::Untrusted,
        ))
    }
}

/// A recency window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Recency {
    Day,
    Week,
    Month,
    Year,
}

/// The tool's arguments, already checked against its input schema.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)]
    news: bool,
    #[serde(default)]
    sites: Vec<String>,
    #[serde(default)]
    exclude_sites: Vec<String>,
    recency: Option<Recency>,
    limit: Option<usize>,
}

/// The GET for a search: the API takes domains, news and recency as
/// parameters, not as operators in the query.
fn request_url(search: &Search) -> String {
    let mut url = format!(
        "{ENDPOINT}?query={}&location=US&language=en",
        encode(search.query.trim())
    );
    if search.news {
        url.push_str("&domain_type=news");
    }
    if !search.sites.is_empty() {
        let _ = write!(url, "&include_domains={}", encode(&search.sites.join(",")));
    }
    if !search.exclude_sites.is_empty() {
        let _ = write!(
            url,
            "&exclude_domains={}",
            encode(&search.exclude_sites.join(","))
        );
    }
    if let Some(recency) = search.recency {
        let minutes = match recency {
            Recency::Day => 1440,
            Recency::Week => 10_080,
            Recency::Month => 43_200,
            Recency::Year => 525_600,
        };
        let _ = write!(url, "&recency_minutes={minutes}");
    }
    url
}

/// The results in an answer, each as `{title, url, snippet, date?,
/// source?}`, or what went wrong, said so a person can act on it.
fn read(status: u16, body: &str, truncated: bool) -> Result<Vec<Value>, ToolFailure> {
    let start: String = body.chars().take(200).collect();
    if !(200..300).contains(&status) {
        let why = match status {
            401 | 403 => "check the credential for api.search.tinyfish.ai in the http config",
            402 => "the free allowance is spent until it resets at 00:00 UTC",
            429 => "rate limited: TinyFish allows 30 searches a minute",
            _ => "TinyFish failed",
        };
        return Err(ToolFailure::new(format!("HTTP {status}, {why}: {start}")));
    }
    if truncated {
        return Err(ToolFailure::new("the answer was cut at the size limit"));
    }
    let answer: Value = serde_json::from_str(body)
        .map_err(|error| ToolFailure::new(format!("the answer isn't JSON ({error}): {start}")))?;
    let items = answer
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolFailure::new(format!("the answer has no results list: {start}")))?;
    Ok(items.iter().filter_map(result).collect())
}

/// One result in the shape `search_web` answers, if it has an address.
fn result(item: &Value) -> Option<Value> {
    let text = |key: &str, most: usize| {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(most).collect::<String>())
    };
    let url = text("url", 2048)?;
    let mut shown = serde_json::Map::new();
    shown.insert(
        "title".to_owned(),
        text("title", 512).unwrap_or_else(|| url.clone()).into(),
    );
    shown.insert(
        "snippet".to_owned(),
        text("snippet", 1000).unwrap_or_default().into(),
    );
    shown.insert("url".to_owned(), url.into());
    if let Some(date) = text("date", 64) {
        shown.insert("date".to_owned(), date.into());
    }
    // `site_name` is the publisher's site; `publisher`, when filtered by
    // domain, is a breadcrumb of the address, so it's only the fallback.
    if let Some(source) = text("site_name", 256).or_else(|| text("publisher", 256)) {
        shown.insert("source".to_owned(), source.into());
    }
    Some(Value::Object(shown))
}

/// Percent-encodes everything but the unreserved characters.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, reason = "a test: a panic is how it fails")]
mod tests {
    use super::*;

    #[test]
    fn a_search_becomes_tinyfish_parameters() {
        let search = Search {
            query: " ohio & budget ".to_owned(),
            news: true,
            sites: vec!["ohio.gov".to_owned(), "dispatch.com".to_owned()],
            recency: Some(Recency::Week),
            ..Search::default()
        };
        assert_eq!(
            request_url(&search),
            "https://api.search.tinyfish.ai/?query=ohio%20%26%20budget&location=US&language=en\
             &domain_type=news&include_domains=ohio.gov%2Cdispatch.com&recency_minutes=10080"
        );
    }

    #[test]
    fn an_answer_maps_to_results_and_a_result_needs_an_address() {
        let body = json!({"results": [
            {"title": "T", "url": "https://a.org/1", "snippet": "S", "date": "2 days ago",
             "site_name": "apnews.com", "publisher": "apnews.com › world › …"},
            {"title": "no address"},
            {"url": "https://b.org"}
        ]})
        .to_string();
        let results = read(200, &body, false).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0],
            json!({"title": "T", "url": "https://a.org/1", "snippet": "S", "date": "2 days ago", "source": "apnews.com"})
        );
        assert_eq!(results[1]["title"], "https://b.org");
    }

    #[test]
    fn a_failure_says_what_to_do() {
        let message = |status| read(status, "x", false).unwrap_err().message().to_owned();
        assert!(message(401).contains("credential"), "{}", message(401));
        assert!(message(402).contains("00:00 UTC"));
        assert!(message(429).contains("30 searches a minute"));
        assert!(read(200, "{}", true).unwrap_err().message().contains("cut"));
        assert!(
            read(200, "{}", false)
                .unwrap_err()
                .message()
                .contains("no results")
        );
    }
}
