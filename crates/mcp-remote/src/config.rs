//! The owner's settings for each remote server. A model can't reach any of
//! them.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use url::Url;

/// How long a call to a server may take by default, in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u32 = 30;
/// How much of a result is kept by default, in characters.
pub const DEFAULT_RESULT_CHARS: usize = 50_000;
/// The most of a result that can be kept, in characters.
pub const MAX_RESULT_CHARS: usize = 200_000;

const fn default_timeout() -> u32 {
    DEFAULT_TIMEOUT_SECONDS
}

const fn default_result_chars() -> usize {
    DEFAULT_RESULT_CHARS
}

/// One remote MCP server and the tools taken from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Its name. Each of its tools is offered as `<name>_<tool>`.
    #[schemars(length(min = 1, max = 24), regex(pattern = r"^[a-z][a-z0-9]{0,23}$"))]
    pub name: String,
    /// Its Streamable HTTP endpoint. https, unless `allow_insecure`.
    #[schemars(length(min = 1, max = 2048))]
    pub url: String,
    /// The header its key goes in, such as `authorization` or `x-api-key`.
    /// With `env`, or not at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    pub header: Option<String>,
    /// Put before the key, such as `Bearer `.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[schemars(length(max = 32))]
    pub prefix: String,
    /// The environment variable holding the key. With `header`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub env: Option<String>,
    /// Allows plain http, such as to a server on the same host.
    #[serde(default)]
    pub allow_insecure: bool,
    /// The server keeps nothing it's sent, so private runs may call it. A
    /// public run may call any server.
    #[serde(default)]
    pub no_log: bool,
    /// The tools taken from it, by the names it lists them under. Nothing
    /// else it offers is reachable.
    #[schemars(length(min = 1, max = 128), inner(length(min = 1, max = 128)))]
    pub tools: Vec<String>,
    /// Which of `tools` only read. Every other one counts as a write, so a
    /// probe never runs it and it spends the run's write budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 128), inner(length(min = 1, max = 128)))]
    pub read_only: Vec<String>,
    /// How long one call may take, in seconds.
    #[serde(default = "default_timeout")]
    #[schemars(range(min = 1, max = 600))]
    pub timeout_seconds: u32,
    /// How much of a result a run sees, in characters; past it the result
    /// is cut and says so.
    #[serde(default = "default_result_chars")]
    #[schemars(range(min = 1, max = 200_000))]
    pub max_result_chars: usize,
}

/// The name a remote tool is offered under: the server's name, then the
/// tool's in lower case with anything but letters, digits and `_` as `_`.
#[must_use]
pub fn local_name(server: &str, tool: &str) -> String {
    let tool: String = tool
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{server}_{tool}")
}

impl McpServerConfig {
    /// What's wrong with it that its schema can't say.
    ///
    /// # Errors
    ///
    /// The first problem, naming the server.
    pub fn check(&self) -> Result<Url, String> {
        let problem = |what: String| format!("MCP server {}: {what}", self.name);
        let url = Url::parse(&self.url).map_err(|error| problem(format!("its url: {error}")))?;
        match url.scheme() {
            "https" => {}
            "http" if self.allow_insecure => {}
            "http" => {
                return Err(problem(
                    "its url is http, and allow_insecure isn't set".to_owned(),
                ));
            }
            other => return Err(problem(format!("its url is {other}, not http or https"))),
        }
        if self.header.is_some() != self.env.is_some() {
            return Err(problem("header and env go together".to_owned()));
        }
        let tools: BTreeSet<&str> = self.tools.iter().map(String::as_str).collect();
        if tools.len() != self.tools.len() {
            return Err(problem("a tool is listed twice".to_owned()));
        }
        if let Some(stray) = self
            .read_only
            .iter()
            .find(|tool| !tools.contains(tool.as_str()))
        {
            return Err(problem(format!(
                "read_only names {stray}, which isn't in tools"
            )));
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            let name = local_name(&self.name, tool);
            if name.len() > 64 {
                return Err(problem(format!(
                    "{tool} would be offered as {name}, over 64 characters"
                )));
            }
            if !names.insert(name.clone()) {
                return Err(problem(format!(
                    "two tools would both be offered as {name}"
                )));
            }
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

    use super::*;

    fn server() -> McpServerConfig {
        McpServerConfig {
            name: "jobs".to_owned(),
            url: "https://example.com/mcp".to_owned(),
            header: None,
            prefix: String::new(),
            env: None,
            allow_insecure: false,
            no_log: false,
            tools: vec!["search_jobs".to_owned(), "Get-Job".to_owned()],
            read_only: vec!["search_jobs".to_owned()],
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            max_result_chars: DEFAULT_RESULT_CHARS,
        }
    }

    #[test]
    fn a_tool_is_offered_under_its_server() {
        assert_eq!(local_name("jobs", "Get-Job"), "jobs_get_job");
        assert_eq!(local_name("jobs", "search.v2"), "jobs_search_v2");
        assert!(server().check().is_ok());
    }

    #[test]
    fn what_the_schema_cant_say_is_checked() {
        let mut http = server();
        http.url = "http://example.com/mcp".to_owned();
        assert!(http.check().unwrap_err().contains("allow_insecure"));
        http.allow_insecure = true;
        assert!(http.check().is_ok());

        let mut half = server();
        half.header = Some("x-api-key".to_owned());
        assert!(
            half.check()
                .unwrap_err()
                .contains("header and env go together")
        );

        let mut stray = server();
        stray.read_only.push("apply".to_owned());
        assert!(stray.check().unwrap_err().contains("read_only names apply"));

        let mut clash = server();
        clash.tools.push("get_job".to_owned());
        assert!(
            clash
                .check()
                .unwrap_err()
                .contains("both be offered as jobs_get_job")
        );
    }
}
