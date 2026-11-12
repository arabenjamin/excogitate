//! Whether a changed schema breaks what was built against the old one.
//!
//! The rule depends on who reads the schema. An [`Role::Input`] is what
//! the runtime accepts, such as a config file or a request: everything the
//! old schema accepted must still be accepted. An [`Role::Output`] is what
//! it produces, such as a receipt or a response: everything it may now
//! produce must still pass the old schema, except a property added where
//! the old one was closed, which a reader ignores.
//!
//! The check is conservative. A change it can't show is safe, such as a
//! rewritten condition, is reported as a break.

use std::collections::BTreeSet;
use std::fmt;

use serde_json::{Map, Value};

use crate::strict::EXTENSION_KEY;

/// Who reads a schema, which decides what a break is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The runtime accepts it: the new schema must accept all the old one did.
    Input,
    /// The runtime produces it: the old schema must accept all the new one
    /// allows, apart from added properties.
    Output,
}

/// One change that breaks a reader or writer of the old schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Break {
    /// Where, as a dotted path of properties; `[]` is an array's items and
    /// `|n` a schema's n-th choice.
    pub path: String,
    /// What changed, from old to new.
    pub what: String,
}

impl fmt::Display for Break {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() || self.path.starts_with('|') {
            write!(f, "(top){}: {}", self.path, self.what)
        } else {
            write!(f, "{}: {}", self.path, self.what)
        }
    }
}

/// Every change from `old` to `new` that breaks a reader or writer of
/// `old`, for a schema in `role`. Empty means the change is compatible.
#[must_use]
pub fn breaking_changes(old: &Value, new: &Value, role: Role) -> Vec<Break> {
    let (wider, narrower) = match role {
        Role::Input => (new, old),
        Role::Output => (old, new),
    };
    let mut walk = Walk {
        wider_root: wider,
        narrower_root: narrower,
        role,
        seen: BTreeSet::new(),
        found: Vec::new(),
    };
    walk.covers(wider, narrower, "");
    walk.found
}

/// Keys that only describe, and never change what a schema accepts.
const NOTES: [&str; 6] = [
    "description",
    "title",
    "default",
    "examples",
    "$schema",
    "$comment",
];

/// Bounds a value must stay under, and over.
const UPPER: [&str; 5] = [
    "maxLength",
    "maxItems",
    "maxProperties",
    "maximum",
    "exclusiveMaximum",
];
const LOWER: [&str; 5] = [
    "minLength",
    "minItems",
    "minProperties",
    "minimum",
    "exclusiveMinimum",
];

/// Keys a conditional is made of; any change to one is a break.
const CONDITIONS: [&str; 4] = ["if", "then", "else", "not"];

/// One comparison: does everything `narrower` allows also pass `wider`?
struct Walk<'a> {
    wider_root: &'a Value,
    narrower_root: &'a Value,
    role: Role,
    /// Pairs of definitions already being compared, so a recursive type
    /// ends.
    seen: BTreeSet<(String, String)>,
    found: Vec<Break>,
}

impl Walk<'_> {
    fn report(&mut self, path: &str, what: String) {
        self.found.push(Break {
            path: path.to_owned(),
            what,
        });
    }

    /// The old and new value of something, from the wider and narrower one.
    fn old_new<T>(&self, wider: T, narrower: T) -> (T, T) {
        match self.role {
            Role::Input => (narrower, wider),
            Role::Output => (wider, narrower),
        }
    }

    fn covers(&mut self, wider: &Value, narrower: &Value, path: &str) {
        let (wider, wider_ref) = resolve(wider, self.wider_root);
        let (narrower, narrower_ref) = resolve(narrower, self.narrower_root);
        if let (Some(w), Some(n)) = (wider_ref, narrower_ref)
            && !self.seen.insert((w.to_owned(), n.to_owned()))
        {
            return;
        }
        match (wider, narrower) {
            (_, Value::Bool(false)) | (Value::Bool(true), _) => return,
            (Value::Bool(false), _) => {
                self.report(path, "now refuses everything".to_owned());
                return;
            }
            _ => {}
        }
        let (Some(wider), Some(narrower)) = (wider.as_object(), narrower.as_object()) else {
            return;
        };
        let wider = flatten_all_of(wider);
        let narrower = flatten_all_of(narrower);
        if wider.contains_key(EXTENSION_KEY) || is_open(&wider) {
            return;
        }
        if self.choices(&wider, &narrower, path) {
            return;
        }
        self.conditions(&wider, &narrower, path);
        self.types(&wider, &narrower, path);
        self.values(&wider, &narrower, path);
        self.exact(&wider, &narrower, path);
        self.bounds(&wider, &narrower, path);
        self.objects(&wider, &narrower, path);
        self.items(&wider, &narrower, path);
    }

    /// `oneOf` and `anyOf`. Every choice the narrower one allows must fit
    /// the wider one, and a narrower one without choices must fit one of
    /// the wider one's. True when choices were compared, so nothing else is.
    fn choices(
        &mut self,
        wider: &Map<String, Value>,
        narrower: &Map<String, Value>,
        path: &str,
    ) -> bool {
        if let Some(options) = choices_of(narrower) {
            let whole = Value::Object(wider.clone());
            for (index, option) in options.iter().enumerate() {
                let option = Value::Object(merged(narrower, option));
                let here = format!("{path}|{index}");
                self.covers(&whole, &option, &here);
            }
            return true;
        }
        let Some(options) = choices_of(wider) else {
            return false;
        };
        let narrower = Value::Object(narrower.clone());
        // The closest choice: when one is off by a single change, that
        // change is the clearest thing to report.
        let mut closest: Option<Vec<Break>> = None;
        for option in options {
            let option = Value::Object(merged(wider, option));
            let found = self.trial(&option, &narrower, path);
            if found.is_empty() {
                return true;
            }
            if closest.as_ref().is_none_or(|best| found.len() < best.len()) {
                closest = Some(found);
            }
        }
        match closest {
            Some(mut only) if only.len() == 1 => self.found.append(&mut only),
            _ => {
                let what = match self.role {
                    Role::Input => "an old value matches none of the new choices",
                    Role::Output => "a new value matches none of the old choices",
                };
                self.report(path, what.to_owned());
            }
        }
        true
    }

    /// What would break if `narrower` had to fit `wider`, without reporting.
    fn trial(&self, wider: &Value, narrower: &Value, path: &str) -> Vec<Break> {
        let mut inner = Walk {
            wider_root: self.wider_root,
            narrower_root: self.narrower_root,
            role: self.role,
            seen: self.seen.clone(),
            found: Vec::new(),
        };
        inner.covers(wider, narrower, path);
        inner.found
    }

    fn conditions(
        &mut self,
        wider: &Map<String, Value>,
        narrower: &Map<String, Value>,
        path: &str,
    ) {
        for key in CONDITIONS {
            if let Some(condition) = wider.get(key)
                && narrower.get(key) != Some(condition)
            {
                self.report(path, format!("`{key}` changed"));
            }
        }
    }

    fn types(&mut self, wider: &Map<String, Value>, narrower: &Map<String, Value>, path: &str) {
        let Some(allowed) = types_of(wider) else {
            return;
        };
        let fits = types_of(narrower).is_some_and(|types| {
            types
                .iter()
                .all(|t| allowed.contains(t) || (t == "integer" && allowed.contains("number")))
        });
        if !fits {
            let (old, new) = self.old_new(type_text(wider), type_text(narrower));
            self.report(path, format!("type {old} became {new}"));
        }
    }

    fn values(&mut self, wider: &Map<String, Value>, narrower: &Map<String, Value>, path: &str) {
        let Some(allowed) = values_of(wider) else {
            return;
        };
        let Some(values) = values_of(narrower) else {
            let what = match self.role {
                Role::Input => "now limited to a set of values",
                Role::Output => "no longer limited to a set of values",
            };
            self.report(path, what.to_owned());
            return;
        };
        for value in values {
            if !allowed.contains(&value) {
                let what = match self.role {
                    Role::Input => format!("value {value} removed"),
                    Role::Output => format!("value {value} added"),
                };
                self.report(path, what);
            }
        }
    }

    /// `format` and `pattern`, which must stay as they were.
    fn exact(&mut self, wider: &Map<String, Value>, narrower: &Map<String, Value>, path: &str) {
        for key in ["format", "pattern"] {
            if let Some(rule) = wider.get(key)
                && narrower.get(key) != Some(rule)
            {
                let (old, new) = self.old_new(Some(rule), narrower.get(key));
                self.report(path, format!("{key} {} became {}", shown(old), shown(new)));
            }
        }
    }

    fn bounds(&mut self, wider: &Map<String, Value>, narrower: &Map<String, Value>, path: &str) {
        for (keys, upper) in [(UPPER, true), (LOWER, false)] {
            for key in keys {
                let Some(limit) = wider.get(key).and_then(Value::as_f64) else {
                    continue;
                };
                let within = narrower
                    .get(key)
                    .and_then(Value::as_f64)
                    .is_some_and(|bound| {
                        if upper {
                            bound <= limit
                        } else {
                            bound >= limit
                        }
                    });
                if !within {
                    let (old, new) = self.old_new(wider.get(key), narrower.get(key));
                    self.report(path, format!("{key} {} became {}", shown(old), shown(new)));
                }
            }
        }
    }

    fn objects(&mut self, wider: &Map<String, Value>, narrower: &Map<String, Value>, path: &str) {
        let empty = Map::new();
        let wider_properties = properties_of(wider).unwrap_or(&empty);
        let narrower_properties = properties_of(narrower).unwrap_or(&empty);
        let wider_extra = wider.get("additionalProperties");
        let wider_closed = wider_extra == Some(&Value::Bool(false));
        for (name, schema) in narrower_properties {
            let here = join(path, name);
            if let Some(allowed) = wider_properties.get(name) {
                self.covers(allowed, schema, &here);
            } else if wider_closed {
                if self.role == Role::Input {
                    self.report(&here, "property removed".to_owned());
                }
            } else if let Some(extra) = wider_extra {
                self.covers(extra, schema, &here);
            }
        }
        let narrower_required = required_of(narrower);
        for name in required_of(wider) {
            if !narrower_required.contains(&name) {
                let what = match self.role {
                    Role::Input => "now required",
                    Role::Output => "no longer always present",
                };
                self.report(&join(path, &name), what.to_owned());
            }
        }
        let narrower_extra = narrower.get("additionalProperties");
        match (wider_extra, narrower_extra) {
            (Some(Value::Bool(false)), Some(Value::Bool(false))) => {}
            (Some(Value::Bool(false)), _) => {
                if self.role == Role::Input {
                    self.report(path, "unlisted properties now refused".to_owned());
                }
            }
            (Some(extra @ Value::Object(_)), Some(other @ Value::Object(_))) => {
                self.covers(extra, other, &format!("{path}{{}}"));
            }
            _ => {}
        }
    }

    fn items(&mut self, wider: &Map<String, Value>, narrower: &Map<String, Value>, path: &str) {
        let Some(allowed) = wider.get("items") else {
            return;
        };
        let here = format!("{path}[]");
        match narrower.get("items") {
            Some(items) => self.covers(allowed, items, &here),
            None => self.report(&here, "items were any value, now limited".to_owned()),
        }
    }
}

/// A `$ref` followed into its root's `$defs`, with the reference's name.
fn resolve<'v>(schema: &'v Value, root: &'v Value) -> (&'v Value, Option<&'v str>) {
    let mut current = schema;
    let mut name = None;
    // A chain of references ends, or it's cut off here.
    for _ in 0..16 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(target) = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
        else {
            break;
        };
        name = Some(reference);
        current = target;
    }
    (current, name)
}

/// A schema with a single `allOf` folded into it.
fn flatten_all_of(schema: &Map<String, Value>) -> Map<String, Value> {
    let mut flat = schema.clone();
    if let Some(Value::Array(parts)) = flat.remove("allOf") {
        if let [Value::Object(only)] = parts.as_slice() {
            for (key, value) in only {
                flat.entry(key.clone()).or_insert_with(|| value.clone());
            }
        } else {
            flat.insert("allOf".to_owned(), Value::Array(parts));
        }
    }
    flat
}

/// A schema with no constraint at all.
fn is_open(schema: &Map<String, Value>) -> bool {
    schema.keys().all(|key| NOTES.contains(&key.as_str()))
}

fn choices_of(schema: &Map<String, Value>) -> Option<&Vec<Value>> {
    schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
}

/// One choice with its parent's other keys, which apply to it too.
fn merged(parent: &Map<String, Value>, option: &Value) -> Map<String, Value> {
    let mut whole: Map<String, Value> = parent
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "oneOf" | "anyOf"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(option) = option.as_object() {
        for (key, value) in option {
            whole.insert(key.clone(), value.clone());
        }
    } else {
        whole.insert("allOf".to_owned(), Value::Array(vec![option.clone()]));
    }
    whole
}

fn types_of(schema: &Map<String, Value>) -> Option<BTreeSet<String>> {
    match schema.get("type")? {
        Value::String(one) => Some(BTreeSet::from([one.clone()])),
        Value::Array(many) => Some(
            many.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        _ => None,
    }
}

fn type_text(schema: &Map<String, Value>) -> String {
    types_of(schema).map_or_else(
        || "any".to_owned(),
        |types| types.into_iter().collect::<Vec<_>>().join("|"),
    )
}

fn values_of(schema: &Map<String, Value>) -> Option<Vec<Value>> {
    if let Some(one) = schema.get("const") {
        return Some(vec![one.clone()]);
    }
    schema.get("enum").and_then(Value::as_array).cloned()
}

fn properties_of(schema: &Map<String, Value>) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

fn required_of(schema: &Map<String, Value>) -> BTreeSet<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn shown(value: Option<&Value>) -> String {
    value.map_or_else(|| "none".to_owned(), ToString::to_string)
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

#[cfg(test)]
mod tests;
