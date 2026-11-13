//! How the owner configures `http_request`. The model can't touch any of it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The longest text `max_body_chars` can allow.
pub const MAX_BODY_CHARS: usize = 200_000;

/// An HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    /// Reads.
    #[default]
    Get,
    /// Reads the headers only.
    Head,
    /// Creates or submits.
    Post,
    /// Replaces.
    Put,
    /// Changes part of something.
    Patch,
    /// Removes.
    Delete,
}

impl Method {
    /// The name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    /// True if it only reads.
    #[must_use]
    pub const fn is_read(self) -> bool {
        matches!(self, Self::Get | Self::Head)
    }
}

/// A key the runtime attaches to requests for one host. The model never sees
/// its value. It goes in a header or, for a service that takes it no other
/// way, a query parameter: one of the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// The exact host it's for. A subdomain doesn't count.
    #[schemars(length(min = 1, max = 253))]
    pub host: String,
    /// The header that carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    pub header: Option<String>,
    /// The query parameter that carries it, instead of a header. A caller
    /// that sets this parameter itself is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    pub query: Option<String>,
    /// Methods allowed for this host only, besides `allowed_methods`: POST
    /// to a search API, without letting a model POST anywhere else.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 6))]
    pub methods: Vec<Method>,
    /// Text before the key, such as `Bearer `.
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub prefix: String,
    /// The environment variable holding the key.
    #[schemars(length(min = 1, max = 128))]
    pub env: String,
    /// Allow sending the key over plain http.
    #[serde(default)]
    pub allow_insecure: bool,
}

/// Configuration for `http_request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct HttpConfig {
    /// Address ranges it may not reach, in CIDR form.
    #[schemars(length(min = 1, max = 256), inner(length(max = 64)))]
    pub blocked_ranges: Vec<String>,
    /// Ranges it may reach even if blocked, such as a service on your own
    /// network.
    #[schemars(length(max = 256), inner(length(max = 64)))]
    pub allowed_ranges: Vec<String>,
    /// The methods a model may use, for every host. Any method but GET and
    /// HEAD can send data out; to allow one for a single host, give it in
    /// that host's credential (`methods`) instead.
    #[schemars(length(min = 1, max = 6))]
    pub allowed_methods: Vec<Method>,
    /// Keys to attach, by host.
    #[schemars(length(max = 64))]
    pub credentials: Vec<Credential>,
    /// The most one call may take, redirects and reading included.
    #[schemars(range(min = 1, max = 300))]
    pub timeout_seconds: u64,
    /// The most of a response it reads.
    #[schemars(range(min = 1, max = 67_108_864))]
    pub max_read_bytes: usize,
    /// The most text it returns.
    #[schemars(range(min = 1, max = MAX_BODY_CHARS))]
    pub max_body_chars: usize,
    /// How many redirects it follows.
    #[schemars(range(max = 10))]
    pub max_redirects: u32,
    /// The `User-Agent` it sends. Left at the default, the server sends the
    /// program's name instead.
    #[schemars(length(min = 1, max = 256))]
    pub user_agent: String,
}

/// Loopback, private, link-local and other ranges that aren't the public
/// internet, including the IPv6 forms that can carry an IPv4 address.
const DEFAULT_BLOCKED: [&str; 20] = [
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
    "::/96",
    "fc00::/7",
    "fe80::/10",
    "fec0::/10",
    "ff00::/8",
    "64:ff9b::/96",
    "64:ff9b:1::/48",
    "2001::/32",
    "2002::/16",
];

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            blocked_ranges: DEFAULT_BLOCKED.map(str::to_owned).to_vec(),
            allowed_ranges: Vec::new(),
            allowed_methods: vec![Method::Get, Method::Head],
            credentials: Vec::new(),
            timeout_seconds: 30,
            max_read_bytes: 4 * 1024 * 1024,
            max_body_chars: 50_000,
            max_redirects: 5,
            user_agent: "runtime".to_owned(),
        }
    }
}

/// The configuration can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HttpConfigError {
    /// It doesn't meet its schema.
    #[error("http configuration is invalid: {0}")]
    Invalid(String),
    /// A range isn't valid CIDR.
    #[error("not a valid address range: {0}")]
    Range(String),
    /// A credential's variable isn't set, or is empty.
    #[error("environment variable {0} isn't set")]
    MissingSecret(String),
    /// A credential can't be sent: its header or key isn't valid, or it
    /// names both a header and a query parameter, or neither.
    #[error(
        "the credential for {0} can't be sent: it needs a valid header or query parameter, one of them"
    )]
    BadCredential(String),
    /// The HTTP client couldn't be built.
    #[error("couldn't build the http client: {0}")]
    Client(String),
}
