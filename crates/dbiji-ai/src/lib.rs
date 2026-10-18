//! AI assistant for DBiji (PRD phase 3: chat and query suggestions with the
//! user's own API key; DBeaver parity: chat, SQL generation, query
//! explanation, error fixing, object description, several providers).
//!
//! * [`AiConfig`] / [`AiProviderKind`]: Anthropic Messages API or any OpenAI
//!   compatible service (OpenAI, Ollama, LM Studio, OpenRouter ...),
//! * [`AiClient::chat_stream`]: streamed answer with retries, refusal
//!   handling and cancellation,
//! * [`prompts`]: system prompt, task prompts, schema context, SQL extraction.
//!
//! Privacy: the only network connections are to the configured service; no
//! data rows are ever sent, only schema structure and what the user types.
//! The API key is a [`dbiji_core::Secret`]: never serialised or logged.

// `DbError` (dbiji-core) is 144 bytes; every `DbResult` triggers this lint.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

mod client;
mod config;
pub mod prompts;
mod protocol;
mod types;

pub use client::AiClient;
pub use config::{
    ANTHROPIC_BASE_URL, ANTHROPIC_MODEL_PRESETS, AiConfig, AiProviderKind, DEFAULT_ANTHROPIC_MODEL, EFFORT_LEVELS, LM_STUDIO_BASE_URL, OLLAMA_BASE_URL, OPENAI_BASE_URL,
    RetryPolicy, SERVER_FALLBACK_MODELS,
};
pub use prompts::{SchemaContext, SchemaContextBuilder, extract_sql_blocks, system_prompt};
pub use protocol::{ANTHROPIC_VERSION, FALLBACK_BETA};
pub use types::{ChatMessage, ChatRequest, ChatResponse, Role, Usage};
