//! Validation of untrusted values. Schema first, then serde, so a value that
//! reaches a Rust type has already met its bounds.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::schema_of;

/// The most problems reported for one value.
const MAX_PROBLEMS: usize = 10;
/// The longest problem message. A message can quote the value, and the value
/// is untrusted and may be huge.
const MAX_MESSAGE_CHARS: usize = 200;

/// A schema that could not be compiled.
#[derive(Debug, thiserror::Error)]
#[error("invalid schema: {0}")]
pub struct InvalidSchema(String);

/// One way a value failed its schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Where in the value, as a JSON pointer.
    pub path: String,
    /// What is wrong there, cut to a bounded length.
    pub message: String,
}

/// Why a value was refused. It's structured so that a model can act on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}{}", summary(.problems), if *.truncated { " (more problems not shown)" } else { "" })]
pub struct Refusal {
    /// The problems found, at most a fixed number of them.
    pub problems: Vec<Problem>,
    /// True when more problems existed than are listed.
    pub truncated: bool,
}

fn summary(problems: &[Problem]) -> String {
    let parts: Vec<String> = problems
        .iter()
        .map(|p| format!("{}: {}", p.path, p.message))
        .collect();
    parts.join("; ")
}

/// A compiled schema.
#[derive(Debug)]
pub struct Validator(jsonschema::Validator);

impl Validator {
    /// Compiles the schema of `T`.
    ///
    /// # Errors
    ///
    /// [`InvalidSchema`] if the generated schema doesn't compile.
    pub fn for_type<T: JsonSchema>() -> Result<Self, InvalidSchema> {
        Self::from_schema(&schema_of::<T>())
    }

    /// Compiles a schema given as JSON. Formats (`uuid`, `date-time`) are
    /// enforced, and nothing is fetched over a network.
    ///
    /// # Errors
    ///
    /// [`InvalidSchema`] if `schema` isn't a valid JSON Schema.
    pub fn from_schema(schema: &Value) -> Result<Self, InvalidSchema> {
        jsonschema::options()
            .should_validate_formats(true)
            .build(schema)
            .map(Self)
            .map_err(|error| InvalidSchema(error.to_string()))
    }

    /// Checks `value` against the schema.
    ///
    /// # Errors
    ///
    /// [`Refusal`] listing what failed.
    pub fn check(&self, value: &Value) -> Result<(), Refusal> {
        let mut problems = Vec::new();
        let mut truncated = false;
        for error in self.0.iter_errors(value) {
            if problems.len() == MAX_PROBLEMS {
                truncated = true;
                break;
            }
            problems.push(Problem {
                path: error.instance_path().to_string(),
                message: bounded(&error.to_string()),
            });
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Refusal {
                problems,
                truncated,
            })
        }
    }

    /// Checks `value`, then converts it to `T`.
    ///
    /// # Errors
    ///
    /// [`Refusal`] if the value fails the schema or the conversion.
    pub fn parse<T: DeserializeOwned>(&self, value: Value) -> Result<T, Refusal> {
        self.check(&value)?;
        serde_json::from_value(value).map_err(|error| Refusal {
            problems: vec![Problem {
                path: String::new(),
                message: bounded(&error.to_string()),
            }],
            truncated: false,
        })
    }
}

/// Cuts text that may be untrusted and huge to a length safe to show.
#[must_use]
pub fn bounded(message: &str) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        return message.to_owned();
    }
    let mut cut: String = message.chars().take(MAX_MESSAGE_CHARS).collect();
    cut.push_str("… (cut)");
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, JsonSchema, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Args {
        #[schemars(length(max = 5))]
        name: String,
        count: Option<u32>,
    }

    fn validator() -> Validator {
        Validator::for_type::<Args>().unwrap()
    }

    #[test]
    fn a_good_value_parses() {
        let args: Args = validator().parse(json!({ "name": "ok" })).unwrap();
        assert_eq!(
            args,
            Args {
                name: "ok".to_owned(),
                count: None
            }
        );
    }

    #[test]
    fn an_unknown_key_is_refused_with_a_message() {
        let refusal = validator()
            .check(&json!({ "name": "ok", "extra": 1 }))
            .unwrap_err();
        assert_eq!(refusal.problems.len(), 1);
        assert!(refusal.problems[0].message.contains("extra"));
    }

    #[test]
    fn a_wrong_type_is_refused_at_its_path() {
        let refusal = validator().check(&json!({ "name": 7 })).unwrap_err();
        assert_eq!(refusal.problems[0].path, "/name");
    }

    #[test]
    fn a_huge_value_does_not_produce_a_huge_message() {
        let huge = "x".repeat(100_000);
        let refusal = validator().check(&json!({ "name": huge })).unwrap_err();
        assert!(refusal.problems[0].message.chars().count() < 300);
    }

    #[test]
    fn multibyte_text_is_cut_on_a_character_boundary() {
        let cut = bounded(&"é".repeat(1000));
        assert!(cut.ends_with("(cut)"));
    }

    #[test]
    fn many_problems_are_capped_and_say_so() {
        let schema = json!({ "type": "array", "items": { "type": "string" } });
        let validator = Validator::from_schema(&schema).unwrap();
        let refusal = validator.check(&json!(vec![1; 50])).unwrap_err();
        assert_eq!(refusal.problems.len(), MAX_PROBLEMS);
        assert!(refusal.truncated);
        assert!(refusal.to_string().contains("not shown"));
    }

    #[test]
    fn formats_are_enforced() {
        let schema = json!({ "type": "string", "format": "date-time" });
        let validator = Validator::from_schema(&schema).unwrap();
        assert!(validator.check(&json!("2026-11-10T00:00:00Z")).is_ok());
        assert!(validator.check(&json!("yesterday")).is_err());
    }
}
