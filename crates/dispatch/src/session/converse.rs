//! The round loop: ask the model, run what it asks for, repeat until it
//! answers or something stops it.

use model::{ChatRequest, FinishReason, Gateway, Message};
use schema::bounded;

use super::{ModelStop, Session, event_text};
use crate::trajectory::{Speaker, Step};
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
        self.record_messages(&messages).await;
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
                // An answer before a required tool ran goes back once.
                if !response.content.trim().is_empty()
                    && let Some(reminder) = self.reminder()
                {
                    messages.push(Message::assistant(response.content));
                    self.emit(Step::Message {
                        role: Speaker::User,
                        content: event_text(&reminder),
                        tool_calls: Vec::new(),
                    })
                    .await;
                    messages.push(Message::user(reminder));
                    continue;
                }
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

    /// Records the messages a run starts from, before anything the harness
    /// does for it, such as a memory search, so its trajectory shows the
    /// task before the calls made for it. [`Session::converse`] records only
    /// the messages added after these.
    pub async fn begin(&mut self, messages: &[Message]) {
        self.record_messages(messages).await;
    }

    /// Records what the model is given to start from, so a trajectory shows
    /// the task and the instructions it ran under. Messages already recorded
    /// by [`Session::begin`] aren't recorded again.
    async fn record_messages(&mut self, messages: &[Message]) {
        let new = messages.get(self.messages_recorded..).unwrap_or_default();
        self.messages_recorded = messages.len();
        for message in new {
            self.emit(Step::Message {
                role: message.role.into(),
                content: event_text(&message.content),
                tool_calls: message
                    .tool_calls
                    .iter()
                    .map(|call| super::recorded_name(&call.name))
                    .collect(),
            })
            .await;
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
