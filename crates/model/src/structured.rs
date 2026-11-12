//! Typed values from model text: find the first complete JSON object, check
//! it against the schema, allow one repair, and then fail visibly.

use schema::{Validator, bounded, check_strict, schema_of};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Deserializer, Value};

use crate::{
    ChatRequest, Completion, FinishReason, Gateway, GatewayError, Message, ResponseSchema,
    RoutePolicy,
};

/// How many opening braces are tried before giving up. Each failed start
/// costs at most the rest of the text, so this bounds hostile input.
const MAX_START_POINTS: usize = 64;

/// How much of a refused reply is sent back for repair.
const MAX_REPAIR_ECHO_CHARS: usize = 4000;

/// The first complete JSON object in `text`, if any. Prose around it is
/// ignored, and so is a brace that doesn't begin an object.
#[must_use]
pub fn first_json_object(text: &str) -> Option<Value> {
    text.match_indices('{')
        .take(MAX_START_POINTS)
        .find_map(|(start, _)| {
            let rest = text.get(start..)?;
            let mut stream = Deserializer::from_str(rest).into_iter::<Value>();
            stream.next()?.ok().filter(Value::is_object)
        })
}

/// A typed value from a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Structured<T> {
    /// The value.
    pub value: T,
    /// The call that produced it.
    pub completion: Completion,
    /// True if the first reply was refused and a repair was needed.
    pub repaired: bool,
    /// What every call cost, in millionths of a dollar.
    pub total_cost_usd_micros: u64,
}

/// No usable value came back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StructuredError {
    /// The model couldn't be called.
    #[error(transparent)]
    Gateway(#[from] GatewayError),
    /// The schema can't be used. A bug in the caller.
    #[error("the output schema is unusable: {0}")]
    BadSchema(String),
    /// The first reply was refused, and the call to repair it failed.
    #[error("the model's reply was refused: {first}; then the repair call failed: {error}")]
    RepairFailed {
        /// Why the first reply was refused.
        first: String,
        /// Why the repair call failed.
        error: GatewayError,
        /// What the first call cost, in millionths of a dollar.
        cost_usd_micros: u64,
    },
    /// Both replies were refused.
    #[error("the model's reply was refused twice: {first}; then: {repair}")]
    Refused {
        /// Why the first reply was refused.
        first: String,
        /// Why the repair was refused.
        repair: String,
        /// What the two calls cost, in millionths of a dollar.
        cost_usd_micros: u64,
    },
}

impl StructuredError {
    /// What was spent before it failed, in millionths of a dollar. It's
    /// spent all the same, so a caller keeping accounts must count it.
    #[must_use]
    pub const fn cost_usd_micros(&self) -> u64 {
        match self {
            Self::Gateway(_) | Self::BadSchema(_) => 0,
            Self::RepairFailed {
                cost_usd_micros, ..
            }
            | Self::Refused {
                cost_usd_micros, ..
            } => *cost_usd_micros,
        }
    }
}

fn read_reply<T: DeserializeOwned>(
    completion: &Completion,
    validator: &Validator,
) -> Result<T, String> {
    let reply = &completion.response;
    if reply.content.trim().is_empty() {
        return Err("the reply was empty".to_owned());
    }
    let Some(value) = first_json_object(&reply.content) else {
        return Err(if reply.finish == FinishReason::Length {
            "the reply was cut off before a complete JSON object".to_owned()
        } else {
            "the reply has no complete JSON object".to_owned()
        });
    };
    validator
        .parse(value)
        .map_err(|refusal| refusal.to_string())
}

/// Calls a model and returns a `T`. The request is sent with `T`'s schema;
/// a reply that doesn't meet it gets one repair attempt.
///
/// # Errors
///
/// [`StructuredError`] if `T`'s schema isn't strict, the model can't be
/// called, or both replies are refused.
pub async fn generate_structured<T: JsonSchema + DeserializeOwned>(
    gateway: &Gateway,
    policy: &RoutePolicy,
    mut request: ChatRequest,
    name: &str,
) -> Result<Structured<T>, StructuredError> {
    let schema = schema_of::<T>();
    if let Some(violation) = check_strict(&schema).first() {
        return Err(StructuredError::BadSchema(violation.to_string()));
    }
    let validator = Validator::from_schema(&schema)
        .map_err(|error| StructuredError::BadSchema(error.to_string()))?;
    request.response_schema = Some(ResponseSchema {
        name: name.to_owned(),
        schema,
    });

    let first = gateway.chat(policy, &request).await?;
    let mut total_cost_usd_micros = first.cost_usd_micros;
    let first_problem = match read_reply::<T>(&first, &validator) {
        Ok(value) => {
            return Ok(Structured {
                value,
                completion: first,
                repaired: false,
                total_cost_usd_micros,
            });
        }
        Err(problem) => problem,
    };

    let echoed: String = first
        .response
        .content
        .chars()
        .take(MAX_REPAIR_ECHO_CHARS)
        .collect();
    request.messages.push(Message::assistant(echoed));
    request.messages.push(Message::user(format!(
        "That reply was refused: {}. Reply with one JSON object that meets the schema, and nothing else.",
        bounded(&first_problem)
    )));
    let mut repair_policy = policy.clone();
    repair_policy.budget_usd_micros = policy
        .budget_usd_micros
        .map(|left| left.saturating_sub(first.cost_usd_micros));

    let second = match gateway.chat(&repair_policy, &request).await {
        Ok(second) => second,
        Err(error) => {
            return Err(StructuredError::RepairFailed {
                first: first_problem,
                error,
                cost_usd_micros: total_cost_usd_micros,
            });
        }
    };
    total_cost_usd_micros = total_cost_usd_micros.saturating_add(second.cost_usd_micros);
    match read_reply::<T>(&second, &validator) {
        Ok(value) => Ok(Structured {
            value,
            completion: second,
            repaired: true,
            total_cost_usd_micros,
        }),
        Err(repair) => Err(StructuredError::Refused {
            first: first_problem,
            repair,
            cost_usd_micros: total_cost_usd_micros,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::catalog::test_support::catalog;
    use crate::gateway::test_support::{Scripted, text};
    use crate::{MemoryLedger, ModelError, Role};
    use proptest::prelude::*;
    use protocol::DataClass;
    use serde::Deserialize;
    use serde_json::json;
    use std::sync::Arc;

    #[derive(Debug, Deserialize, JsonSchema, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Verdict {
        #[schemars(length(max = 8))]
        label: String,
        score: u32,
    }

    /// Accepts any string, so it fails the strictness check.
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Loose {
        #[allow(dead_code)]
        free: String,
    }

    const YAML: &str = "
version: 1
providers:
  - {name: p, base_url: 'http://a/v1', no_log: true, timeout_secs: 5}
models:
  - {name: m, provider: p, wire_model: a, capabilities: [chat],
     context_tokens: 9000, max_output_tokens: 100}
";

    fn policy() -> RoutePolicy {
        RoutePolicy {
            capabilities: vec![],
            data_class: DataClass::Private,
            purpose: "work".to_owned(),
            budget_usd_micros: None,
            embedding_dimension: None,
            only_model: None,
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user("judge this")],
            tools: vec![],
            max_output_tokens: 50,
            temperature: None,
            response_schema: None,
        }
    }

    async fn run(
        replies: Vec<Result<crate::ChatResponse, ModelError>>,
    ) -> (Result<Structured<Verdict>, StructuredError>, Arc<Scripted>) {
        let client = Arc::new(Scripted::new(replies));
        let gateway = Gateway::new(
            catalog(YAML),
            Arc::clone(&client) as Arc<dyn crate::ModelClient>,
            Arc::new(MemoryLedger::new()),
        );
        let result = generate_structured(&gateway, &policy(), request(), "verdict").await;
        (result, client)
    }

    const GOOD: &str = r#"{"label":"ok","score":1}"#;

    #[test]
    fn prose_around_the_object_is_ignored() {
        let text = format!("Sure! Here: {GOOD} Hope that helps {{not json}}");
        assert_eq!(
            first_json_object(&text),
            Some(json!({"label":"ok","score":1}))
        );
    }

    #[test]
    fn the_first_of_two_objects_wins_not_the_span_of_both() {
        assert_eq!(
            first_json_object(r#"{"a":1} and {"b":2}"#),
            Some(json!({"a":1}))
        );
    }

    #[test]
    fn a_stray_brace_before_the_object_is_skipped() {
        assert_eq!(
            first_json_object(r#"use {x} then {"a":1}"#),
            Some(json!({"a":1}))
        );
    }

    #[test]
    fn braces_in_strings_do_not_confuse_it() {
        assert_eq!(first_json_object(r#"{"a":"}{"}"#), Some(json!({"a":"}{"})));
    }

    #[test]
    fn nested_objects_stay_whole() {
        assert_eq!(
            first_json_object(r#"x {"a":{"b":1}} y"#),
            Some(json!({"a":{"b":1}}))
        );
    }

    #[test]
    fn nothing_complete_means_none() {
        assert_eq!(first_json_object(r#"{"a":"#), None);
        assert_eq!(first_json_object("no json at all"), None);
        assert_eq!(first_json_object(""), None);
    }

    #[test]
    fn hostile_nesting_returns_quickly() {
        assert_eq!(first_json_object(&"{".repeat(1_000_000)), None);
        assert_eq!(first_json_object(&r#"{"a":"#.repeat(200_000)), None);
    }

    proptest! {
        #[test]
        fn no_text_panics_it(text in "\\PC*") {
            let _ = first_json_object(&text);
        }

        #[test]
        fn an_object_in_prose_is_found(before in "[a-z é]*", after in "[a-z é}]*", n in any::<u32>()) {
            let text = format!("{before}{{\"n\":{n}}}{after}");
            prop_assert_eq!(first_json_object(&text), Some(json!({ "n": n })));
        }
    }

    #[tokio::test]
    async fn a_good_reply_is_typed_at_once() {
        let (result, client) = run(vec![Ok(text(GOOD))]).await;
        let done = result.unwrap();
        assert_eq!(
            done.value,
            Verdict {
                label: "ok".to_owned(),
                score: 1
            }
        );
        assert!(!done.repaired);
        assert_eq!(client.called().len(), 1);
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls[0].1.response_schema.as_ref().unwrap().name, "verdict");
    }

    #[tokio::test]
    async fn one_repair_is_allowed_and_says_what_was_wrong() {
        let bad = r#"{"label":"ok","score":1,"extra":true}"#;
        let (result, client) = run(vec![Ok(text(bad)), Ok(text(GOOD))]).await;
        assert!(result.unwrap().repaired);
        let calls = client.calls.lock().unwrap();
        let messages = &calls[1].1.messages;
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].content, bad);
        assert!(
            messages[2].content.contains("extra"),
            "{}",
            messages[2].content
        );
    }

    #[tokio::test]
    async fn two_refusals_fail_visibly_with_both_reasons() {
        let (result, client) =
            run(vec![Ok(text("no json")), Ok(text(r#"{"score":"high"}"#))]).await;
        let error = result.unwrap_err();
        let StructuredError::Refused { first, repair, .. } = &error else {
            panic!("{error:?}");
        };
        assert!(first.contains("no complete JSON"));
        assert_ne!(repair, "");
        assert_eq!(client.called().len(), 2);
    }

    #[tokio::test]
    async fn a_repair_call_that_fails_keeps_the_first_reason_and_its_cost() {
        let refused = ModelError::Status {
            code: 400,
            excerpt: String::new(),
        };
        let (result, client) = run(vec![Ok(text("no json")), Err(refused)]).await;
        let error = result.unwrap_err();
        let StructuredError::RepairFailed { first, .. } = &error else {
            panic!("{error:?}");
        };
        assert!(first.contains("no complete JSON"));
        assert_eq!(error.cost_usd_micros(), 0, "this model is free");
        assert_eq!(client.called().len(), 2);
    }

    #[tokio::test]
    async fn an_empty_reply_says_it_was_empty() {
        // A reasoning model can spend its turn thinking and say nothing.
        let (result, _) = run(vec![Ok(text("")), Ok(text("  "))]).await;
        let StructuredError::Refused { first, repair, .. } = result.unwrap_err() else {
            panic!();
        };
        assert_eq!(first, "the reply was empty");
        assert_eq!(repair, "the reply was empty");
    }

    #[tokio::test]
    async fn over_long_values_are_refused() {
        let long = r#"{"label":"much too long for it","score":1}"#;
        let (result, _) = run(vec![Ok(text(long)), Ok(text(long))]).await;
        assert!(matches!(result, Err(StructuredError::Refused { .. })));
    }

    #[tokio::test]
    async fn a_cut_off_reply_says_so() {
        let mut cut = text(r#"{"label":"#);
        cut.finish = FinishReason::Length;
        let (result, _) = run(vec![Ok(cut), Ok(text("x"))]).await;
        let StructuredError::Refused { first, .. } = result.unwrap_err() else {
            panic!();
        };
        assert!(first.contains("cut off"));
    }

    #[tokio::test]
    async fn a_loose_output_type_never_reaches_a_model() {
        let client = Arc::new(Scripted::new([]));
        let gateway = Gateway::new(
            catalog(YAML),
            Arc::clone(&client) as Arc<dyn crate::ModelClient>,
            Arc::new(MemoryLedger::new()),
        );
        let result = generate_structured::<Loose>(&gateway, &policy(), request(), "loose").await;
        assert!(matches!(result, Err(StructuredError::BadSchema(_))));
        assert_eq!(client.called(), [] as [std::string::String; 0]);
    }
}
