//! The model port. It talks to any endpoint on the `OpenAI` wire, picks a model
//! by capability, privacy and budget, and turns model text into typed values.
//!
//! Models are named only in configuration, never in code. Everything that
//! comes back from a model is treated as untrusted input.

#![deny(clippy::string_slice, clippy::indexing_slicing)]

mod catalog;
mod chat;
mod client;
mod gateway;
mod ledger;
mod route;
mod structured;
mod wire;

pub use catalog::{
    ApiKey, Capability, Catalog, ConfigError, Model, ModelConfig, ModelsConfig, Price, Provider,
    ProviderConfig,
};
pub use chat::{
    ChatRequest, ChatResponse, FinishReason, Message, ResponseSchema, Role, ToolCall, ToolSpec,
    Usage, estimate_tokens,
};
pub use client::{ModelClient, ModelError, OpenAiClient};
pub use gateway::{Completion, Embedded, FailedAttempt, Gateway, GatewayError};
pub use ledger::{MemoryLedger, SpendLedger};
pub use route::{Candidate, NoRoute, Rejection, RoutePolicy, TokenEstimate, plan};
#[doc(hidden)]
pub use wire::parse_chat;

pub use structured::{Structured, StructuredError, first_json_object, generate_structured};
