//! The tool itself: one call, each hop checked, the answer bounded.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use plugin_api::{Effect, Tool, ToolContext, ToolDefinition, ToolFailure, ToolOutput};
use protocol::Trust;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, LOCATION};
use reqwest::{Response, StatusCode};
use schema::Validator;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::{Host, Url};

use super::config::{HttpConfig, HttpConfigError, MAX_BODY_CHARS, Method};
use super::guard::{AddressPolicy, BlockedAddress, GuardedResolver};
use super::text::readable_text;
use crate::clamp::{clamp_chars, clamped};

const MAX_URL: usize = 2048;
const MAX_HEADERS: usize = 16;
const MAX_REQUEST_BODY: usize = 64_000;
const MAX_NOTE: usize = 512;
const MAX_TITLE: usize = 512;
const REDACTED: &str = "[redacted]";

/// Headers a model may not set: the ones that carry identity, or that would
/// let it change how the request is framed or decoded.
const REFUSED_HEADERS: [&str; 11] = [
    "authorization",
    "cookie",
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
    "forwarded",
];

/// How much of the page the model wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Mode {
    /// The readable text of a page.
    #[default]
    Text,
    /// The response as it came.
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HeaderArg {
    /// Its name.
    #[schemars(length(min = 1, max = 64))]
    name: String,
    /// Its value.
    #[schemars(length(max = 1024))]
    value: String,
}

/// What the model asks for.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HttpArgs {
    /// The address, http or https.
    #[schemars(length(min = 1, max = MAX_URL))]
    url: String,
    /// The method.
    #[serde(default)]
    method: Method,
    /// Extra request headers.
    #[serde(default)]
    #[schemars(length(max = MAX_HEADERS))]
    headers: Vec<HeaderArg>,
    /// The request body.
    #[schemars(length(max = MAX_REQUEST_BODY))]
    body: Option<String>,
    /// `text` for a page's readable text, `raw` for the response as it came.
    #[serde(default)]
    mode: Mode,
}

/// What came back. A 404 is a successful call that reports a 404.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct HttpResult {
    /// The status code.
    status: u16,
    /// The address that answered, after any redirects.
    #[schemars(length(max = MAX_URL))]
    url: String,
    /// The response's content type.
    #[schemars(length(max = 256))]
    content_type: Option<String>,
    /// The page's title.
    #[schemars(length(max = MAX_TITLE))]
    title: Option<String>,
    /// The content.
    #[schemars(length(max = MAX_BODY_CHARS))]
    body: Option<String>,
    /// True if there was more than `body` holds.
    truncated: bool,
    /// Why there's no `body`.
    #[schemars(length(max = MAX_NOTE))]
    omitted_reason: Option<String>,
    /// True if some bytes weren't valid UTF-8 and were replaced.
    decoding_errors: bool,
    /// How many redirects were followed.
    redirects: u32,
    /// Why a redirect wasn't followed.
    #[schemars(length(max = MAX_NOTE))]
    redirect_not_followed: Option<String>,
}

/// A key's value. Its `Debug` doesn't print it.
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[derive(Debug)]
struct ResolvedCredential {
    host: String,
    header: HeaderName,
    value: HeaderValue,
    secret: Secret,
    allow_insecure: bool,
}

/// One request of a call. A call is one or more, one per redirect.
#[derive(Debug, Clone)]
struct Hop {
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Option<String>,
}

/// Lets a model fetch from, and if configured send to, the web.
#[derive(Debug)]
pub struct HttpRequestTool {
    client: reqwest::Client,
    policy: Arc<AddressPolicy>,
    credentials: Vec<ResolvedCredential>,
    methods: Vec<Method>,
    timeout: Duration,
    max_read_bytes: usize,
    max_body_chars: usize,
    max_redirects: u32,
}

fn resolve_credentials(
    config: &HttpConfig,
    secret_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<ResolvedCredential>, HttpConfigError> {
    config
        .credentials
        .iter()
        .map(|credential| {
            let secret = secret_of(&credential.env)
                .filter(|secret| !secret.is_empty())
                .ok_or_else(|| HttpConfigError::MissingSecret(credential.env.clone()))?;
            let bad = || HttpConfigError::BadCredential(credential.host.clone());
            let mut value = HeaderValue::from_str(&format!("{}{secret}", credential.prefix))
                .map_err(|_| bad())?;
            value.set_sensitive(true);
            Ok(ResolvedCredential {
                host: credential.host.to_ascii_lowercase(),
                header: HeaderName::from_bytes(credential.header.as_bytes()).map_err(|_| bad())?,
                value,
                secret: Secret(secret),
                allow_insecure: credential.allow_insecure,
            })
        })
        .collect()
}

fn refuse(reason: impl fmt::Display) -> ToolFailure {
    ToolFailure(schema::bounded(&format!("refused: {reason}")))
}

fn describe(error: &reqwest::Error) -> ToolFailure {
    let mut source: Option<&(dyn Error + 'static)> = error.source();
    while let Some(cause) = source {
        if let Some(blocked) = cause.downcast_ref::<BlockedAddress>() {
            return refuse(blocked);
        }
        source = cause.source();
    }
    let what = if error.is_timeout() {
        "the request timed out"
    } else if error.is_connect() {
        "couldn't connect"
    } else {
        "the request failed"
    };
    ToolFailure(what.to_owned())
}

/// The part of a content type before its parameters, in lowercase.
fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn is_html(essence: &str) -> bool {
    matches!(essence, "text/html" | "application/xhtml+xml")
}

fn is_text(essence: &str) -> bool {
    essence.starts_with("text/")
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
        || matches!(
            essence,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/yaml"
                | "application/x-yaml"
        )
}

/// What the bytes say. An incomplete character at the end of a cut response
/// isn't an error, so it's dropped first.
fn decode(mut bytes: Vec<u8>, cut: bool) -> (String, bool) {
    if cut
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    match String::from_utf8(bytes) {
        Ok(text) => (text, false),
        Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
    }
}

/// True if the response tells the client to go elsewhere.
const fn is_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

impl HttpRequestTool {
    /// The tool, configured. `secret_of` looks up a credential's variable.
    ///
    /// # Errors
    ///
    /// [`HttpConfigError`] if the configuration doesn't meet its schema or
    /// can't be used.
    pub fn new(
        config: &HttpConfig,
        secret_of: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, HttpConfigError> {
        let invalid = |error: String| HttpConfigError::Invalid(error);
        let value = serde_json::to_value(config).map_err(|error| invalid(error.to_string()))?;
        Validator::for_type::<HttpConfig>()
            .map_err(|error| invalid(error.to_string()))?
            .check(&value)
            .map_err(|error| invalid(error.to_string()))?;
        let policy = Arc::new(AddressPolicy::new(
            &config.blocked_ranges,
            &config.allowed_ranges,
        )?);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(GuardedResolver(Arc::clone(&policy))))
            .user_agent(config.user_agent.clone())
            .build()
            .map_err(|error| HttpConfigError::Client(error.to_string()))?;
        Ok(Self {
            client,
            policy,
            credentials: resolve_credentials(config, secret_of)?,
            methods: config.allowed_methods.clone(),
            timeout: Duration::from_secs(config.timeout_seconds),
            max_read_bytes: config.max_read_bytes,
            max_body_chars: config.max_body_chars,
            max_redirects: config.max_redirects,
        })
    }

    fn method_names(&self) -> String {
        let names: Vec<&str> = self.methods.iter().map(|method| method.as_str()).collect();
        names.join(", ")
    }

    /// Removes any key a response has echoed back.
    fn scrub(&self, text: String) -> String {
        self.credentials.iter().fold(text, |text, credential| {
            if text.contains(&credential.secret.0) {
                text.replace(&credential.secret.0, REDACTED)
            } else {
                text
            }
        })
    }

    fn check_url(&self, url: &Url) -> Result<(), ToolFailure> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(refuse("only http and https are allowed"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(refuse("a URL may not carry a user name or password"));
        }
        let literal = match url.host() {
            Some(Host::Ipv4(address)) => Some(address.into()),
            Some(Host::Ipv6(address)) => Some(address.into()),
            Some(Host::Domain(_)) => None,
            None => return Err(refuse("the URL has no host")),
        };
        // Connecting to an address literal involves no DNS, so the resolver
        // never sees it.
        if literal.is_some_and(|address| !self.policy.permits(address)) {
            return Err(refuse("that address is blocked"));
        }
        Ok(())
    }

    fn credential_for(&self, url: &Url) -> Result<Option<&ResolvedCredential>, ToolFailure> {
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let Some(credential) = self.credentials.iter().find(|c| c.host == host) else {
            return Ok(None);
        };
        if url.scheme() != "https" && !credential.allow_insecure {
            return Err(refuse(format!(
                "the key for {host} is only sent over https"
            )));
        }
        Ok(Some(credential))
    }

    fn request_headers(&self, wanted: &[HeaderArg]) -> Result<HeaderMap, ToolFailure> {
        let mut headers = HeaderMap::new();
        for header in wanted {
            let lowered = header.name.to_ascii_lowercase();
            if REFUSED_HEADERS.contains(&lowered.as_str())
                || lowered.starts_with("proxy-")
                || self
                    .credentials
                    .iter()
                    .any(|c| c.header.as_str() == lowered)
            {
                return Err(refuse(format!("a model may not set {lowered}")));
            }
            let name = HeaderName::from_bytes(lowered.as_bytes())
                .map_err(|_| refuse("that isn't a valid header name"))?;
            let value = HeaderValue::from_str(&header.value)
                .map_err(|_| refuse("that isn't a valid header value"))?;
            headers.append(name, value);
        }
        Ok(headers)
    }

    /// The next request after a redirect, or why it won't be made.
    fn next_hop(
        &self,
        hop: &Hop,
        status: StatusCode,
        target: Url,
        carries_key: bool,
        redirects: u32,
    ) -> Result<Hop, String> {
        if redirects >= self.max_redirects {
            return Err(format!("the limit of {} redirects", self.max_redirects));
        }
        let same_origin = target.origin() == hop.url.origin();
        if carries_key && !same_origin {
            return Err(format!(
                "it leaves the host the key is for, for {}",
                clamped(target.to_string(), MAX_NOTE / 2)
            ));
        }
        let becomes_get = matches!(status.as_u16(), 301..=303) && hop.method != Method::Head;
        let method = if becomes_get { Method::Get } else { hop.method };
        if !self.methods.contains(&method) {
            return Err(format!("{} isn't allowed", method.as_str()));
        }
        Ok(Hop {
            url: target,
            method,
            headers: if same_origin {
                hop.headers.clone()
            } else {
                HeaderMap::new()
            },
            body: if becomes_get { None } else { hop.body.clone() },
        })
    }

    async fn send(
        &self,
        hop: &Hop,
        credential: Option<&ResolvedCredential>,
    ) -> Result<Response, ToolFailure> {
        let method = reqwest::Method::from_bytes(hop.method.as_str().as_bytes())
            .map_err(|_| refuse("that isn't a method"))?;
        let mut request = self
            .client
            .request(method, hop.url.clone())
            .headers(hop.headers.clone());
        if let Some(credential) = credential {
            request = request.header(credential.header.clone(), credential.value.clone());
        }
        if let Some(body) = &hop.body {
            request = request.body(body.clone());
        }
        request.send().await.map_err(|error| describe(&error))
    }

    /// Reads up to the limit. True if there was more.
    async fn read_bounded(&self, response: &mut Response) -> Result<(Vec<u8>, bool), ToolFailure> {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| describe(&error))? {
            let room = self.max_read_bytes.saturating_sub(bytes.len());
            if chunk.len() > room {
                bytes.extend_from_slice(chunk.get(..room).unwrap_or_default());
                return Ok((bytes, true));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((bytes, false))
    }

    /// Follows the redirects it may. Each hop's address is checked first.
    async fn follow(
        &self,
        first: Hop,
    ) -> Result<(Response, Url, u32, Option<String>), ToolFailure> {
        let credential = self.credential_for(&first.url)?;
        let mut hop = first;
        let mut redirects = 0;
        loop {
            self.check_url(&hop.url)?;
            let response = self.send(&hop, credential).await?;
            let status = response.status();
            let location = response.headers().get(LOCATION);
            let target = match location {
                Some(value) if is_redirect(status) => value
                    .to_str()
                    .ok()
                    .and_then(|text| hop.url.join(text).ok())
                    .ok_or_else(|| refuse("the redirect's address isn't valid"))?,
                _ => return Ok((response, hop.url, redirects, None)),
            };
            match self.next_hop(&hop, status, target, credential.is_some(), redirects) {
                Ok(next) => {
                    hop = next;
                    redirects += 1;
                }
                Err(reason) => return Ok((response, hop.url, redirects, Some(reason))),
            }
        }
    }

    async fn fetch(&self, args: HttpArgs) -> Result<ToolOutput, ToolFailure> {
        if !self.methods.contains(&args.method) {
            return Err(refuse(format!(
                "{} isn't allowed here; allowed: {}",
                args.method.as_str(),
                self.method_names()
            )));
        }
        if args.body.is_some() && args.method.is_read() {
            return Err(refuse("a GET or HEAD can't carry a body"));
        }
        let first = Hop {
            url: Url::parse(&args.url).map_err(|_| refuse("that isn't a valid URL"))?,
            method: args.method,
            headers: self.request_headers(&args.headers)?,
            body: args.body,
        };
        let (mut response, url, redirects, not_followed) = self.follow(first).await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| clamped(value.to_owned(), 256));
        let mut result = HttpResult {
            status,
            url: clamped(self.scrub(url.to_string()), MAX_URL),
            content_type,
            title: None,
            body: None,
            truncated: false,
            omitted_reason: None,
            decoding_errors: false,
            redirects,
            redirect_not_followed: not_followed.map(|reason| clamped(self.scrub(reason), MAX_NOTE)),
        };
        if args.method != Method::Head {
            self.read_body(&mut response, args.mode, &mut result)
                .await?;
        }
        Ok(ToolOutput {
            value: serde_json::to_value(result)
                .map_err(|error| ToolFailure(schema::bounded(&error.to_string())))?,
            trust: Trust::Untrusted,
            cost_usd_micros: 0,
        })
    }

    /// Fills in the body, its title, and whatever kept it from being whole.
    async fn read_body(
        &self,
        response: &mut Response,
        mode: Mode,
        result: &mut HttpResult,
    ) -> Result<(), ToolFailure> {
        let kind = result.content_type.as_deref().map(essence);
        if let Some(kind) = kind.as_deref().filter(|kind| !is_text(kind)) {
            result.omitted_reason = Some(clamped(format!("{kind} isn't text"), MAX_NOTE));
            return Ok(());
        }
        let (bytes, cut) = self.read_bounded(response).await?;
        if kind.is_none() && bytes.contains(&0) {
            result.omitted_reason = Some("the response looks binary".to_owned());
            return Ok(());
        }
        let (text, decoding_errors) = decode(bytes, cut);
        result.decoding_errors = decoding_errors;
        let readable = mode == Mode::Text && kind.as_deref().is_some_and(is_html);
        let (title, text) = if readable {
            tokio::task::spawn_blocking(move || readable_text(&text))
                .await
                .map_err(|_| ToolFailure("reading the page failed".to_owned()))?
        } else {
            (None, text)
        };
        if readable && text.is_empty() {
            result.omitted_reason =
                Some("the page has no readable text; it may need JavaScript".to_owned());
        }
        result.title = title.map(|title| clamped(self.scrub(title), MAX_TITLE));
        let mut text = self.scrub(text);
        let shortened = clamp_chars(&mut text, self.max_body_chars);
        result.truncated = cut || shortened;
        result.body = Some(text).filter(|text| !text.is_empty());
        Ok(())
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn definition(&self) -> ToolDefinition {
        let description = format!(
            "Makes an HTTP request and returns the response as untrusted text. \
             Allowed methods: {}. Internal addresses are refused.",
            self.method_names()
        );
        ToolDefinition::for_types::<HttpArgs, HttpResult>(
            "http_request",
            description,
            Effect::Write,
        )
    }

    fn effect_of(&self, arguments: &Value) -> Effect {
        match arguments.get("method").and_then(Value::as_str) {
            None | Some("GET" | "HEAD") => Effect::Read,
            Some(_) => Effect::Write,
        }
    }

    async fn run(&self, arguments: Value, _: ToolContext) -> Result<ToolOutput, ToolFailure> {
        let args: HttpArgs = serde_json::from_value(arguments)
            .map_err(|error| ToolFailure(schema::bounded(&error.to_string())))?;
        tokio::time::timeout(self.timeout, self.fetch(args))
            .await
            .unwrap_or_else(|_| {
                Err(ToolFailure(format!(
                    "the call took longer than {} seconds",
                    self.timeout.as_secs()
                )))
            })
    }
}
