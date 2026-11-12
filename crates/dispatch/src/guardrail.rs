//! Guardrails: rules on an agent's tools that dispatch enforces, so a rule
//! the agent must keep doesn't depend on the model reading it.

use regex::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most guardrails one agent may have.
pub const MAX_GUARDRAILS: usize = 16;

/// The largest compiled pattern, in bytes, so a pattern can't be made to
/// use a great deal of memory.
const PATTERN_BYTES: usize = 1 << 20;

/// Compiles a pattern that ignores case, held to a size.
///
/// # Errors
///
/// Why it doesn't compile, in a sentence.
pub fn pattern(text: &str) -> Result<Regex, String> {
    RegexBuilder::new(text)
        .case_insensitive(true)
        .size_limit(PATTERN_BYTES)
        .dfa_size_limit(PATTERN_BYTES)
        .build()
        .map_err(|error| schema::bounded(&format!("the pattern {text:?}: {error}")))
}

/// A call refused for what one of its arguments says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Deny {
    /// The argument, as a dotted path such as `operation` or `filter.kind`.
    #[schemars(length(min = 1, max = 128))]
    pub argument: String,
    /// A pattern that ignores case. A call whose argument matches is
    /// refused. A string is matched as it is, anything else as JSON, and a
    /// call without the argument is let through.
    #[schemars(length(min = 1, max = 256))]
    pub matches: String,
}

/// A rule on one of the agent's tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Guardrail {
    /// Its name, unique within the agent. A call it refuses names it.
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-z][a-z0-9_]*$"))]
    pub name: String,
    /// The tool it holds to the rule: one the agent has.
    #[schemars(length(min = 1, max = 64))]
    pub tool: String,
    /// Why, as the model is told when the rule stops it.
    #[schemars(length(min = 1, max = 500))]
    pub because: String,
    /// Refuses a call whose argument matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Deny>,
    /// Refuses the agent's calls past this many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1000))]
    pub max_calls: Option<u32>,
    /// The agent must call the tool, and see it run, before it answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

impl Guardrail {
    /// Checks it can be enforced.
    ///
    /// # Errors
    ///
    /// What's wrong, in a sentence.
    pub fn check(&self) -> Result<(), String> {
        if self.deny.is_none() && self.max_calls.is_none() && !self.required {
            return Err(format!(
                "the guardrail {} has no rule: give it deny, max_calls or required",
                self.name
            ));
        }
        if let Some(deny) = &self.deny {
            pattern(&deny.matches).map_err(|why| format!("the guardrail {}: {why}", self.name))?;
        }
        Ok(())
    }
}

/// Checks a list of guardrails: each can be enforced, the names are
/// unique, and each holds a tool in `tools`.
///
/// # Errors
///
/// The first problem, in a sentence.
pub fn check_guardrails(guardrails: &[Guardrail], tools: &[String]) -> Result<(), String> {
    if guardrails.len() > MAX_GUARDRAILS {
        return Err(format!("at most {MAX_GUARDRAILS} guardrails"));
    }
    for (index, guardrail) in guardrails.iter().enumerate() {
        guardrail.check()?;
        if !tools.contains(&guardrail.tool) {
            return Err(format!(
                "the guardrail {} holds {}, a tool the agent doesn't have",
                guardrail.name, guardrail.tool
            ));
        }
        if guardrails
            .iter()
            .take(index)
            .any(|earlier| earlier.name == guardrail.name)
        {
            return Err(format!("two guardrails are named {}", guardrail.name));
        }
    }
    Ok(())
}

/// A guardrail, ready to enforce.
#[derive(Debug, Clone)]
pub(crate) struct Enforced {
    pub(crate) rule: Guardrail,
    deny: Option<Regex>,
    /// The agent's calls to the tool that passed their checks.
    pub(crate) calls: u32,
    /// Whether the tool has run for the agent.
    pub(crate) ran: bool,
    /// Whether the agent was reminded of it.
    pub(crate) reminded: bool,
}

impl Enforced {
    /// Ready to enforce, or why it can't be.
    pub(crate) fn new(rule: Guardrail) -> Result<Self, String> {
        rule.check()?;
        let deny = rule
            .deny
            .as_ref()
            .map(|deny| pattern(&deny.matches))
            .transpose()?;
        Ok(Self {
            rule,
            deny,
            calls: 0,
            ran: false,
            reminded: false,
        })
    }

    /// Why this call breaks the rule, if it does, and whether it's the
    /// call limit it broke.
    pub(crate) fn breaks(&self, arguments: &Value) -> Option<(String, bool)> {
        if let Some(max) = self.rule.max_calls
            && self.calls >= max
        {
            return Some((
                format!(
                    "the guardrail {} allows {} at most {max} calls: {}",
                    self.rule.name, self.rule.tool, self.rule.because
                ),
                true,
            ));
        }
        let (deny, regex) = self.rule.deny.as_ref().zip(self.deny.as_ref())?;
        let found = deny
            .argument
            .split('.')
            .try_fold(arguments, |value, key| value.get(key))?;
        let text = found
            .as_str()
            .map_or_else(|| found.to_string(), str::to_owned);
        regex.is_match(&text).then(|| {
            (
                format!(
                    "the guardrail {} refuses {} with that {}: {}",
                    self.rule.name, self.rule.tool, deny.argument, self.rule.because
                ),
                false,
            )
        })
    }

    /// The reminder an agent gets for answering before the tool ran.
    pub(crate) fn reminder(&self) -> String {
        format!(
            "[guardrail {name}] Before you answer, call {tool}. {because}",
            name = self.rule.name,
            tool = self.rule.tool,
            because = self.rule.because
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    fn rail(value: &Value) -> Guardrail {
        serde_json::from_value(value.clone()).unwrap()
    }

    #[test]
    fn a_denied_argument_is_found_by_its_path_and_ignores_case() {
        let rule = Enforced::new(rail(&json!({
            "name": "no_deletes", "tool": "memory", "because": "Notes are kept.",
            "deny": { "argument": "op.kind", "matches": "^delete$" }
        })))
        .unwrap();
        let (why, limit) = rule.breaks(&json!({ "op": { "kind": "DELETE" } })).unwrap();
        assert!(why.contains("no_deletes") && why.contains("Notes are kept."));
        assert!(!limit);
        assert!(
            rule.breaks(&json!({ "op": { "kind": "search" } }))
                .is_none()
        );
        assert!(rule.breaks(&json!({ "query": "delete" })).is_none());
    }

    #[test]
    fn a_value_that_isnt_a_string_is_matched_as_json() {
        let rule = Enforced::new(rail(&json!({
            "name": "small", "tool": "memory", "because": "Keep it small.",
            "deny": { "argument": "limit", "matches": "^[0-9]{3,}$" }
        })))
        .unwrap();
        assert!(rule.breaks(&json!({ "limit": 500 })).is_some());
        assert!(rule.breaks(&json!({ "limit": 5 })).is_none());
    }

    #[test]
    fn calls_past_the_limit_are_refused() {
        let mut rule = Enforced::new(rail(&json!({
            "name": "few", "tool": "http_request", "because": "Fetching is slow.",
            "max_calls": 2
        })))
        .unwrap();
        rule.calls = 1;
        assert!(rule.breaks(&json!({})).is_none());
        rule.calls = 2;
        assert!(rule.breaks(&json!({})).unwrap().1);
    }

    #[test]
    fn a_list_is_checked_whole() {
        let tools = vec!["memory".to_owned()];
        let required = json!({ "name": "a", "tool": "memory", "because": "b", "required": true });
        assert_eq!(check_guardrails(&[rail(&required)], &tools), Ok(()));
        let none = rail(&json!({ "name": "a", "tool": "memory", "because": "b" }));
        assert!(
            check_guardrails(&[none], &tools)
                .unwrap_err()
                .contains("no rule")
        );
        let other = rail(&json!({ "name": "a", "tool": "nope", "because": "b", "required": true }));
        assert!(
            check_guardrails(&[other], &tools)
                .unwrap_err()
                .contains("doesn't have")
        );
        let twice = [rail(&required), rail(&required)];
        assert!(
            check_guardrails(&twice, &tools)
                .unwrap_err()
                .contains("two")
        );
        let bad = rail(&json!({
            "name": "a", "tool": "memory", "because": "b",
            "deny": { "argument": "x", "matches": "(" }
        }));
        assert!(
            check_guardrails(&[bad], &tools)
                .unwrap_err()
                .contains("pattern")
        );
    }
}
