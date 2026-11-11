//! `{{inputs.url}}` and `{{steps.fetch.result.body}}`. Substitution is one
//! pass over the definition, never over what was substituted, so text a page
//! returned can't reach for anything else.

use serde_json::Value;

const OPEN: &str = "{{";
const CLOSE: &str = "}}";

/// A template can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TemplateError {
    /// It isn't well formed.
    #[error("malformed template: {0}")]
    Malformed(String),
    /// It refers to something that isn't there.
    #[error("{0} is missing")]
    Missing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part<'a> {
    Text(&'a str),
    Reference(&'a str),
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

fn parse(text: &str) -> Result<Vec<Part<'_>>, TemplateError> {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some((before, after)) = rest.split_once(OPEN) {
        if !before.is_empty() {
            parts.push(Part::Text(before));
        }
        let Some((inner, tail)) = after.split_once(CLOSE) else {
            return Err(TemplateError::Malformed("an unclosed {{".to_owned()));
        };
        let path = inner.trim();
        if !valid_path(path) {
            return Err(TemplateError::Malformed(format!("bad reference {path:?}")));
        }
        parts.push(Part::Reference(path));
        rest = tail;
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest));
    }
    Ok(parts)
}

/// Every reference in `text`.
///
/// # Errors
///
/// [`TemplateError::Malformed`] if it isn't well formed.
pub(crate) fn references(text: &str) -> Result<Vec<&str>, TemplateError> {
    Ok(parse(text)?
        .into_iter()
        .filter_map(|part| match part {
            Part::Reference(path) => Some(path),
            Part::Text(_) => None,
        })
        .collect())
}

fn lookup<'a>(context: &'a Value, path: &str) -> Result<&'a Value, TemplateError> {
    let mut current = context;
    for segment in path.split('.') {
        current = current
            .get(segment)
            .ok_or_else(|| TemplateError::Missing(path.to_owned()))?;
    }
    Ok(current)
}

fn as_text(value: &Value, path: &str) -> Result<String, TemplateError> {
    match value {
        Value::Null => Err(TemplateError::Missing(path.to_owned())),
        Value::String(text) => Ok(text.clone()),
        other => Ok(other.to_string()),
    }
}

/// `text` with its references filled in, as text.
///
/// # Errors
///
/// [`TemplateError`] if it's malformed or something it refers to is missing.
pub fn render_text(text: &str, context: &Value) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(text.len());
    for part in parse(text)? {
        match part {
            Part::Text(literal) => out.push_str(literal),
            Part::Reference(path) => out.push_str(&as_text(lookup(context, path)?, path)?),
        }
    }
    Ok(out)
}

/// `value` with every string's references filled in. A string that is only
/// one reference takes that value's type.
///
/// # Errors
///
/// [`TemplateError`] if a template is malformed or refers to what's missing.
pub fn render_value(value: &Value, context: &Value) -> Result<Value, TemplateError> {
    match value {
        Value::String(text) => match parse(text)?.as_slice() {
            [Part::Reference(path)] => lookup(context, path).cloned(),
            _ => render_text(text, context).map(Value::String),
        },
        Value::Array(items) => items
            .iter()
            .map(|item| render_value(item, context))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, field)| Ok((key.clone(), render_value(field, context)?)))
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(Value::Object),
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn context() -> Value {
        json!({
            "inputs": { "url": "http://a/", "n": 3, "gone": null },
            "steps": { "fetch": { "result": { "body": "{{inputs.url}} and more" } } }
        })
    }

    #[test]
    fn text_is_filled_in() {
        assert_eq!(
            render_text("get {{ inputs.url }} x{{inputs.n}}", &context()).unwrap(),
            "get http://a/ x3"
        );
    }

    #[test]
    fn a_lone_reference_keeps_its_type() {
        let rendered = render_value(
            &json!({ "a": "{{inputs.n}}", "b": ["x{{inputs.n}}"] }),
            &context(),
        );
        assert_eq!(rendered.unwrap(), json!({ "a": 3, "b": ["x3"] }));
    }

    #[test]
    fn what_a_step_returned_is_not_expanded_again() {
        let rendered = render_text("{{steps.fetch.result.body}}", &context()).unwrap();
        assert_eq!(rendered, "{{inputs.url}} and more");
    }

    #[test]
    fn a_missing_value_is_an_error_not_an_empty_string() {
        for text in [
            "{{inputs.nope}}",
            "x {{steps.other.answer}}",
            "x {{inputs.gone}}",
        ] {
            let error = render_text(text, &context()).unwrap_err();
            assert!(
                matches!(error, TemplateError::Missing(_)),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn malformed_templates_are_refused() {
        for text in ["{{inputs.url", "{{}}", "{{a b}}", "{{a..b}}"] {
            let error = render_text(text, &context()).unwrap_err();
            assert!(
                matches!(error, TemplateError::Malformed(_)),
                "{text}: {error}"
            );
        }
    }

    proptest! {
        #[test]
        fn any_text_renders_or_is_refused_without_panicking(text in ".{0,200}") {
            let _ = render_text(&text, &context());
            let _ = references(&text);
        }

        #[test]
        fn text_with_no_braces_comes_back_unchanged(text in "[^{}]{0,200}") {
            prop_assert_eq!(render_text(&text, &context()).unwrap(), text);
        }
    }
}
