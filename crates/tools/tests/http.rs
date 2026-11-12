//! The `http_request` tool, against servers on the loopback.
#![allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{any, get};
use plugin_api::{Effect, Tool, ToolContext, ToolFailure, ToolOutput};
use protocol::{DataClass, RunId, Trust};
use schema::{Validator, check_strict};
use serde_json::{Value, json};
use tools::{Credential, HttpConfig, HttpConfigError, HttpRequestTool, Method};

fn context() -> ToolContext {
    ToolContext::new(RunId::generate(), DataClass::Private, 0)
}

const SECRET: &str = "s3cr3t-key-value";

/// A server, and how many requests have reached it.
struct Server {
    address: SocketAddr,
    hits: Arc<AtomicUsize>,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn serve_on(bind: &str, router: Router) -> Server {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&hits);
    let counting_router = router.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            count.fetch_add(1, Ordering::SeqCst);
            async move { next.run(request).await }
        },
    ));
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, counting_router).await.unwrap() });
    Server { address, hits }
}

fn html(body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
}

fn echo_headers(headers: &HeaderMap) -> String {
    let mut lines: Vec<String> = headers
        .iter()
        .map(|(name, value)| format!("{name}={}", value.to_str().unwrap_or("?")))
        .collect();
    lines.sort();
    lines.join("\n")
}

fn site() -> Router {
    Router::new()
        .route(
            "/page",
            get(|| async {
                html("<title>T</title><nav>menu</nav><p>Hello <b>there</b></p><script>x</script>")
            }),
        )
        .route("/json", get(|| async { axum::Json(json!({ "a": 1 })) }))
        .route(
            "/missing",
            get(|| async { (StatusCode::NOT_FOUND, "nothing here") }),
        )
        .route(
            "/png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], vec![0_u8, 1, 2]) }),
        )
        .route("/big", get(|| async { "é".repeat(200_000) }))
        .route("/empty", get(|| async { html("<script>app()</script>") }))
        .route(
            "/echo",
            any(|headers: HeaderMap| async move { echo_headers(&headers) }),
        )
        .route(
            "/hop",
            get(|| async { Redirect::temporary("/page").into_response() }),
        )
        .route(
            "/loop",
            get(|| async { Redirect::temporary("/loop").into_response() }),
        )
        .route(
            "/post-redirect",
            any(|| async { Redirect::to("/method").into_response() }),
        )
        .route(
            "/method",
            any(|method: axum::http::Method, body: String| async move {
                format!("{method}:{body}")
            }),
        )
}

fn loopback_config() -> HttpConfig {
    HttpConfig {
        allowed_ranges: vec!["127.0.0.1/32".to_owned()],
        ..HttpConfig::default()
    }
}

fn tool_with(config: &HttpConfig) -> HttpRequestTool {
    HttpRequestTool::new(config, &|_| Some(SECRET.to_owned())).unwrap()
}

fn tool() -> HttpRequestTool {
    tool_with(&loopback_config())
}

fn credential_for(address: SocketAddr) -> Credential {
    Credential {
        host: address.ip().to_string(),
        header: "x-api-key".to_owned(),
        prefix: "Key ".to_owned(),
        env: "THE_KEY".to_owned(),
        allow_insecure: true,
    }
}

async fn call(tool: &HttpRequestTool, args: Value) -> Result<ToolOutput, ToolFailure> {
    let definition = tool.definition();
    Validator::from_schema(&definition.input_schema)
        .unwrap()
        .check(&args)
        .unwrap();
    let output = tool.run(args, context()).await?;
    Validator::from_schema(&definition.output_schema)
        .unwrap()
        .check(&output.value)
        .unwrap();
    assert_eq!(output.trust, Trust::Untrusted);
    Ok(output)
}

async fn fetch(tool: &HttpRequestTool, url: &str) -> Value {
    call(tool, json!({ "url": url })).await.unwrap().value
}

async fn refused(tool: &HttpRequestTool, args: Value) -> String {
    call(tool, args).await.unwrap_err().message().to_owned()
}

#[test]
fn its_schemas_are_strict() {
    let definition = tool().definition();
    assert_eq!(check_strict(&definition.input_schema), vec![]);
    assert_eq!(check_strict(&definition.output_schema), vec![]);
}

#[test]
fn reading_is_a_read_and_sending_is_a_write() {
    let tool = tool();
    assert_eq!(tool.effect_of(&json!({ "url": "x" })), Effect::Read);
    assert_eq!(tool.effect_of(&json!({ "method": "HEAD" })), Effect::Read);
    assert_eq!(tool.effect_of(&json!({ "method": "POST" })), Effect::Write);
}

#[tokio::test]
async fn it_returns_a_pages_readable_text() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = fetch(&tool(), &server.url("/page")).await;
    assert_eq!(got["status"], 200);
    assert_eq!(got["title"], "T");
    assert_eq!(got["body"], "Hello there");
    assert_eq!(got["truncated"], false);
}

#[tokio::test]
async fn raw_mode_returns_the_markup() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = call(
        &tool(),
        json!({ "url": server.url("/page"), "mode": "raw" }),
    )
    .await
    .unwrap()
    .value;
    assert!(got["body"].as_str().unwrap().contains("<nav>"));
    assert_eq!(got["title"], Value::Null);
}

#[tokio::test]
async fn json_comes_back_as_text() {
    let server = serve_on("127.0.0.1:0", site()).await;
    assert_eq!(
        fetch(&tool(), &server.url("/json")).await["body"],
        r#"{"a":1}"#
    );
}

#[tokio::test]
async fn an_error_status_is_a_successful_call() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = fetch(&tool(), &server.url("/missing")).await;
    assert_eq!(got["status"], 404);
    assert_eq!(got["body"], "nothing here");
}

#[tokio::test]
async fn binary_says_why_it_was_left_out() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = fetch(&tool(), &server.url("/png")).await;
    assert_eq!(got["body"], Value::Null);
    assert!(
        got["omitted_reason"]
            .as_str()
            .unwrap()
            .contains("image/png")
    );
}

#[tokio::test]
async fn a_page_with_no_text_says_so() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = fetch(&tool(), &server.url("/empty")).await;
    assert!(
        got["omitted_reason"]
            .as_str()
            .unwrap()
            .contains("JavaScript")
    );
}

#[tokio::test]
async fn a_long_response_is_marked_truncated() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let shorter = tool_with(&HttpConfig {
        max_body_chars: 10,
        ..loopback_config()
    });
    let got = fetch(&shorter, &server.url("/big")).await;
    assert_eq!(got["body"].as_str().unwrap().chars().count(), 10);
    assert_eq!(got["truncated"], true);
}

#[tokio::test]
async fn reading_stops_at_the_limit_without_a_false_decoding_error() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let shorter = tool_with(&HttpConfig {
        max_read_bytes: 101, // ends halfway through a two-byte character
        ..loopback_config()
    });
    let got = fetch(&shorter, &server.url("/big")).await;
    assert_eq!(got["body"].as_str().unwrap().chars().count(), 50);
    assert_eq!(got["truncated"], true);
    assert_eq!(got["decoding_errors"], false);
}

#[tokio::test]
async fn internal_addresses_are_refused_however_they_are_written() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let default = tool_with(&HttpConfig::default());
    let port = server.address.port();
    for url in [
        format!("http://127.0.0.1:{port}/page"),
        format!("http://localhost:{port}/page"),
        format!("http://2130706433:{port}/page"),
        format!("http://0x7f.1:{port}/page"),
        format!("http://[::ffff:127.0.0.1]:{port}/page"),
        format!("http://[::1]:{port}/page"),
    ] {
        let error = refused(&default, json!({ "url": url })).await;
        assert!(error.contains("blocked"), "{url}: {error}");
    }
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn a_redirect_to_a_blocked_address_never_reaches_it() {
    let inside = serve_on("127.0.0.2:0", site()).await;
    let target = inside.url("/page");
    let router = Router::new().route(
        "/out",
        get(move || {
            let target = target.clone();
            async move { Redirect::temporary(&target).into_response() }
        }),
    );
    let outside = serve_on("127.0.0.1:0", router).await;
    let error = refused(&tool(), json!({ "url": outside.url("/out") })).await;
    assert!(error.contains("blocked"), "{error}");
    assert_eq!(inside.hits(), 0);
}

#[tokio::test]
async fn it_follows_redirects_and_counts_them() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = fetch(&tool(), &server.url("/hop")).await;
    assert_eq!(got["body"], "Hello there");
    assert_eq!(got["redirects"], 1);
    assert_eq!(got["url"], server.url("/page"));
}

#[tokio::test]
async fn too_many_redirects_are_reported() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = fetch(&tool(), &server.url("/loop")).await;
    assert_eq!(got["status"], 307);
    assert_eq!(got["redirects"], 5);
    assert!(
        got["redirect_not_followed"]
            .as_str()
            .unwrap()
            .contains("limit")
    );
    assert_eq!(server.hits(), 6);
}

#[tokio::test]
async fn a_post_redirected_with_see_other_drops_its_body() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let tool = tool_with(&HttpConfig {
        allowed_methods: vec![Method::Get, Method::Post],
        ..loopback_config()
    });
    let got = call(
        &tool,
        json!({ "url": server.url("/post-redirect"), "method": "POST", "body": "data" }),
    )
    .await
    .unwrap()
    .value;
    assert_eq!(got["body"], "GET:");
}

#[tokio::test]
async fn only_the_configured_methods_run() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let error = refused(
        &tool(),
        json!({ "url": server.url("/method"), "method": "POST", "body": "leak" }),
    )
    .await;
    assert!(error.contains("POST"), "{error}");
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn a_get_cant_carry_a_body() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let error = refused(
        &tool(),
        json!({ "url": server.url("/method"), "body": "x" }),
    )
    .await;
    assert!(error.contains("body"), "{error}");
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn a_redirect_cant_change_to_a_method_that_isnt_allowed() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let tool = tool_with(&HttpConfig {
        allowed_methods: vec![Method::Post],
        ..loopback_config()
    });
    let args = json!({ "url": server.url("/post-redirect"), "method": "POST", "body": "x" });
    let got = call(&tool, args).await.unwrap().value;
    assert_eq!(got["status"], 303);
    assert!(
        got["redirect_not_followed"]
            .as_str()
            .unwrap()
            .contains("GET")
    );
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn head_returns_no_body() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let got = call(
        &tool(),
        json!({ "url": server.url("/page"), "method": "HEAD" }),
    )
    .await
    .unwrap()
    .value;
    assert_eq!(got["status"], 200);
    assert_eq!(got["body"], Value::Null);
}

#[tokio::test]
async fn what_a_url_cant_do() {
    let tool = tool();
    for (url, expected) in [
        ("ftp://127.0.0.1/x", "http"),
        ("file:///etc/passwd", "http"),
        ("http://user:pass@127.0.0.1/", "user name"),
        ("not a url", "valid URL"),
    ] {
        let error = refused(&tool, json!({ "url": url })).await;
        assert!(error.contains(expected), "{url}: {error}");
    }
}

#[tokio::test]
async fn a_model_cant_set_identity_headers() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let config = HttpConfig {
        credentials: vec![credential_for(server.address)],
        ..loopback_config()
    };
    let tool = tool_with(&config);
    for name in [
        "Authorization",
        "COOKIE",
        "Host",
        "Proxy-Foo",
        "X-Api-Key",
        "Accept-Encoding",
    ] {
        let args = json!({
            "url": server.url("/echo"),
            "headers": [{ "name": name, "value": "x" }]
        });
        let error = refused(&tool, args).await;
        assert!(error.contains("may not set"), "{name}: {error}");
    }
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn a_model_can_set_ordinary_headers() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let args = json!({
        "url": server.url("/echo"),
        "headers": [{ "name": "X-Thing", "value": "yes" }]
    });
    let got = call(&tool(), args).await.unwrap().value;
    assert!(got["body"].as_str().unwrap().contains("x-thing=yes"));
}

#[tokio::test]
async fn a_key_is_attached_and_never_reaches_the_model() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let config = HttpConfig {
        credentials: vec![credential_for(server.address)],
        ..loopback_config()
    };
    let got = fetch(&tool_with(&config), &server.url("/echo")).await;
    let body = got["body"].as_str().unwrap();
    assert!(body.contains("x-api-key=Key [redacted]"), "{body}");
    assert!(!got.to_string().contains(SECRET));
}

#[tokio::test]
async fn a_key_needs_https_unless_allowed() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let config = HttpConfig {
        credentials: vec![Credential {
            allow_insecure: false,
            ..credential_for(server.address)
        }],
        ..loopback_config()
    };
    let error = refused(&tool_with(&config), json!({ "url": server.url("/echo") })).await;
    assert!(error.contains("https"), "{error}");
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn a_key_does_not_follow_a_redirect_to_another_origin() {
    let other = serve_on("127.0.0.1:0", site()).await;
    let target = other.url("/echo");
    let router = Router::new().route(
        "/out",
        get(move || {
            let target = target.clone();
            async move { Redirect::temporary(&target).into_response() }
        }),
    );
    let server = serve_on("127.0.0.1:0", router).await;
    let config = HttpConfig {
        credentials: vec![credential_for(server.address)],
        ..loopback_config()
    };
    let got = fetch(&tool_with(&config), &server.url("/out")).await;
    assert_eq!(got["status"], 307);
    assert!(
        got["redirect_not_followed"]
            .as_str()
            .unwrap()
            .contains("key")
    );
    assert_eq!(other.hits(), 0);
}

#[tokio::test]
async fn a_models_headers_stop_at_the_origin() {
    let other = serve_on("127.0.0.1:0", site()).await;
    let target = other.url("/echo");
    let router = Router::new().route(
        "/out",
        get(move || {
            let target = target.clone();
            async move { Redirect::temporary(&target).into_response() }
        }),
    );
    let server = serve_on("127.0.0.1:0", router).await;
    let args = json!({
        "url": server.url("/out"),
        "headers": [{ "name": "X-Thing", "value": "yes" }]
    });
    let got = call(&tool(), args).await.unwrap().value;
    assert_eq!(got["redirects"], 1);
    assert!(!got["body"].as_str().unwrap().contains("x-thing"));
}

#[tokio::test]
async fn a_slow_server_hits_the_time_limit() {
    let router = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            "late"
        }),
    );
    let server = serve_on("127.0.0.1:0", router).await;
    let tool = tool_with(&HttpConfig {
        timeout_seconds: 1,
        ..loopback_config()
    });
    let error = refused(&tool, json!({ "url": server.url("/slow") })).await;
    assert!(error.contains("longer than 1 seconds"), "{error}");
}

#[test]
fn a_config_that_blocks_nothing_is_refused() {
    let config = HttpConfig {
        blocked_ranges: Vec::new(),
        ..HttpConfig::default()
    };
    let error = HttpRequestTool::new(&config, &|_| None).unwrap_err();
    assert!(matches!(error, HttpConfigError::Invalid(_)), "{error}");
}

#[test]
fn a_missing_secret_is_refused_at_startup() {
    let config = HttpConfig {
        credentials: vec![credential_for("127.0.0.1:1".parse().unwrap())],
        ..HttpConfig::default()
    };
    let error = HttpRequestTool::new(&config, &|_| None).unwrap_err();
    assert_eq!(error, HttpConfigError::MissingSecret("THE_KEY".to_owned()));
}

#[test]
fn debug_output_does_not_show_a_key() {
    let config = HttpConfig {
        credentials: vec![credential_for("127.0.0.1:1".parse().unwrap())],
        ..HttpConfig::default()
    };
    let tool = tool_with(&config);
    assert!(!format!("{tool:?}").contains(SECRET));
}

#[tokio::test]
async fn a_plugins_request_comes_back_typed_and_raw() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let tool = tool();
    let page = tool
        .request_for_plugin(plugin_api::HttpRequest::get(server.url("/page")))
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert!(page.body.contains("<nav>menu</nav>"), "{}", page.body);
    assert!(!page.truncated);
    assert_eq!(page.note, None);
    let missing = tool
        .request_for_plugin(plugin_api::HttpRequest::get(server.url("/missing")))
        .await
        .unwrap();
    assert_eq!(missing.status, 404);
    let image = tool
        .request_for_plugin(plugin_api::HttpRequest::get(server.url("/png")))
        .await
        .unwrap();
    assert_eq!(image.body, "");
    assert!(image.note.unwrap().contains("image/png"));
}

#[tokio::test]
async fn a_plugins_request_meets_the_same_schema_and_rules_as_a_models() {
    let server = serve_on("127.0.0.1:0", site()).await;
    let tool = tool();
    let mut crowded = plugin_api::HttpRequest::get(server.url("/page"));
    for n in 0..17 {
        crowded = crowded.with_header(format!("x-{n}"), "v");
    }
    assert!(tool.request_for_plugin(crowded).await.is_err());
    let identity =
        plugin_api::HttpRequest::get(server.url("/page")).with_header("Host", "elsewhere");
    assert!(tool.request_for_plugin(identity).await.is_err());
    let blocked = plugin_api::HttpRequest::get("http://169.254.169.254/latest/meta-data");
    assert!(tool.request_for_plugin(blocked).await.is_err());
    assert_eq!(server.hits(), 0);
}
