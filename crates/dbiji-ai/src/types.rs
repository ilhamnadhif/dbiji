//! Chat request and response types.

use serde::{Deserialize, Serialize};

/// Author of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The person using DBiji.
    User,
    /// The model.
    Assistant,
}

impl Role {
    /// Wire name (`user` / `assistant`).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One message of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Author.
    pub role: Role,
    /// Plain text content.
    pub content: String,
}

impl ChatMessage {
    /// Message from the user.
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage { role: Role::User, content: content.into() }
    }
    /// Earlier answer of the model (conversation history).
    pub fn assistant(content: impl Into<String>) -> Self {
        ChatMessage { role: Role::Assistant, content: content.into() }
    }
}

/// A request: system prompt (instructions plus schema context) and the
/// conversation, which must end with a user message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Instructions and schema context ([`crate::system_prompt`]).
    pub system: String,
    /// Conversation, oldest first.
    pub messages: Vec<ChatMessage>,
}

impl ChatRequest {
    /// Request from a system prompt and messages.
    pub fn new(system: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        ChatRequest { system: system.into(), messages }
    }
}

/// Token usage reported by the service.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens: for Anthropic those neither read from nor written to the
    /// cache (see the cache fields); for OpenAI compatible services all of them.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_input_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_creation_input_tokens: u64,
}

/// Completed answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Full answer text (the concatenation of all streamed deltas).
    pub text: String,
    /// Why generation stopped (`end_turn`, `max_tokens`, `stop`, `length` ...).
    pub stop_reason: Option<String>,
    /// The answer was cut off by the token limit.
    pub truncated: bool,
    /// Token usage.
    pub usage: Usage,
    /// Model that produced the answer (may differ from the requested one
    /// after a server-side fallback).
    pub model: String,
    /// Model that declined the request when a fallback model answered instead.
    pub fallback_from: Option<String>,
}
