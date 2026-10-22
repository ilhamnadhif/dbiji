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
    /// The result of a tool the model called ([`ChatMessage::tool_call_id`]).
    Tool,
}

impl Role {
    /// Wire name (`user` / `assistant` / `tool`).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A function the model may call (agent mode): it answers with
/// [`ChatResponse::tool_calls`] and gets the results as [`Role::Tool`] messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDef {
    /// Name the model calls it by.
    pub name: String,
    /// What it does and when to use it (read by the model).
    pub description: String,
    /// JSON schema of the arguments (an object).
    pub parameters: String,
}

/// A call of a tool by the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Id the result refers to.
    pub id: String,
    /// The tool called.
    pub name: String,
    /// Arguments as JSON text.
    pub arguments: String,
}

/// One message of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Author.
    pub role: Role,
    /// Plain text content.
    pub content: String,
    /// The model's tool calls (an assistant message of a tool round).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The call a [`Role::Tool`] message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The model's reasoning of a tool round, sent back with it (OpenAI
    /// compatible thinking models ask for it within the round).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl ChatMessage {
    fn new(role: Role, content: String) -> Self {
        ChatMessage { role, content, tool_calls: Vec::new(), tool_call_id: None, reasoning: None }
    }
    /// Message from the user.
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage::new(Role::User, content.into())
    }
    /// Earlier answer of the model (conversation history).
    pub fn assistant(content: impl Into<String>) -> Self {
        ChatMessage::new(Role::Assistant, content.into())
    }
    /// The model's answer that called tools (with its text and reasoning, if any).
    pub fn assistant_calls(content: impl Into<String>, calls: Vec<ToolCall>, reasoning: Option<String>) -> Self {
        ChatMessage { tool_calls: calls, reasoning: reasoning.filter(|r| !r.is_empty()), ..ChatMessage::new(Role::Assistant, content.into()) }
    }
    /// The result of tool call `id`.
    pub fn tool_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        ChatMessage { tool_call_id: Some(id.into()), ..ChatMessage::new(Role::Tool, content.into()) }
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
    /// Tools the model may call (empty: none).
    #[serde(default)]
    pub tools: Vec<ToolDef>,
}

impl ChatRequest {
    /// Request from a system prompt and messages.
    pub fn new(system: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        ChatRequest { system: system.into(), messages, tools: Vec::new() }
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
    /// Tools the model called (`stop_reason` `tool_use` / `tool_calls`).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Reasoning text (OpenAI compatible `reasoning_content`), not shown.
    #[serde(default)]
    pub reasoning: String,
}
