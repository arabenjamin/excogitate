//! The models configuration, and the checked catalog built from it.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use schema::{Refusal, YamlError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const ONE_MILLION: u128 = 1_000_000;

/// The contents of `models.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    /// The format version.
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    /// The endpoints.
    #[schemars(length(max = 32))]
    pub providers: Vec<ProviderConfig>,
    /// The models they serve. Among equal prices, earlier ones are preferred.
    #[schemars(length(max = 128))]
    pub models: Vec<ModelConfig>,
    /// Named lists of models, each in the order to try them. A call that
    /// asks for a role tries its models in that order instead of cheapest
    /// first: what's measured best for a job need not be what's cheapest.
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub roles: Vec<RoleConfig>,
}

/// One role: a job, and the models that do it, best first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    /// Its name, such as `writer`. Agents and the runtime ask for it.
    #[schemars(length(min = 1, max = 32), regex(pattern = r"^[a-z][a-z0-9_-]*$"))]
    pub name: String,
    /// The models, in the order to try them. Each must be defined above.
    #[schemars(length(min = 1, max = 16), inner(length(min = 1, max = 64)))]
    pub models: Vec<String>,
}

/// One endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Its name.
    #[schemars(length(min = 1, max = 64))]
    pub name: String,
    /// The URL that `/chat/completions` and `/embeddings` hang off.
    #[schemars(length(min = 1, max = 512))]
    pub base_url: String,
    /// The environment variable holding its API key, if it needs one.
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Z][A-Z0-9_]*$"))]
    pub api_key_env: Option<String>,
    /// Whether it keeps no logs of what it's sent. Only then may private work
    /// go to it. Required, so nobody gets this by default.
    pub no_log: bool,
    /// How long one request may take.
    #[schemars(range(min = 1, max = 3600))]
    pub timeout_secs: u32,
    /// The most it may spend per UTC day. Required for a priced model.
    pub spend_cap_usd_micros_per_day: Option<u64>,
    /// The purposes that may use its priced models.
    #[serde(default)]
    #[schemars(length(max = 32), inner(length(min = 1, max = 64)))]
    pub paid_for: Vec<String>,
}

/// What a model can do. Agents ask for these, never for a model by name.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Chat completions.
    Chat,
    /// Tool calls.
    Tools,
    /// The server enforces a JSON Schema on the reply.
    JsonSchema,
    /// Embeddings.
    Embeddings,
    /// Measured able to run a multi-step investigative loop.
    Investigative,
}

/// What a model costs, in millionths of a dollar per million tokens.
/// Integers, so that spend adds up exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// Per million input tokens.
    pub input_usd_micros_per_mtok: u64,
    /// Per million output tokens.
    pub output_usd_micros_per_mtok: u64,
}

impl Price {
    /// What this many tokens cost, in millionths of a dollar, rounded up.
    #[must_use]
    pub fn cost(&self, input_tokens: u32, output_tokens: u32) -> u64 {
        let part =
            |tokens: u32, rate: u64| (u128::from(tokens) * u128::from(rate)).div_ceil(ONE_MILLION);
        let total = part(input_tokens, self.input_usd_micros_per_mtok)
            + part(output_tokens, self.output_usd_micros_per_mtok);
        u64::try_from(total).unwrap_or(u64::MAX)
    }

    fn is_free(self) -> bool {
        self == Self::default()
    }
}

/// One model, as configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// The name used in receipts and logs.
    #[schemars(length(min = 1, max = 64))]
    pub name: String,
    /// The provider serving it.
    #[schemars(length(min = 1, max = 64))]
    pub provider: String,
    /// The id the endpoint knows it by.
    #[schemars(length(min = 1, max = 256))]
    pub wire_model: String,
    /// What it can do.
    #[schemars(length(min = 1, max = 8))]
    pub capabilities: Vec<Capability>,
    /// Its context window.
    #[schemars(range(min = 1))]
    pub context_tokens: u32,
    /// The most it may write in one reply.
    #[schemars(range(min = 1))]
    pub max_output_tokens: u32,
    /// The width of its embeddings. Required with the `embeddings` capability.
    #[schemars(range(min = 1, max = 65535))]
    pub embedding_dimension: Option<u32>,
    /// What it costs. Absent means free.
    pub price: Option<Price>,
}

/// The models configuration could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file isn't YAML.
    #[error("models config isn't valid YAML: {0}")]
    Yaml(String),
    /// The file doesn't meet its schema.
    #[error("models config fails its schema: {0}")]
    Schema(#[from] Refusal),
    /// The file meets its schema but doesn't make sense.
    #[error("models config: {0}")]
    Invalid(String),
}

/// An API key. It can't be printed or serialized, by accident or otherwise.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(redacted)")
    }
}

/// A usable endpoint. Its key belongs to it, so nothing can send the key to
/// a host of its choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// Its name.
    pub name: String,
    /// The URL requests hang off, without a trailing slash.
    pub base_url: String,
    /// Its key, if it needs one.
    pub api_key: Option<ApiKey>,
    /// Whether it keeps no logs.
    pub no_log: bool,
    /// How long one request may take.
    pub timeout: Duration,
    /// The most it may spend per UTC day.
    pub daily_cap_usd_micros: Option<u64>,
    /// The purposes that may use its priced models.
    pub paid_for: Vec<String>,
}

/// A usable model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// The name used in receipts and logs.
    pub name: String,
    /// The provider serving it.
    pub provider: Arc<Provider>,
    /// The id the endpoint knows it by.
    pub wire_model: String,
    /// What it can do.
    pub capabilities: BTreeSet<Capability>,
    /// Its context window.
    pub context_tokens: u32,
    /// The most it may write in one reply.
    pub max_output_tokens: u32,
    /// The width of its embeddings.
    pub embedding_dimension: Option<usize>,
    /// What it costs.
    pub price: Price,
}

impl Model {
    /// Whether it can do this.
    #[must_use]
    pub fn can(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Whether using it costs money.
    #[must_use]
    pub fn is_paid(&self) -> bool {
        !self.price.is_free()
    }
}

/// The checked models. Holding one means the configuration made sense.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    models: Vec<Arc<Model>>,
    roles: Vec<(String, Vec<Arc<Model>>)>,
}

impl Catalog {
    /// Reads `models.yaml`, resolving API keys through `env`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] if it isn't YAML, fails its schema, is inconsistent,
    /// or names a key variable that isn't set.
    pub fn from_yaml(
        text: &str,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let config: ModelsConfig = schema::parse_yaml(text).map_err(|error| match error {
            YamlError::NotYaml(problem) => ConfigError::Yaml(problem),
            YamlError::Schema(refusal) => ConfigError::Schema(refusal),
            other => ConfigError::Invalid(other.to_string()),
        })?;
        Self::from_config(config, env)
    }

    /// Checks a parsed configuration.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the first thing that doesn't make sense.
    pub fn from_config(
        config: ModelsConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let mut providers: Vec<Arc<Provider>> = Vec::new();
        for provider in config.providers {
            if providers.iter().any(|known| known.name == provider.name) {
                return Err(invalid(format!("provider {} repeats", provider.name)));
            }
            providers.push(Arc::new(resolve_provider(provider, &env)?));
        }
        let mut names = HashSet::new();
        let mut models = Vec::new();
        for model in config.models {
            if !names.insert(model.name.clone()) {
                return Err(invalid(format!("model {} repeats", model.name)));
            }
            models.push(Arc::new(resolve_model(model, &providers)?));
        }
        let mut roles: Vec<(String, Vec<Arc<Model>>)> = Vec::new();
        for role in config.roles {
            if roles.iter().any(|(name, _)| *name == role.name) {
                return Err(invalid(format!("role {} repeats", role.name)));
            }
            let mut listed: Vec<Arc<Model>> = Vec::new();
            for wanted in &role.models {
                let model = models
                    .iter()
                    .find(|model| model.name == *wanted)
                    .ok_or_else(|| {
                        invalid(format!("role {}: no model named {wanted}", role.name))
                    })?;
                if listed.iter().any(|known| known.name == *wanted) {
                    return Err(invalid(format!("role {}: {wanted} repeats", role.name)));
                }
                listed.push(Arc::clone(model));
            }
            roles.push((role.name, listed));
        }
        Ok(Self { models, roles })
    }

    /// A role's models, best first, if the role is defined.
    #[must_use]
    pub fn role(&self, name: &str) -> Option<&[Arc<Model>]> {
        self.roles
            .iter()
            .find(|(role, _)| role == name)
            .map(|(_, models)| models.as_slice())
    }

    /// The roles' names, in configured order.
    pub fn role_names(&self) -> impl Iterator<Item = &str> {
        self.roles.iter().map(|(name, _)| name.as_str())
    }

    /// The models, in configured order.
    #[must_use]
    pub fn models(&self) -> &[Arc<Model>] {
        &self.models
    }
}

fn invalid(message: String) -> ConfigError {
    ConfigError::Invalid(message)
}

fn resolve_provider(
    config: ProviderConfig,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Provider, ConfigError> {
    let name = config.name;
    let url = Url::parse(&config.base_url)
        .map_err(|error| invalid(format!("provider {name}: bad base_url: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "provider {name}: base_url must be http(s)"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(format!(
            "provider {name}: put credentials in api_key_env, not in base_url"
        )));
    }
    let api_key = match config.api_key_env {
        None => None,
        Some(variable) => match env(&variable) {
            Some(key) if !key.is_empty() => Some(ApiKey(key)),
            _ => {
                return Err(invalid(format!("provider {name}: {variable} isn't set")));
            }
        },
    };
    Ok(Provider {
        base_url: config.base_url.trim_end_matches('/').to_owned(),
        api_key,
        no_log: config.no_log,
        timeout: Duration::from_secs(u64::from(config.timeout_secs)),
        daily_cap_usd_micros: config.spend_cap_usd_micros_per_day,
        paid_for: config.paid_for,
        name,
    })
}

fn resolve_model(config: ModelConfig, providers: &[Arc<Provider>]) -> Result<Model, ConfigError> {
    let name = config.name;
    let provider = providers
        .iter()
        .find(|provider| provider.name == config.provider)
        .ok_or_else(|| {
            invalid(format!(
                "model {name}: no provider named {}",
                config.provider
            ))
        })?;
    let capabilities: BTreeSet<Capability> = config.capabilities.into_iter().collect();
    let embeds = capabilities.contains(&Capability::Embeddings);
    if embeds != config.embedding_dimension.is_some() {
        return Err(invalid(format!(
            "model {name}: embedding_dimension goes with the embeddings capability, and only with it"
        )));
    }
    if capabilities.contains(&Capability::Tools) && !capabilities.contains(&Capability::Chat) {
        return Err(invalid(format!("model {name}: tools needs chat")));
    }
    let price = config.price.unwrap_or_default();
    if !price.is_free() && (provider.daily_cap_usd_micros.is_none() || provider.paid_for.is_empty())
    {
        return Err(invalid(format!(
            "model {name} is priced, so provider {} needs spend_cap_usd_micros_per_day and paid_for",
            provider.name
        )));
    }
    Ok(Model {
        name,
        provider: Arc::clone(provider),
        wire_model: config.wire_model,
        capabilities,
        context_tokens: config.context_tokens,
        max_output_tokens: config.max_output_tokens,
        embedding_dimension: config.embedding_dimension.map(|width| width as usize),
        price,
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A catalog from YAML, with every key set.
    pub(crate) fn catalog(yaml: &str) -> Catalog {
        Catalog::from_yaml(yaml, |_| Some("the-key".to_owned())).unwrap()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../models.example.yaml");

    fn load(yaml: &str) -> Result<Catalog, ConfigError> {
        Catalog::from_yaml(yaml, |_| Some("the-key".to_owned()))
    }

    fn base(provider: &str, model: &str) -> String {
        format!(
            "version: 1\nproviders:\n  - {{name: p, base_url: 'http://h/v1', no_log: true, timeout_secs: 5{provider}}}\n\
             models:\n  - {{name: m, provider: p, wire_model: w, capabilities: [chat], context_tokens: 9, max_output_tokens: 1{model}}}\n"
        )
    }

    #[test]
    fn a_role_must_name_defined_models_once_each() {
        let with = |roles: &str| format!("{}roles:\n{roles}", base("", ""));
        let good = load(&with("  - {name: writer, models: [m]}\n")).unwrap();
        assert_eq!(good.role("writer").unwrap()[0].name, "m");
        assert!(good.role("judge").is_none());
        for (roles, problem) in [
            (
                "  - {name: writer, models: [nope]}\n",
                "no model named nope",
            ),
            ("  - {name: writer, models: [m, m]}\n", "m repeats"),
            (
                "  - {name: writer, models: [m]}\n  - {name: writer, models: [m]}\n",
                "role writer repeats",
            ),
        ] {
            let error = load(&with(roles)).unwrap_err();
            assert!(error.to_string().contains(problem), "{error}");
        }
        // A role's name is a plain word, and it lists at least one model.
        assert!(load(&with("  - {name: Writer, models: [m]}\n")).is_err());
        assert!(load(&with("  - {name: writer, models: []}\n")).is_err());
    }

    #[test]
    fn the_example_file_loads() {
        // It has to start with no keys set, as the quickstart does.
        let catalog = Catalog::from_yaml(EXAMPLE, |_| None).unwrap();
        assert_eq!(catalog.models().len(), 2);
        let local = &catalog.models()[1];
        assert_eq!(local.embedding_dimension, Some(1024));
        assert!(local.provider.api_key.is_none());
    }

    #[test]
    fn the_examples_paid_section_is_valid_once_uncommented() {
        let uncommented: String = EXAMPLE
            .lines()
            .map(|line| match line.strip_prefix("  # ") {
                Some(rest) if rest.starts_with("- ") || rest.starts_with("  ") => {
                    format!("  {rest}\n")
                }
                _ => format!("{line}\n"),
            })
            .collect();
        let catalog = load(&uncommented).unwrap();
        assert_eq!(catalog.models().len(), 3);
        assert!(catalog.models()[2].is_paid());
    }

    #[test]
    fn a_minimal_file_loads_and_trims_the_url() {
        let yaml = base("", "").replace("http://h/v1", "http://h/v1/");
        let catalog = load(&yaml).unwrap();
        assert_eq!(catalog.models()[0].provider.base_url, "http://h/v1");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let yaml = base("", ", surprise: 1");
        assert!(matches!(load(&yaml), Err(ConfigError::Schema(_))));
    }

    #[test]
    fn broken_yaml_is_refused() {
        assert!(matches!(load("a: [b"), Err(ConfigError::Yaml(_))));
    }

    #[test]
    fn no_log_must_be_stated() {
        let yaml = base("", "").replace("no_log: true, ", "");
        assert!(matches!(load(&yaml), Err(ConfigError::Schema(_))));
    }

    #[test]
    fn a_priced_model_needs_a_cap_and_an_allowlist() {
        let price = ", price: {input_usd_micros_per_mtok: 1, output_usd_micros_per_mtok: 1}";
        let error = load(&base("", price)).unwrap_err();
        assert!(error.to_string().contains("spend_cap"));
        let capped = load(&base(
            ", spend_cap_usd_micros_per_day: 5, paid_for: [x]",
            price,
        ));
        assert!(capped.unwrap().models()[0].is_paid());
    }

    #[test]
    fn embeddings_need_a_width_and_only_they_have_one() {
        let error = load(&base("", ", embedding_dimension: 3")).unwrap_err();
        assert!(error.to_string().contains("embedding_dimension"));
        let yaml = base("", "").replace("[chat]", "[embeddings]");
        assert!(load(&yaml).is_err());
    }

    #[test]
    fn a_missing_key_variable_is_named() {
        let yaml = base(", api_key_env: MY_KEY", "");
        let error = Catalog::from_yaml(&yaml, |_| None).unwrap_err();
        assert!(error.to_string().contains("MY_KEY"));
    }

    #[test]
    fn credentials_in_the_url_are_refused() {
        let yaml = base("", "").replace("http://h/v1", "http://user:pw@h/v1");
        assert!(load(&yaml).unwrap_err().to_string().contains("credentials"));
    }

    #[test]
    fn an_unknown_provider_is_refused() {
        let yaml = base("", "").replace("provider: p", "provider: q");
        assert!(load(&yaml).is_err());
    }

    #[test]
    fn a_key_never_prints() {
        let yaml = base(", api_key_env: MY_KEY", "");
        let catalog = Catalog::from_yaml(&yaml, |_| Some("sekret".to_owned())).unwrap();
        assert!(!format!("{catalog:?}").contains("sekret"));
    }

    #[test]
    fn cost_rounds_up_and_does_not_overflow() {
        let price = Price {
            input_usd_micros_per_mtok: 1,
            output_usd_micros_per_mtok: 1_000_000,
        };
        assert_eq!(price.cost(1, 0), 1);
        assert_eq!(price.cost(0, 1_000_000), 1_000_000);
        let huge = Price {
            input_usd_micros_per_mtok: u64::MAX,
            output_usd_micros_per_mtok: u64::MAX,
        };
        assert_eq!(huge.cost(u32::MAX, u32::MAX), u64::MAX);
    }
}
