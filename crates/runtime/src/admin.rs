//! `people` and `keys` on the command line: thin calls to the running
//! server's API with the owner key. The store takes one process, so the
//! command line can't open it while the server runs; it asks the server.

use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};

use crate::http::check_owner_key;
use crate::run::RunError;

/// How to use the two commands.
pub(crate) const USAGE: &str = "\
people                                   list the people
people put <slug> <name> <role> [disabled]
                                         add or change one; role is viewer, member or admin
keys [person:<slug>]                     list keys, or one person's
keys make <person:slug> <scopes> <name>  make a key; scopes is a comma list of
                                         read, chat, run and admin
keys revoke <id>                         revoke a key";

/// Where the server is, and the owner key to reach it with.
fn server(product: &str) -> Result<(String, String), RunError> {
    let prefix = product.to_uppercase();
    let variable = format!("{prefix}_OWNER_KEY");
    let key = std::env::var(&variable).unwrap_or_default();
    check_owner_key(&key).map_err(|problem| RunError::OwnerKey { variable, problem })?;
    let url = std::env::var(format!("{prefix}_URL"))
        .ok()
        .filter(|url| !url.trim().is_empty())
        .or_else(|| {
            crate::run::health_url(std::env::var(format!("{prefix}_BIND")).ok().as_deref())
                .and_then(|health| health.strip_suffix("/health").map(str::to_owned))
        })
        .unwrap_or_else(|| format!("http://{}", crate::run::DEFAULT_BIND));
    Ok((url.trim_end_matches('/').to_owned(), key))
}

async fn ask(
    product: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, RunError> {
    let (url, key) = server(product)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| RunError::Admin(error.to_string()))?;
    let mut request = client
        .request(method, format!("{url}{path}"))
        .bearer_auth(key);
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(|error| RunError::Admin(format!("{url}: {error}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| RunError::Admin(error.to_string()))?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        Ok(value)
    } else {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        Err(RunError::Admin(format!("{status}: {message}")))
    }
}

fn show(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// Runs `people …` or `keys …`. Returns None if the words aren't one of
/// them, so the caller can print the usage.
pub(crate) async fn run(product: &str, words: &[&str]) -> Option<Result<(), RunError>> {
    let call = match words {
        ["people"] => (Method::GET, "/api/v1/people?limit=200".to_owned(), None),
        ["people", "put", slug, name, role, rest @ ..] => (
            Method::PUT,
            format!("/api/v1/people/{slug}"),
            Some(json!({ "name": name, "role": role, "disabled": rest == ["disabled"] })),
        ),
        ["keys"] => (Method::GET, "/api/v1/keys?limit=200".to_owned(), None),
        ["keys", "make", person, scopes, name] => (
            Method::POST,
            "/api/v1/keys".to_owned(),
            Some(json!({
                "person": person,
                "scopes": scopes.split(',').map(str::trim).collect::<Vec<_>>(),
                "name": name,
            })),
        ),
        ["keys", "revoke", id] => (Method::DELETE, format!("/api/v1/keys/{id}"), None),
        ["keys", person] if person.starts_with("person:") => (
            Method::GET,
            format!("/api/v1/keys?limit=200&person={person}"),
            None,
        ),
        _ => return None,
    };
    let (method, path, body) = call;
    let made_a_key = method == Method::POST;
    Some(ask(product, method, &path, body).await.map(|value| {
        if !value.is_null() {
            show(&value);
        }
        if made_a_key {
            eprintln!("the secret is shown this once; keep it somewhere safe");
        }
    }))
}
