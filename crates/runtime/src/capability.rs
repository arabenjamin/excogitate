//! The `http` and `model` capabilities a plugin can be granted, made from
//! the runtime's own parts: HTTP is the `http_request` tool, with its
//! network rules, and model calls go through the gateway, routed and charged
//! like the runtime's own.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use model::{ChatRequest, Gateway, Message, RoutePolicy};
use plugin_api::{
    CapabilityError, CapabilityUse, HttpCapability, HttpRequest, HttpResponse, ModelCapability,
    ModelReply, PluginCapability, ToolContext,
};
use plugin_host::{CapabilityMeter, CapabilitySource};
use tools::HttpRequestTool;

/// Makes both capabilities for any plugin that's granted them.
#[derive(Debug)]
pub(crate) struct RuntimeCapabilities {
    pub(crate) http: Arc<HttpRequestTool>,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) max_output_tokens: u32,
}

impl CapabilitySource for RuntimeCapabilities {
    fn http(&self, _plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn HttpCapability> {
        Arc::new(PluginHttp {
            tool: Arc::clone(&self.http),
            meter,
        })
    }

    fn model(&self, plugin: &str, meter: Arc<CapabilityMeter>) -> Arc<dyn ModelCapability> {
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

/// Refuses a capability call once the run has no tool calls left: each one
/// counts as a tool call.
fn check_calls_left(meter: &CapabilityMeter, context: ToolContext) -> Result<(), CapabilityError> {
    if meter.calls(context.run) >= context.tool_calls_left {
        return Err(CapabilityError::Refused(
            "the run has no tool calls left, and a capability call counts as one".to_owned(),
        ));
    }
    Ok(())
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug)]
struct PluginHttp {
    tool: Arc<HttpRequestTool>,
    meter: Arc<CapabilityMeter>,
}

#[async_trait]
impl HttpCapability for PluginHttp {
    async fn request(
        &self,
        request: HttpRequest,
        context: ToolContext,
    ) -> Result<HttpResponse, CapabilityError> {
        check_calls_left(&self.meter, context)?;
        let what = schema::bounded(&format!(
            "{} {}",
            request.method.to_uppercase(),
            request.url
        ));
        let begun = Instant::now();
        let answer = self
            .tool
            .request_for_plugin(request)
            .await
            .map_err(|failure| CapabilityError::Refused(failure.message().to_owned()));
        let used = CapabilityUse::new(PluginCapability::Http, what, answer.is_ok())
            .with_duration_ms(millis(begun));
        self.meter.record(context.run, used);
        answer
    }
}

#[derive(Debug)]
struct PluginModel {
    gateway: Arc<Gateway>,
    purpose: String,
    meter: Arc<CapabilityMeter>,
    max_output_tokens: u32,
}

#[async_trait]
impl ModelCapability for PluginModel {
    async fn complete(
        &self,
        prompt: &str,
        context: ToolContext,
    ) -> Result<ModelReply, CapabilityError> {
        // One call at a time per run, so what's left is known when it's
        // checked: what the run had, less what this plugin has spent in the
        // call that's still running.
        let _turn = self.meter.turn(context.run).await;
        check_calls_left(&self.meter, context)?;
        let begun = Instant::now();
        let budget = context
            .budget_usd_micros
            .saturating_sub(self.meter.pending(context.run));
        let policy = RoutePolicy {
            capabilities: Vec::new(),
            data_class: context.data_class,
            purpose: self.purpose.clone(),
            budget_usd_micros: Some(budget),
            embedding_dimension: None,
            only_model: None,
        };
        let request = ChatRequest {
            messages: vec![Message::user(prompt)],
            tools: Vec::new(),
            max_output_tokens: self.max_output_tokens,
            temperature: None,
            response_schema: None,
        };
        let completion = match self.gateway.chat(&policy, &request).await {
            Ok(completion) => completion,
            Err(error) => {
                let used = CapabilityUse::new(PluginCapability::Model, "no model answered", false)
                    .with_duration_ms(millis(begun));
                self.meter.record(context.run, used);
                return Err(CapabilityError::Failed(schema::bounded(&error.to_string())));
            }
        };
        let answered = !completion.response.content.trim().is_empty();
        let used = CapabilityUse::new(PluginCapability::Model, &completion.model, answered)
            .with_cost(completion.cost_usd_micros)
            .with_duration_ms(millis(begun));
        self.meter.record(context.run, used);
        if !answered {
            return Err(CapabilityError::Failed(
                "the model's reply was empty".to_owned(),
            ));
        }
        Ok(ModelReply::new(
            completion.response.content,
            completion.model,
            completion.cost_usd_micros,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugins_purpose_carries_its_name() {
        assert_eq!(purpose("feeds"), "plugin:feeds");
    }
}
