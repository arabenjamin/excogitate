//! The route table, and the `OpenAPI` 3.1 document built from it. Each route
//! carries its handler and how the document describes it, side by side, so
//! the router can't hold a route the document leaves out.

use std::collections::BTreeMap;

use axum::routing::MethodRouter;
use serde_json::{Map, Value, json};

use super::App;

/// The `OpenAPI` version written.
const OPENAPI: &str = "3.1.0";
/// Where the component schemas live in the document.
const COMPONENTS: &str = "#/components/schemas/";
/// Where a schema's own parts live in it.
const DEFS: &str = "#/$defs/";

/// A route's method, as the document names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    const fn name(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
            Self::Put => "put",
            Self::Delete => "delete",
        }
    }
}

/// A query parameter.
#[derive(Debug, Clone, Copy)]
pub(super) struct Param {
    pub(super) name: &'static str,
    pub(super) about: &'static str,
    /// The values it takes, if it's one of a few.
    pub(super) values: &'static [&'static str],
    pub(super) number: bool,
}

/// The page size, which every list takes.
pub(super) const LIMIT: Param = Param {
    name: "limit",
    about: "How many to return, 1 to 200. 50 if left out.",
    values: &[],
    number: true,
};
/// Where the page starts, which every list takes.
pub(super) const OFFSET: Param = Param {
    name: "offset",
    about: "How many to skip first.",
    values: &[],
    number: true,
};

/// A body: none, one with a schema of its own, or one the document only
/// describes.
#[derive(Debug, Clone, Copy)]
pub(super) enum Body {
    Empty,
    Typed(fn() -> Value),
    Described(&'static str),
}

/// One reply a route can make.
#[derive(Debug, Clone, Copy)]
pub(super) struct Reply {
    status: u16,
    about: &'static str,
    body: Body,
}

/// Who may reach a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    /// Anyone: `/health`.
    Open,
    /// Only with a key.
    Keyed,
    /// `/metrics`: keyed unless `server.open_metrics` is set.
    Metrics,
}

/// One route: its handler, and what the document says about it.
pub(super) struct Route {
    pub(super) id: &'static str,
    pub(super) method: Method,
    pub(super) path: &'static str,
    summary: &'static str,
    query: &'static [Param],
    request: Body,
    replies: Vec<Reply>,
    pub(super) access: Access,
    pub(super) handler: MethodRouter<App>,
}

impl Route {
    fn new(
        method: Method,
        id: &'static str,
        path: &'static str,
        summary: &'static str,
        handler: MethodRouter<App>,
    ) -> Self {
        Self {
            id,
            method,
            path,
            summary,
            query: &[],
            request: Body::Empty,
            replies: Vec::new(),
            access: Access::Keyed,
            handler,
        }
    }

    pub(super) fn get(
        id: &'static str,
        path: &'static str,
        summary: &'static str,
        handler: MethodRouter<App>,
    ) -> Self {
        Self::new(Method::Get, id, path, summary, handler)
    }

    pub(super) fn post(
        id: &'static str,
        path: &'static str,
        summary: &'static str,
        handler: MethodRouter<App>,
    ) -> Self {
        Self::new(Method::Post, id, path, summary, handler)
    }

    pub(super) fn put(
        id: &'static str,
        path: &'static str,
        summary: &'static str,
        handler: MethodRouter<App>,
    ) -> Self {
        Self::new(Method::Put, id, path, summary, handler)
    }

    pub(super) fn delete(
        id: &'static str,
        path: &'static str,
        summary: &'static str,
        handler: MethodRouter<App>,
    ) -> Self {
        Self::new(Method::Delete, id, path, summary, handler)
    }

    pub(super) const fn query(mut self, query: &'static [Param]) -> Self {
        self.query = query;
        self
    }

    pub(super) const fn body(mut self, body: Body) -> Self {
        self.request = body;
        self
    }

    pub(super) fn reply(mut self, status: u16, about: &'static str, body: Body) -> Self {
        self.replies.push(Reply {
            status,
            about,
            body,
        });
        self
    }

    pub(super) const fn access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }

    /// The names in braces in its path.
    fn path_params(&self) -> impl Iterator<Item = &'static str> {
        self.path.split('/').filter_map(|part| {
            part.strip_prefix('{')
                .and_then(|part| part.strip_suffix('}'))
        })
    }
}

/// A document's title, version and server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct About<'a> {
    pub(crate) title: &'a str,
    pub(crate) version: &'a str,
    pub(crate) server: &'a str,
    /// The license, as an SPDX expression, if it has one.
    pub(crate) license: Option<&'a str>,
}

/// The document for `routes`, and for each of `tools` (a `tools/list`
/// listing) at a path of its own in place of the generic tool route.
///
/// # Errors
///
/// Why, if two different schemas would take the same name.
pub(super) fn document(
    routes: &[Route],
    tools: Option<&Value>,
    about: About<'_>,
) -> Result<Value, String> {
    let mut components = Components::default();
    let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let error = components.add("ApiError", schema::schema_of::<super::ApiError>())?;
    for route in routes {
        if tools.is_some() && route.id == super::tools::CALL_TOOL {
            continue;
        }
        let operation = components.operation(route, &error)?;
        paths
            .entry(route.path.to_owned())
            .or_default()
            .insert(route.method.name().to_owned(), Value::Object(operation));
    }
    if let Some(tools) = tools {
        for tool in tools
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (path, operation) = components.tool(tool, &error)?;
            paths
                .entry(path)
                .or_default()
                .insert("post".to_owned(), Value::Object(operation));
        }
    }
    let mut info = json!({ "title": about.title, "version": about.version });
    if let (Some(license), Some(object)) = (about.license, info.as_object_mut()) {
        object.insert(
            "license".to_owned(),
            json!({ "name": license, "identifier": license }),
        );
    }
    Ok(json!({
        "openapi": OPENAPI,
        "info": info,
        "servers": [{ "url": about.server }],
        "security": [{ "bearer": [] }],
        "paths": paths,
        "components": {
            "schemas": components.schemas,
            "securitySchemes": {
                "bearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "The owner key.",
                },
            },
        },
    }))
}

/// The component schemas gathered so far.
#[derive(Debug, Default)]
struct Components {
    schemas: BTreeMap<String, Value>,
}

impl Components {
    /// Adds `schema` as `name`, its own parts as components beside it, and
    /// returns a reference to it.
    fn add(&mut self, name: &str, mut schema: Value) -> Result<Value, String> {
        let defs = schema
            .as_object_mut()
            .and_then(|object| {
                object.remove("$schema");
                // A nested copy of the same type has no title, and the
                // component's name says it.
                object.remove("title");
                object.remove("$defs")
            })
            .and_then(|defs| match defs {
                Value::Object(defs) => Some(defs),
                _ => None,
            })
            .unwrap_or_default();
        let names = self.names_for(name, &defs);
        for (def, mut part) in defs {
            rewrite(&mut part, &names);
            let called = names.get(&def).cloned().unwrap_or(def);
            self.put(&called, part)?;
        }
        rewrite(&mut schema, &names);
        self.put(name, schema)?;
        Ok(json!({ "$ref": format!("{COMPONENTS}{name}") }))
    }

    /// What each of a schema's parts is called in the document: its own
    /// name, unless a different schema has it. Then the name of a component
    /// that's the same schema, if there is one, or its name prefixed with
    /// `owner`.
    fn names_for(&self, owner: &str, defs: &Map<String, Value>) -> BTreeMap<String, String> {
        let mut names: BTreeMap<String, String> =
            defs.keys().map(|def| (def.clone(), def.clone())).collect();
        // A part's name depends on its parts' names, so it's worked out
        // again until none moves.
        for _ in 0..=defs.len() {
            let mut changed = false;
            for (def, part) in defs {
                let mut part = part.clone();
                rewrite(&mut part, &names);
                let preferred = if self
                    .schemas
                    .get(def)
                    .is_none_or(|existing| *existing == part)
                {
                    def.clone()
                } else {
                    self.schemas
                        .iter()
                        .find(|(_, existing)| **existing == part)
                        .map_or_else(|| format!("{owner}{def}"), |(name, _)| name.clone())
                };
                if names.get(def) != Some(&preferred) {
                    names.insert(def.clone(), preferred);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        names
    }

    fn put(&mut self, name: &str, schema: Value) -> Result<(), String> {
        match self.schemas.get(name) {
            Some(existing) if *existing != schema => {
                Err(format!("two different schemas are both named {name}"))
            }
            Some(_) => Ok(()),
            None => {
                self.schemas.insert(name.to_owned(), schema);
                Ok(())
            }
        }
    }

    /// A body's content, adding its schema if it has one.
    fn content(&mut self, body: Body, fallback: &str) -> Result<Option<Value>, String> {
        Ok(match body {
            Body::Empty => None,
            Body::Typed(schema_of) => {
                let schema = schema_of();
                let name = schema
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(fallback)
                    .to_owned();
                Some(json!({ "application/json": { "schema": self.add(&name, schema)? } }))
            }
            Body::Described(about) => Some(json!({
                "application/json": { "schema": { "type": "object", "description": about } }
            })),
        })
    }

    fn operation(&mut self, route: &Route, error: &Value) -> Result<Map<String, Value>, String> {
        let mut parameters: Vec<Value> = route
            .path_params()
            .map(|name| {
                json!({ "name": name, "in": "path", "required": true,
                        "schema": { "type": "string" } })
            })
            .collect();
        for param in route.query {
            let mut schema = if param.number {
                json!({ "type": "integer", "minimum": 0 })
            } else {
                json!({ "type": "string" })
            };
            if !param.values.is_empty()
                && let Some(object) = schema.as_object_mut()
            {
                object.insert("enum".to_owned(), json!(param.values));
            }
            parameters.push(
                json!({ "name": param.name, "in": "query", "required": false,
                                    "description": param.about, "schema": schema }),
            );
        }
        let mut operation = Map::new();
        operation.insert("operationId".to_owned(), json!(route.id));
        operation.insert("summary".to_owned(), json!(route.summary));
        if !parameters.is_empty() {
            operation.insert("parameters".to_owned(), json!(parameters));
        }
        if let Some(content) = self.content(route.request, &format!("{}_request", route.id))? {
            operation.insert(
                "requestBody".to_owned(),
                json!({ "required": true, "content": content }),
            );
        }
        let mut responses = Map::new();
        for reply in &route.replies {
            let mut response = json!({ "description": reply.about });
            if let Some(content) = self.content(reply.body, &format!("{}_reply", route.id))?
                && let Some(object) = response.as_object_mut()
            {
                object.insert("content".to_owned(), content);
            }
            responses.insert(reply.status.to_string(), response);
        }
        errors(&mut responses, error);
        operation.insert("responses".to_owned(), Value::Object(responses));
        match route.access {
            Access::Open => {
                operation.insert("security".to_owned(), json!([]));
            }
            Access::Metrics => {
                operation.insert(
                    "description".to_owned(),
                    json!("Needs the key unless server.open_metrics is set."),
                );
            }
            Access::Keyed => {}
        }
        Ok(operation)
    }

    /// One tool's own path, `/api/v1/tools/<name>`, and its operation.
    fn tool(
        &mut self,
        tool: &Value,
        error: &Value,
    ) -> Result<(String, Map<String, Value>), String> {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .ok_or("a listed tool has no name")?;
        let about = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let input = self.add(
            &format!("tool_{name}_input"),
            tool.get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({})),
        )?;
        let output = self.add(
            &format!("tool_{name}_output"),
            tool.get("outputSchema")
                .cloned()
                .unwrap_or_else(|| json!({})),
        )?;
        let failure = self.add(
            "ToolFailureView",
            schema::schema_of::<super::tools::ToolFailureView>(),
        )?;
        let mut operation = Map::new();
        operation.insert("operationId".to_owned(), json!(format!("tool_{name}")));
        operation.insert("summary".to_owned(), json!(format!("Call {name}")));
        operation.insert("description".to_owned(), json!(about));
        operation.insert("tags".to_owned(), json!(["tools"]));
        operation.insert(
            "requestBody".to_owned(),
            json!({ "required": true, "content": { "application/json": { "schema": input } } }),
        );
        operation.insert(
            "responses".to_owned(),
            json!({
                "200": {
                    "description": "What the tool returned, or why it failed: a failed call \
                                    still ran.",
                    "content": { "application/json": {
                        "schema": { "oneOf": [output, failure] } } },
                },
            }),
        );
        if let Some(responses) = operation
            .get_mut("responses")
            .and_then(Value::as_object_mut)
        {
            errors(responses, error);
        }
        Ok((format!("/api/v1/tools/{name}"), operation))
    }
}

/// The error replies every operation can make.
fn errors(responses: &mut Map<String, Value>, error: &Value) {
    for (status, about) in [
        (
            "4XX",
            "Refused: the request, the key, or what it names. The body says why.",
        ),
        ("5XX", "The server failed, or is busy. The body says which."),
    ] {
        responses.insert(
            status.to_owned(),
            json!({ "description": about,
                    "content": { "application/json": { "schema": error } } }),
        );
    }
}

/// Points every reference to a schema's own part at that part's component.
fn rewrite(value: &mut Value, names: &BTreeMap<String, String>) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(reference)) = object.get_mut("$ref")
                && let Some(rest) = reference.strip_prefix(DEFS)
            {
                let (def, tail) = rest
                    .split_once('/')
                    .map_or((rest, ""), |(def, tail)| (def, tail));
                let called = names.get(def).map_or(def, String::as_str);
                let tail = if tail.is_empty() {
                    String::new()
                } else {
                    format!("/{tail}")
                };
                *reference = format!("{COMPONENTS}{called}{tail}");
            }
            for (key, inner) in object.iter_mut() {
                if !matches!(key.as_str(), "enum" | "const" | "default" | "examples") {
                    rewrite(inner, names);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| rewrite(item, names)),
        _ => {}
    }
}

/// Every reference in `document` that points at nothing.
#[cfg(test)]
pub(super) fn dangling(document: &Value) -> Vec<String> {
    fn walk(value: &Value, root: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                if let Some(Value::String(reference)) = object.get("$ref") {
                    let pointer = reference.strip_prefix('#').unwrap_or(reference);
                    if root.pointer(pointer).is_none() {
                        found.push(reference.clone());
                    }
                }
                object.values().for_each(|inner| walk(inner, root, found));
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, root, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(document, document, &mut found);
    found
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "a test")]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const ABOUT: About<'static> = About {
        title: "t",
        version: "1",
        server: "/",
        license: None,
    };

    #[test]
    fn every_route_is_in_the_document_once_and_nothing_dangles() {
        let routes = super::super::all_routes();
        let document = document(&routes, None, ABOUT).unwrap();
        assert_eq!(dangling(&document), Vec::<String>::new());
        let mut ids = BTreeSet::new();
        for route in &routes {
            assert!(ids.insert(route.id), "{} is used twice", route.id);
            let operation = &document["paths"][route.path][route.method.name()];
            assert_eq!(operation["operationId"], route.id, "{}", route.path);
            for name in route.path_params() {
                let declared = operation["parameters"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|param| param["name"] == name && param["in"] == "path");
                assert!(declared, "{} doesn't declare {name}", route.path);
            }
        }
        let operations: usize = document["paths"]
            .as_object()
            .unwrap()
            .values()
            .map(|path| path.as_object().unwrap().len())
            .sum();
        assert_eq!(operations, routes.len());
        assert_eq!(document["paths"]["/health"]["get"]["security"], json!([]));
    }

    #[test]
    fn a_served_document_lists_each_tool_in_place_of_the_generic_route() {
        let shared = json!({ "type": "object", "properties": { "x": { "type": "string" } } });
        let listing = json!({ "tools": [
            { "name": "a", "description": "A.",
              "inputSchema": { "type": "object",
                  "properties": { "p": { "$ref": "#/$defs/Part" } },
                  "$defs": { "Part": shared } },
              "outputSchema": { "type": "object" } },
            { "name": "b", "description": "B.",
              "inputSchema": { "type": "object",
                  "properties": { "p": { "$ref": "#/$defs/Part" } },
                  "$defs": { "Part": { "type": "integer" } } },
              "outputSchema": { "type": "object" } },
        ] });
        let routes = super::super::all_routes();
        let document = document(&routes, Some(&listing), ABOUT).unwrap();
        assert_eq!(dangling(&document), Vec::<String>::new());
        let paths = &document["paths"];
        assert!(paths.get("/api/v1/tools/{name}").is_none());
        assert_eq!(paths["/api/v1/tools/a"]["post"]["operationId"], "tool_a");
        let schemas = &document["components"]["schemas"];
        // Two different parts named Part: the second one is renamed.
        assert_eq!(schemas["Part"], shared);
        assert_eq!(schemas["tool_b_inputPart"], json!({ "type": "integer" }));
        assert_eq!(
            schemas["tool_b_input"]["properties"]["p"]["$ref"],
            "#/components/schemas/tool_b_inputPart"
        );
    }

    #[test]
    fn a_part_shared_by_two_schemas_is_kept_once() {
        let mut components = Components::default();
        let part = json!({ "type": "string" });
        for name in ["One", "Two"] {
            components
                .add(
                    name,
                    json!({ "properties": { "p": { "$ref": "#/$defs/P" } }, "$defs": { "P": part } }),
                )
                .unwrap();
        }
        assert_eq!(components.schemas.len(), 3);
        assert_eq!(
            components.schemas["Two"]["properties"]["p"]["$ref"],
            "#/components/schemas/P"
        );
    }
}
