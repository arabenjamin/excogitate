//! Reading a YAML file that must meet a schema: the one way every
//! configuration and manifest is read.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{Refusal, Validator, bounded};

/// A YAML text that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum YamlError {
    /// It isn't YAML.
    #[error("it isn't valid YAML: {0}")]
    NotYaml(String),
    /// It's YAML, and fails its schema.
    #[error("it fails its schema: {0}")]
    Schema(Refusal),
    /// The type's schema can't be used. A bug, not the file's fault.
    #[error("its schema is unusable: {0}")]
    Unusable(String),
}

/// Reads `text` as YAML, checks it against `T`'s schema, and converts it.
///
/// # Errors
///
/// [`YamlError`] if it isn't YAML or doesn't meet the schema.
pub fn parse_yaml<T: JsonSchema + DeserializeOwned>(text: &str) -> Result<T, YamlError> {
    let raw: Value = serde_saphyr::from_str(text)
        .map_err(|error| YamlError::NotYaml(bounded(&error.to_string())))?;
    Validator::for_type::<T>()
        .map_err(|error| YamlError::Unusable(bounded(&error.to_string())))?
        .parse(raw)
        .map_err(YamlError::Schema)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Small {
        #[schemars(range(max = 9))]
        n: u32,
    }

    #[test]
    fn yaml_meeting_its_schema_is_read() {
        assert_eq!(parse_yaml::<Small>("n: 3").unwrap(), Small { n: 3 });
    }

    #[test]
    fn each_way_of_being_wrong_says_which() {
        assert!(matches!(
            parse_yaml::<Small>("n: [").unwrap_err(),
            YamlError::NotYaml(_)
        ));
        assert!(matches!(
            parse_yaml::<Small>("n: 10").unwrap_err(),
            YamlError::Schema(_)
        ));
        assert!(matches!(
            parse_yaml::<Small>("m: 1").unwrap_err(),
            YamlError::Schema(_)
        ));
    }
}
