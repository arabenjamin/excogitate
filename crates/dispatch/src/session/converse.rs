//! The round loop: ask the model, run what it asks for, repeat until it
//! answers or something stops it.

use model::{ChatRequest, FinishReason, Gateway, Message};
use schema::bounded;

use super::{ModelStop, Session};
use crate::{BudgetKind, Outcome, Receipt};

/// What a run is asked to do.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// The conversation so far, ending with what to respond to.
    pub messages: Vec<Message>,
    /// The longest each of the model's replies may be.
    pub max_output_tokens: u32,
    /// Sampling temperature, if the caller has a view.
    pub temperature: Option<f32>,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatResult {
    /// The answer. Only a run that answered has one.
    pub answer: Option<String>,
    /// Why there's no answer.
    pub stopped_because: Option<String>,
    /// What actually happened.
    pub receipt: Receipt,
}

/// One round's verdict, when it ended the run.
struct Ending {
    claimed: Outcome,
    answer: Option<String>,
    stopped_because: Option<String>,
}

impl Ending {
    fn stopped(claimed: Outcome, why: &str) -> Self {
        Self {
            claimed,
            answer: None,
            stopped_because: Some(bounded(why)),
        }
    }

    fn budget(kind: BudgetKind) -> Self {
        let name = serde_json::to_value(kind)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        Self::stopped(Outcome::BudgetHit, &format!("the run hit its {name} limit"))
    }
}

impl Session {
    /// Runs the conversation to its end and returns the receipt. It always
    /// ends: every round uses up the rounds budget.
    pub async fn converse(mut self, gateway: &Gateway, conversation: Conversation) -> ChatResult {
        let ending = self.rounds(gateway, conversation).await;
        ChatResult {
            answer: ending.answer,
            stopped_because: ending.stopped_because,
            receipt: self.finish(ending.claimed).await,
        }
    }

    async fn rounds(&mut self, gateway: &Gateway, conversation: Conversation) -> Ending {
        let Conversation {
            mut messages,
            max_output_tokens,
            temperature,
        } = conversation;
        loop {
            let request = ChatRequest {
                messages: messages.clone(),
                tools: Vec::new(),
                max_output_tokens,
                temperature,
                response_schema: None,
            };
            let completion = match self.call_model(gateway, &[], request).await {
                Ok(completion) => completion,
                Err(ModelStop::Budget(kind)) => return Ending::budget(kind),
                Err(ModelStop::Failed(error)) => {
                    return Ending::stopped(Outcome::ModelFailed, &error.to_string());
                }
            };
            let response = completion.response;
            if response.tool_calls.is_empty() {
                return self.final_reply(response.content, response.finish);
            }
            let mut assistant = Message::assistant(response.content);
            assistant.tool_calls.clone_from(&response.tool_calls);
            messages.push(assistant);
            for call in &response.tool_calls {
                let reply = self.dispatch(call).await;
                messages.push(Message::tool(reply.call_id, reply.content));
            }
        }
    }

    /// The model stopped calling tools. Nothing at all isn't an answer.
    fn final_reply(&mut self, content: String, finish: FinishReason) -> Ending {
        if content.trim().is_empty() {
            let why = "the model's reply was empty";
            if let Some(entry) = self.model_calls.last_mut() {
                entry.detail = Some(why.to_owned());
            }
            return Ending::stopped(Outcome::ModelFailed, why);
        }
        self.answer_cut_off = finish == FinishReason::Length;
        Ending {
            claimed: Outcome::Answered,
            answer: Some(content),
            stopped_because: None,
        }
    }
}
