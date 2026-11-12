//! The HTTP side of the port.

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::redirect::Policy;
use schema::bounded;
use serde_json::Value;

use crate::wire;
use crate::{ChatRequest, ChatResponse, Model};

/// The largest reply read from a model server.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// A model call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelError {
    /// The request never got an answer.
    #[error("could not reach the model server: {0}")]
    Transport(String),
    /// It took too long.
    #[error("the model server timed out")]
    Timeout,
    /// The server said to slow down.
    #[error("the model server is rate limiting")]
    RateLimited,
    /// The server answered with an error.
    #[error("the model server answered {code}: {excerpt}")]
    Status {
        /// The HTTP status.
        code: u16,
        /// The start of what it said.
        excerpt: String,
    },
    /// The reply was larger than anything a model should send.
    #[error("the model server's reply is too large")]
    TooLarge,
    /// The reply wasn't what the wire format says.
    #[error("the model server's reply is malformed: {0}")]
    BadResponse(String),
}

impl ModelError {
    /// Whether another model could do better. A request the server calls
    /// malformed or too big would fail the same way anywhere.
    #[must_use]
    pub fn worth_trying_elsewhere(&self) -> bool {
        !matches!(
            self,
            Self::Status {
                code: 400 | 413 | 422,
                ..
            }
        )
    }

    /// Whether the request may have reached the model, so it may be billed:
    /// it timed out, or the server answered with something unusable. A
    /// refusal, a rate limit or a request that never got through isn't.
    #[must_use]
    pub const fn reached_the_model(&self) -> bool {
        matches!(self, Self::Timeout | Self::TooLarge | Self::BadResponse(_))
    }
}

/// Talks to a model server. Everything above this is tested against fakes;
/// [`OpenAiClient`] is the real one.
#[async_trait]
pub trait ModelClient: Send + Sync + std::fmt::Debug {
    /// Makes one chat call, with the reply limited to `output_tokens`.
    ///
    /// # Errors
    ///
    /// [`ModelError`] if the call fails or the reply is malformed.
    async fn chat(
        &self,
        model: &Model,
        request: &ChatRequest,
        output_tokens: u32,
    ) -> Result<ChatResponse, ModelError>;

    /// Embeds each input. Returns one vector per input, in order, each of the
    /// model's width.
    ///
    /// # Errors
    ///
    /// [`ModelError`] if the call fails or the reply has the wrong shape.
    async fn embed(&self, model: &Model, inputs: &[String]) -> Result<Vec<Vec<f32>>, ModelError>;
}

/// The client for any endpoint on the `OpenAI` wire.
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
}

impl OpenAiClient {
    /// A client. It doesn't follow redirects, so a key can't be carried to
    /// another host.
    ///
    /// # Errors
    ///
    /// [`ModelError::Transport`] if the HTTP stack can't start.
    pub fn new() -> Result<Self, ModelError> {
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|error| ModelError::Transport(error.without_url().to_string()))?;
        Ok(Self { http })
    }

    async fn post(&self, model: &Model, path: &str, body: &Value) -> Result<Vec<u8>, ModelError> {
        let provider = &model.provider;
        let mut request = self
            .http
            .post(format!("{}{path}", provider.base_url))
            .timeout(provider.timeout)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(key) = &provider.api_key {
            request = request.bearer_auth(key.expose());
        }
        let mut response = request.send().await.map_err(map_error)?;
        let status = response.status();
        let body = read_bounded(&mut response).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ModelError::RateLimited);
        }
        if !status.is_success() {
            let excerpt = match &body {
                Ok(bytes) => bounded(&String::from_utf8_lossy(bytes)),
                Err(_) => String::new(),
            };
            return Err(ModelError::Status {
                code: status.as_u16(),
                excerpt,
            });
        }
        body
    }
}

async fn read_bounded(response: &mut reqwest::Response) -> Result<Vec<u8>, ModelError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ModelError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn map_error(error: reqwest::Error) -> ModelError {
    if error.is_timeout() {
        ModelError::Timeout
    } else {
        ModelError::Transport(bounded(&error.without_url().to_string()))
    }
}

#[async_trait]
impl ModelClient for OpenAiClient {
    async fn chat(
        &self,
        model: &Model,
        request: &ChatRequest,
        output_tokens: u32,
    ) -> Result<ChatResponse, ModelError> {
        let body = wire::chat_body(model, request, output_tokens);
        wire::parse_chat(&self.post(model, "/chat/completions", &body).await?)
    }

    async fn embed(&self, model: &Model, inputs: &[String]) -> Result<Vec<Vec<f32>>, ModelError> {
        let width = model.embedding_dimension.ok_or_else(|| {
            ModelError::BadResponse(format!("{} has no embedding width", model.name))
        })?;
        let body = wire::embeddings_body(model, inputs);
        let reply = self.post(model, "/embeddings", &body).await?;
        wire::parse_embeddings(&reply, inputs.len(), width)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::catalog::test_support::catalog;
    use crate::{FinishReason, Message};
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode as AxumStatus, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{any, post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct Seen {
        bodies: Arc<Mutex<Vec<Value>>>,
        auth: Arc<Mutex<Vec<Option<String>>>>,
        other_hits: Arc<Mutex<u32>>,
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{address}/v1")
    }

    fn model_at(base: &str, key: bool, timeout: u32) -> Arc<Model> {
        let key = if key { ", api_key_env: K" } else { "" };
        let yaml = format!(
            "version: 1\nproviders:\n  - {{name: p, base_url: '{base}', no_log: true, timeout_secs: {timeout}{key}}}\n\
             models:\n  - {{name: m, provider: p, wire_model: w, capabilities: [chat, embeddings], embedding_dimension: 2, context_tokens: 99, max_output_tokens: 9}}\n"
        );
        Arc::clone(&catalog(&yaml).models()[0])
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_output_tokens: 5,
            temperature: None,
            response_schema: None,
        }
    }

    fn recording(seen: Seen, reply: Value) -> Router {
        Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, body: String| {
                let seen = seen.clone();
                let reply = reply.clone();
                async move {
                    seen.bodies
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&body).unwrap());
                    seen.auth.lock().unwrap().push(
                        headers
                            .get(header::AUTHORIZATION)
                            .map(|v| v.to_str().unwrap().to_owned()),
                    );
                    reply.to_string()
                }
            }),
        )
    }

    fn ok_reply() -> Value {
        json!({ "choices": [{ "message": { "content": "ok" }, "finish_reason": "stop" }] })
    }

    #[tokio::test]
    async fn a_call_reaches_the_server_with_its_key() {
        let seen = Seen::default();
        let base = serve(recording(seen.clone(), ok_reply())).await;
        let client = OpenAiClient::new().unwrap();
        let reply = client
            .chat(&model_at(&base, true, 5), &request(), 5)
            .await
            .unwrap();
        assert_eq!(
            (reply.content.as_str(), reply.finish),
            ("ok", FinishReason::Stop)
        );
        assert_eq!(seen.bodies.lock().unwrap()[0]["max_tokens"], 5);
        assert_eq!(
            seen.auth.lock().unwrap()[0].as_deref(),
            Some("Bearer the-key")
        );
    }

    #[tokio::test]
    async fn no_key_sends_no_header() {
        let seen = Seen::default();
        let base = serve(recording(seen.clone(), ok_reply())).await;
        let client = OpenAiClient::new().unwrap();
        client
            .chat(&model_at(&base, false, 5), &request(), 5)
            .await
            .unwrap();
        assert_eq!(seen.auth.lock().unwrap()[0], None);
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let seen = Seen::default();
        let other = seen.clone();
        let target = serve(Router::new().route(
            "/{*rest}",
            any(move || {
                let other = other.clone();
                async move {
                    *other.other_hits.lock().unwrap() += 1;
                    ok_reply().to_string()
                }
            }),
        ))
        .await;
        let location = format!("{target}/chat/completions");
        let base = serve(Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let location = location.clone();
                async move {
                    (
                        AxumStatus::TEMPORARY_REDIRECT,
                        [(header::LOCATION, location)],
                    )
                        .into_response()
                }
            }),
        ))
        .await;
        let client = OpenAiClient::new().unwrap();
        let error = client
            .chat(&model_at(&base, true, 5), &request(), 5)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ModelError::Status { code: 307, .. }),
            "{error:?}"
        );
        assert_eq!(*seen.other_hits.lock().unwrap(), 0);
    }

    async fn failing(status: AxumStatus, body: String) -> String {
        serve(Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let body = body.clone();
                async move { (status, body).into_response() }
            }),
        ))
        .await
    }

    #[tokio::test]
    async fn rate_limits_and_errors_are_told_apart() {
        let client = OpenAiClient::new().unwrap();
        let base = failing(AxumStatus::TOO_MANY_REQUESTS, String::new()).await;
        let error = client
            .chat(&model_at(&base, false, 5), &request(), 5)
            .await
            .unwrap_err();
        assert_eq!(error, ModelError::RateLimited);
        assert!(error.worth_trying_elsewhere());

        let base = failing(AxumStatus::BAD_REQUEST, "x".repeat(100_000)).await;
        let error = client
            .chat(&model_at(&base, false, 5), &request(), 5)
            .await
            .unwrap_err();
        assert!(!error.worth_trying_elsewhere());
        assert!(error.to_string().chars().count() < 400);
    }

    #[tokio::test]
    async fn an_oversized_reply_is_refused() {
        let client = OpenAiClient::new().unwrap();
        let base = failing(AxumStatus::OK, "x".repeat(MAX_RESPONSE_BYTES + 1)).await;
        let error = client
            .chat(&model_at(&base, false, 5), &request(), 5)
            .await
            .unwrap_err();
        assert_eq!(error, ModelError::TooLarge);
    }

    #[tokio::test]
    async fn a_slow_server_times_out() {
        let base = serve(Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Response::new(axum::body::Body::empty())
            }),
        ))
        .await;
        let client = OpenAiClient::new().unwrap();
        let error = client
            .chat(&model_at(&base, false, 1), &request(), 5)
            .await
            .unwrap_err();
        assert_eq!(error, ModelError::Timeout);
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_transport_error() {
        let client = OpenAiClient::new().unwrap();
        let model = model_at("http://127.0.0.1:1/v1", false, 5);
        let error = client.chat(&model, &request(), 5).await.unwrap_err();
        assert!(matches!(error, ModelError::Transport(_)), "{error:?}");
    }

    #[tokio::test]
    async fn embeddings_go_to_their_own_path() {
        let base = serve(Router::new().route(
            "/v1/embeddings",
            post(|| async { json!({ "data": [{ "embedding": [1, 2] }] }).to_string() }),
        ))
        .await;
        let client = OpenAiClient::new().unwrap();
        let vectors = client
            .embed(&model_at(&base, false, 5), &["a".to_owned()])
            .await
            .unwrap();
        assert_eq!(vectors, vec![vec![1.0, 2.0]]);
    }
}
