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

/// What one of a call's arguments says, as a guardrail's `deny` refuses it
/// or its `action` picks the calls it's about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArgumentMatch {
    /// The argument, as a dotted path such as `operation` or `filter.kind`.
    #[schemars(length(min = 1, max = 128))]
    pub argument: String,
    /// A pattern that ignores case. A string is matched as it is, anything
    /// else as JSON. A call without the argument doesn't match.
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
    /// The calls of the tool its rules are about. Left out, all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ArgumentMatch>,
    /// Refuses a call whose argument matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<ArgumentMatch>,
    /// Refuses the agent's calls past this many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1000))]
    pub max_calls: Option<u32>,
    /// The agent must call the tool, and see it run, before it answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// The same as `required`, but only for an answer this pattern matches,
    /// ignoring case: what the answer says the tool did, such as
    /// `\b(saved|stored)\b`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub claimed: Option<String>,
}

impl Guardrail {
    /// Checks it can be enforced.
    ///
    /// # Errors
    ///
    /// What's wrong, in a sentence.
    pub fn check(&self) -> Result<(), String> {
        let named = |why: String| format!("the guardrail {}: {why}", self.name);
        let rules = [
            self.deny.is_some(),
            self.max_calls.is_some(),
            self.required,
            self.claimed.is_some(),
        ];
        if !rules.contains(&true) {
            return Err(named(
                "it has no rule: give it deny, max_calls, required or claimed".to_owned(),
            ));
        }
        if self.required && self.claimed.is_some() {
            return Err(named(
                "required and claimed both say when the tool must have run; give one".to_owned(),
            ));
        }
        let patterns = [
            self.deny.as_ref().map(|deny| &deny.matches),
            self.action.as_ref().map(|action| &action.matches),
            self.claimed.as_ref(),
        ];
        for text in patterns.into_iter().flatten() {
            pattern(text).map_err(named)?;
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

/// The text of the argument at a dotted `path`: a string as it is,
/// anything else as JSON. `None` if there's no such argument.
fn argument_text(arguments: &Value, path: &str) -> Option<String> {
    let found = path
        .split('.')
        .try_fold(arguments, |value, key| value.get(key))?;
    Some(
        found
            .as_str()
            .map_or_else(|| found.to_string(), str::to_owned),
    )
}

/// A guardrail, ready to enforce.
#[derive(Debug, Clone)]
pub(crate) struct Enforced {
    pub(crate) rule: Guardrail,
    deny: Option<Regex>,
    action: Option<Regex>,
    claimed: Option<Regex>,
    /// The agent's calls it's about that passed their checks.
    pub(crate) calls: u32,
    /// Whether such a call has run for the agent.
    pub(crate) ran: bool,
    /// Whether the agent was reminded of it.
    pub(crate) reminded: bool,
}

impl Enforced {
    /// Ready to enforce, or why it can't be.
    pub(crate) fn new(rule: Guardrail) -> Result<Self, String> {
        rule.check()?;
        let compiled = |text: Option<&String>| text.map(|text| pattern(text)).transpose();
        Ok(Self {
            deny: compiled(rule.deny.as_ref().map(|deny| &deny.matches))?,
            action: compiled(rule.action.as_ref().map(|action| &action.matches))?,
            claimed: compiled(rule.claimed.as_ref())?,
            rule,
            calls: 0,
            ran: false,
            reminded: false,
        })
    }

    /// Whether a call to its tool with these arguments is one it's about.
    pub(crate) fn about(&self, arguments: &Value) -> bool {
        let Some((action, regex)) = self.rule.action.as_ref().zip(self.action.as_ref()) else {
            return true;
        };
        argument_text(arguments, &action.argument).is_some_and(|text| regex.is_match(&text))
    }

    /// Whether `answer` needs a call it's about to have run first.
    pub(crate) fn needs_call(&self, answer: &str) -> bool {
        self.rule.required
            || self
                .claimed
                .as_ref()
                .is_some_and(|regex| regex.is_match(answer))
    }

    /// Why this call breaks the rule, if it does, and whether it's the
    /// call limit it broke.
    pub(crate) fn breaks(&self, arguments: &Value) -> Option<(String, bool)> {
        if !self.about(arguments) {
            return None;
        }
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
        let text = argument_text(arguments, &deny.argument)?;
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

    /// The reminder an agent gets for an answer that needed the call first.
    pub(crate) fn reminder(&self) -> String {
        let call = match &self.rule.action {
            Some(action) => format!(
                "{} with {} matching {:?}",
                self.rule.tool, action.argument, action.matches
            ),
            None => self.rule.tool.clone(),
        };
        let (name, because) = (&self.rule.name, &self.rule.because);
        if self.rule.claimed.is_some() {
            format!(
                "[guardrail {name}] Your answer says what {call} would have done, and it \
                 hasn't run. Call it, or answer without saying so. {because}"
            )
        } else {
            format!("[guardrail {name}] Before you answer, call {call}. {because}")
        }
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

    fn refusal(rules: &[Guardrail], tools: &[String]) -> String {
        check_guardrails(rules, tools).unwrap_err()
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
    fn an_action_narrows_the_calls_a_rule_is_about() {
        let mut rule = Enforced::new(rail(&json!({
            "name": "one_store", "tool": "memory", "because": "Once.", "max_calls": 1,
            "action": { "argument": "operation", "matches": "^store$" }
        })))
        .unwrap();
        assert!(rule.about(&json!({ "operation": "STORE" })));
        assert!(!rule.about(&json!({ "operation": "get" })));
        assert!(!rule.about(&json!({})));
        rule.calls = 1;
        assert!(rule.breaks(&json!({ "operation": "store" })).is_some());
        assert!(rule.breaks(&json!({ "operation": "search" })).is_none());
    }

    #[test]
    fn a_claim_needs_the_call_only_when_the_answer_makes_it() {
        let rule = Enforced::new(rail(&json!({
            "name": "saved", "tool": "memory", "because": "Say so only if it ran.",
            "claimed": r"\b(saved|stored)\b"
        })))
        .unwrap();
        assert!(rule.needs_call("I SAVED it."));
        assert!(!rule.needs_call("Here is what I found."));
        assert!(rule.reminder().contains("answer without saying so"));
    }

    #[test]
    fn a_list_is_checked_whole() {
        let tools = vec!["memory".to_owned()];
        let required = json!({ "name": "a", "tool": "memory", "because": "b", "required": true });
        assert_eq!(check_guardrails(&[rail(&required)], &tools), Ok(()));
        let none = rail(&json!({ "name": "a", "tool": "memory", "because": "b" }));
        assert!(refusal(&[none], &tools).contains("no rule"));
        let other = rail(&json!({ "name": "a", "tool": "nope", "because": "b", "required": true }));
        assert!(refusal(&[other], &tools).contains("doesn't have"));
        let twice = [rail(&required), rail(&required)];
        assert!(refusal(&twice, &tools).contains("two"));
        let bad = rail(&json!({
            "name": "a", "tool": "memory", "because": "b",
            "deny": { "argument": "x", "matches": "(" }
        }));
        assert!(refusal(&[bad], &tools).contains("pattern"));
        let both = rail(&json!({
            "name": "a", "tool": "memory", "because": "b", "required": true, "claimed": "x"
        }));
        assert!(refusal(&[both], &tools).contains("give one"));
        let bare_action = rail(&json!({
            "name": "a", "tool": "memory", "because": "b",
            "action": { "argument": "operation", "matches": "store" }
        }));
        assert!(refusal(&[bare_action], &tools).contains("no rule"));
    }
}
