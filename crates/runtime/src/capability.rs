//! The `http` and `model` capabilities a plugin can be granted, made from
//! the runtime's own parts: HTTP is the `http_request` tool, with its
//! network rules, and model calls go through the gateway, routed and charged
//! like the runtime's own.

use std::sync::Arc;

use async_trait::async_trait;
use model::{ChatRequest, Gateway, Message, RoutePolicy};
use plugin_api::{
    CapabilityError, HttpCapability, HttpRequest, HttpResponse, ModelCapability, ModelReply, Tool,
    ToolContext,
};
use plugin_host::{CapabilitySource, SpendMeter};
use serde_json::{Value, json};
use tools::HttpRequestTool;

/// Makes both capabilities for any plugin that's granted them.
#[derive(Debug)]
pub(crate) struct RuntimeCapabilities {
    pub(crate) http: Arc<HttpRequestTool>,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) max_output_tokens: u32,
}

impl CapabilitySource for RuntimeCapabilities {
    fn http(&self, _plugin: &str) -> Arc<dyn HttpCapability> {
        // The tool's own schema, the same one a model's call meets, so a
        // plugin can't send what a model couldn't. It's the tool's
        // generated schema, so it always compiles; if it somehow didn't,
        // every request would be refused rather than sent unchecked.
        let validator = schema::Validator::from_schema(&self.http.definition().input_schema).ok();
        Arc::new(PluginHttp {
            tool: Arc::clone(&self.http),
            validator,
        })
    }

    fn model(&self, plugin: &str, meter: Arc<SpendMeter>) -> Arc<dyn ModelCapability> {
        Arc::new(PluginModel {
            gateway: Arc::clone(&self.gateway),
            purpose: purpose(plugin),
            meter,
            max_output_tokens: self.max_output_tokens,
        })
    }
}

/// What a plugin's model calls are for, which a paid provider's `paid_for`
/// list has to name before the plugin may pay.
fn purpose(plugin: &str) -> String {
    format!("plugin:{plugin}")
}

#[derive(Debug)]
struct PluginHttp {
    tool: Arc<HttpRequestTool>,
    validator: Option<schema::Validator>,
}

/// The `http_request` arguments for a plugin's request: raw mode, since a
/// plugin parses what it fetches itself.
fn arguments(request: HttpRequest) -> Value {
    let headers: Vec<Value> = request
        .headers
        .into_iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    let mut arguments = json!({
        "url": request.url,
        "method": request.method.to_uppercase(),
        "headers": headers,
        "mode": "raw",
    });
    if let (Some(body), Some(fields)) = (request.body, arguments.as_object_mut()) {
        fields.insert("body".to_owned(), Value::String(body));
    }
    arguments
}

fn response(value: &Value) -> Result<HttpResponse, CapabilityError> {
    let status = value
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .ok_or_else(|| CapabilityError::Failed("the response had no status".to_owned()))?;
    let text = |field: &str| value.get(field).and_then(Value::as_str).map(str::to_owned);
    Ok(HttpResponse {
        status,
        body: text("body").unwrap_or_default(),
        truncated: value.get("truncated").and_then(Value::as_bool) == Some(true),
        note: text("omitted_reason").or_else(|| text("redirect_not_followed")),
    })
}

#[async_trait]
impl HttpCapability for PluginHttp {
    async fn request(
        &self,
        request: HttpRequest,
        context: ToolContext,
    ) -> Result<HttpResponse, CapabilityError> {
        let arguments = arguments(request);
        let validator = self
            .validator
            .as_ref()
            .ok_or_else(|| CapabilityError::Failed("HTTP isn't set up".to_owned()))?;
        validator
            .check(&arguments)
            .map_err(|refusal| CapabilityError::Refused(refusal.to_string()))?;
        let output = self
            .tool
            .run(arguments, context)
            .await
            .map_err(|failure| CapabilityError::Refused(failure.0))?;
        response(&output.value)
    }
}

#[derive(Debug)]
struct PluginModel {
    gateway: Arc<Gateway>,
    purpose: String,
    meter: Arc<SpendMeter>,
    max_output_tokens: u32,
}

#[async_trait]
impl ModelCapability for PluginModel {
    async fn complete(
        &self,
        prompt: &str,
        context: ToolContext,
    ) -> Result<ModelReply, CapabilityError> {
        // What's left of the run, less what this plugin has already spent in
        // the call that's still running.
        let budget = context
            .budget_usd_micros
            .saturating_sub(self.meter.pending(context.run));
        let policy = RoutePolicy {
            capabilities: Vec::new(),
            data_class: context.data_class,
            purpose: self.purpose.clone(),
            budget_usd_micros: Some(budget),
            embedding_dimension: None,
        };
        let request = ChatRequest {
            messages: vec![Message::user(prompt)],
            tools: Vec::new(),
            max_output_tokens: self.max_output_tokens,
            temperature: None,
            response_schema: None,
        };
        let completion = self
            .gateway
            .chat(&policy, &request)
            .await
            .map_err(|error| CapabilityError::Failed(schema::bounded(&error.to_string())))?;
        self.meter.add(context.run, completion.cost_usd_micros);
        if completion.response.content.trim().is_empty() {
            return Err(CapabilityError::Failed(
                "the model's reply was empty".to_owned(),
            ));
        }
        Ok(ModelReply {
            text: completion.response.content,
            model: completion.model,
            cost_usd_micros: completion.cost_usd_micros,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_becomes_raw_http_request_arguments() {
        let built = arguments(HttpRequest {
            method: "POST".to_owned(),
            url: "https://example.com/a".to_owned(),
            headers: vec![("accept".to_owned(), "text/plain".to_owned())],
            body: Some("x".to_owned()),
        });
        assert_eq!(
            built,
            json!({
                "url": "https://example.com/a", "method": "POST", "mode": "raw", "body": "x",
                "headers": [{ "name": "accept", "value": "text/plain" }],
            })
        );
    }

    #[test]
    fn a_missing_body_says_why() {
        let read = response(
            &json!({ "status": 200, "truncated": false, "omitted_reason": "image/png isn't text" }),
        )
        .unwrap();
        assert_eq!(read.body, "");
        assert_eq!(read.note.as_deref(), Some("image/png isn't text"));
        assert!(response(&json!({})).is_err());
    }

    #[test]
    fn a_plugins_purpose_carries_its_name() {
        assert_eq!(purpose("feeds"), "plugin:feeds");
    }
}
