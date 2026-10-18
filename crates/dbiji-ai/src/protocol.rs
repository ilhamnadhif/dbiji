//! Wire formats: request bodies and headers, server-sent events, stream
//! events and error payloads of the Anthropic Messages API and of OpenAI
//! compatible chat completions. Pure functions, no I/O.

use crate::config::{AiConfig, AiProviderKind};
use crate::types::{ChatRequest, ChatResponse, Role, Usage};
use dbiji_core::{DbError, DbResult, ErrorKind, Secret, tr, trf};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

/// `anthropic-version` header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta header enabling `fallbacks: "default"`.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Header value, redacted in `Debug` output when secret.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum HeaderValue {
    Plain(String),
    Secret(Secret),
}

impl HeaderValue {
    pub fn as_str(&self) -> &str {
        match self {
            HeaderValue::Plain(s) => s,
            HeaderValue::Secret(s) => s.expose(),
        }
    }
}

impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderValue::Plain(s) => write!(f, "{s:?}"),
            HeaderValue::Secret(_) => f.write_str("\"***\""),
        }
    }
}

/// A fully described HTTP request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, HeaderValue)>,
    pub body: Value,
}

impl HttpRequest {
    #[cfg(test)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Request variations toggled by the retry logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Flags {
    /// Send `fallbacks: "default"` with the beta header.
    pub fallback: bool,
    /// Send `output_config.effort`.
    pub effort: bool,
    /// OpenAI: send `max_completion_tokens` instead of `max_tokens`.
    pub max_completion_tokens: bool,
}

impl Flags {
    pub fn initial(cfg: &AiConfig) -> Flags {
        Flags { fallback: cfg.fallback_eligible(), effort: cfg.provider == AiProviderKind::Anthropic && cfg.effort.is_some(), max_completion_tokens: false }
    }
}

/// Check the conversation: non-empty, no empty messages, ends with the user
/// (assistant prefill is not supported by current models).
pub(crate) fn validate_request(req: &ChatRequest) -> DbResult<()> {
    if req.messages.is_empty() {
        return Err(DbError::config(tr("The conversation is empty")));
    }
    if req.messages.iter().any(|m| m.content.trim().is_empty()) {
        return Err(DbError::config(tr("Chat messages must not be empty")));
    }
    if req.messages.last().is_some_and(|m| m.role != Role::User) {
        return Err(DbError::config(tr("The conversation must end with a user message")));
    }
    Ok(())
}

/// Build the HTTP request for `req`.
pub(crate) fn build_request(cfg: &AiConfig, req: &ChatRequest, flags: Flags) -> DbResult<HttpRequest> {
    validate_request(req)?;
    let mut headers = vec![("content-type".to_string(), HeaderValue::Plain("application/json".into()))];
    let messages: Vec<Value> = req.messages.iter().map(|m| json!({"role": m.role.as_str(), "content": m.content})).collect();
    let body = match cfg.provider {
        AiProviderKind::Anthropic => {
            if let Some(k) = cfg.key() {
                headers.push(("x-api-key".into(), HeaderValue::Secret(Secret::new(k))));
            }
            headers.push(("anthropic-version".into(), HeaderValue::Plain(ANTHROPIC_VERSION.into())));
            if flags.fallback {
                headers.push(("anthropic-beta".into(), HeaderValue::Plain(FALLBACK_BETA.into())));
            }
            let mut body = json!({
                "model": cfg.model,
                "max_tokens": cfg.max_tokens,
                "messages": messages,
                "stream": true,
            });
            if !req.system.trim().is_empty() {
                body["system"] = json!([{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}]);
            }
            if flags.effort
                && let Some(e) = &cfg.effort
            {
                body["output_config"] = json!({"effort": e});
            }
            if flags.fallback {
                body["fallbacks"] = json!("default");
            }
            body
        }
        AiProviderKind::OpenAiCompatible => {
            if let Some(k) = cfg.key() {
                headers.push(("authorization".into(), HeaderValue::Secret(Secret::new(format!("Bearer {k}")))));
            }
            let mut all = Vec::with_capacity(messages.len() + 1);
            if !req.system.trim().is_empty() {
                all.push(json!({"role": "system", "content": req.system}));
            }
            all.extend(messages);
            let mut body = json!({"model": cfg.model, "messages": all, "stream": true});
            let key = if flags.max_completion_tokens { "max_completion_tokens" } else { "max_tokens" };
            body[key] = json!(cfg.max_tokens);
            body
        }
    };
    Ok(HttpRequest { url: cfg.endpoint(), headers, body })
}

// ------------------------------------------------------------------ SSE

/// One server-sent event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser of a `text/event-stream`, fed line by line.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feed one line (with or without its line terminator); returns an
    /// event when the line completes one.
    pub fn push_line(&mut self, line: &str) -> Option<SseEvent> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }

    /// End of stream: dispatch a pending event.
    pub fn finish(&mut self) -> Option<SseEvent> {
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let ev = SseEvent { event: self.event.take(), data: self.data.join("\n") };
        self.data.clear();
        Some(ev)
    }
}

// --------------------------------------------------------------- errors

/// Error reported by the service.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApiError {
    pub provider: AiProviderKind,
    pub status: Option<u16>,
    pub error_type: String,
    pub message: String,
    pub request_id: Option<String>,
    pub retry_after: Option<Duration>,
}

impl ApiError {
    /// Parse an error body (`{"type":"error","error":{...},"request_id":...}`
    /// or `{"error":{"message":...,"type":...,"code":...}}`).
    pub fn parse(provider: AiProviderKind, status: Option<u16>, body: &str, request_id: Option<String>, retry_after: Option<Duration>) -> ApiError {
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        Self::from_value(provider, status, &v, body, request_id, retry_after)
    }

    pub fn from_value(provider: AiProviderKind, status: Option<u16>, v: &Value, raw: &str, request_id: Option<String>, retry_after: Option<Duration>) -> ApiError {
        let err = &v["error"];
        let error_type = err["type"]
            .as_str()
            .map(str::to_string)
            .or_else(|| err["code"].as_str().map(str::to_string))
            .or_else(|| err["code"].as_i64().map(|c| c.to_string()))
            .unwrap_or_default();
        let message =
            err["message"].as_str().map(str::to_string).or_else(|| err.as_str().map(str::to_string)).or_else(|| v["message"].as_str().map(str::to_string)).unwrap_or_else(|| {
                let t = raw.trim();
                if t.is_empty() { String::new() } else { t.chars().take(500).collect() }
            });
        let request_id = request_id.or_else(|| v["request_id"].as_str().map(str::to_string));
        ApiError { provider, status, error_type, message, request_id, retry_after }
    }

    /// 429, 5xx, 529 and the matching error types are worth retrying.
    pub fn retryable(&self) -> bool {
        matches!(self.status, Some(429 | 500 | 502 | 503 | 504 | 529)) || matches!(self.error_type.as_str(), "rate_limit_error" | "api_error" | "overloaded_error")
    }

    fn text(&self) -> String {
        format!("{} {}", self.message, self.error_type).to_lowercase()
    }

    /// The service rejected the fallback beta / parameter.
    pub fn about_fallback(&self) -> bool {
        self.status == Some(400) && {
            let t = self.text();
            t.contains("anthropic-beta") || t.contains("fallback") || t.contains(crate::protocol::FALLBACK_BETA)
        }
    }

    /// The service rejected the effort parameter.
    pub fn about_effort(&self) -> bool {
        self.status == Some(400) && {
            let t = self.text();
            t.contains("effort") || t.contains("output_config")
        }
    }

    /// OpenAI: `max_tokens` must be `max_completion_tokens` for this model.
    pub fn about_max_completion_tokens(&self) -> bool {
        self.status == Some(400) && self.text().contains("max_completion_tokens")
    }

    pub fn to_db_error(&self) -> DbError {
        let provider = match self.provider {
            AiProviderKind::Anthropic => "Anthropic API",
            AiProviderKind::OpenAiCompatible => tr("AI service"),
        };
        let t = self.error_type.as_str();
        let (kind, what) = match (self.status, t) {
            (Some(401), _) | (_, "authentication_error" | "invalid_api_key") => (ErrorKind::Auth, tr("Authentication failed; check the API key")),
            (Some(402), _) | (_, "billing_error" | "insufficient_quota") => (ErrorKind::Auth, tr("Billing problem with the AI account")),
            (Some(403), _) | (_, "permission_error") => (ErrorKind::Auth, tr("Permission denied by the AI service")),
            (Some(404), _) | (_, "not_found_error" | "model_not_found") => (ErrorKind::Config, tr("Model or endpoint not found; check the model id")),
            (Some(413), _) | (_, "request_too_large") => (ErrorKind::Config, tr("The request is too large; reduce the schema context or the conversation")),
            (Some(429), _) | (_, "rate_limit_error") => (ErrorKind::Connection, tr("Rate limit reached; try again later")),
            (Some(529), _) | (_, "overloaded_error") => (ErrorKind::Connection, tr("The AI service is overloaded; try again later")),
            (Some(500..=599), _) | (_, "api_error") => (ErrorKind::Connection, tr("The AI service had an internal error")),
            (Some(400), _) | (_, "invalid_request_error") => (ErrorKind::Config, tr("The AI service rejected the request")),
            _ => (ErrorKind::Query, tr("The AI service returned an error")),
        };
        let status = self.status.map(|s| format!(" {s}")).unwrap_or_default();
        let message = if self.message.is_empty() { what.to_string() } else { format!("{what}: {}", self.message) };
        let mut e = DbError::new(kind, format!("{provider}{status}: {message}"));
        if !self.error_type.is_empty() {
            e = e.with_code(self.error_type.clone());
        } else if let Some(s) = self.status {
            e = e.with_code(s.to_string());
        }
        if let Some(r) = &self.request_id {
            e = e.with_detail(trf("Request id: {}", &[r]));
        }
        e
    }
}

/// Parse a `retry-after` header (seconds; HTTP dates are ignored).
pub(crate) fn parse_retry_after(v: &str) -> Option<Duration> {
    let s: f64 = v.trim().parse().ok()?;
    if s.is_finite() && s >= 0.0 { Some(Duration::from_secs_f64(s.min(3600.0))) } else { None }
}

// --------------------------------------------------------------- streams

/// What to do after an event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Control {
    Continue,
    Done,
    Error(ApiError),
}

/// Accumulated state of a streamed answer.
#[derive(Debug, Default, Clone)]
pub(crate) struct StreamState {
    pub text: String,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub stop_details: Option<Value>,
    pub usage: Usage,
    pub done: bool,
    pub fallback_from: Option<String>,
    /// Content block index -> block type.
    blocks: HashMap<u64, String>,
}

fn merge_usage(u: &mut Usage, v: &Value) {
    let get = |k: &str| v[k].as_u64();
    if let Some(x) = get("input_tokens") {
        u.input_tokens = x;
    }
    if let Some(x) = get("output_tokens") {
        u.output_tokens = x;
    }
    if let Some(x) = get("cache_read_input_tokens") {
        u.cache_read_input_tokens = x;
    }
    if let Some(x) = get("cache_creation_input_tokens") {
        u.cache_creation_input_tokens = x;
    }
}

impl StreamState {
    /// Handle one Anthropic event. Text deltas are passed to `on_delta`;
    /// thinking, signature and tool deltas are ignored.
    pub fn on_anthropic_event(&mut self, ev: &SseEvent, on_delta: &mut dyn FnMut(&str)) -> Control {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return Control::Continue;
        };
        let ty = v["type"].as_str().or(ev.event.as_deref()).unwrap_or("").to_string();
        match ty.as_str() {
            "message_start" => {
                if let Some(m) = v["message"]["model"].as_str() {
                    self.model = Some(m.to_string());
                }
                merge_usage(&mut self.usage, &v["message"]["usage"]);
            }
            "content_block_start" => {
                let block = &v["content_block"];
                let bt = block["type"].as_str().unwrap_or("").to_string();
                if let Some(i) = v["index"].as_u64() {
                    self.blocks.insert(i, bt.clone());
                }
                match bt.as_str() {
                    "text" => {
                        if let Some(t) = block["text"].as_str().filter(|t| !t.is_empty()) {
                            self.text.push_str(t);
                            on_delta(t);
                        }
                    }
                    "fallback" => {
                        // a declined attempt; the fallback model continues on the same stream
                        if let Some(from) = block["from"]["model"].as_str() {
                            self.fallback_from = Some(from.to_string());
                        }
                        if let Some(to) = block["to"]["model"].as_str() {
                            self.model = Some(to.to_string());
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                if d["type"].as_str() == Some("text_delta")
                    && let Some(t) = d["text"].as_str()
                {
                    self.text.push_str(t);
                    on_delta(t);
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(r.to_string());
                }
                let details = if v["delta"]["stop_details"].is_null() { &v["stop_details"] } else { &v["delta"]["stop_details"] };
                if !details.is_null() {
                    self.stop_details = Some(details.clone());
                }
                merge_usage(&mut self.usage, &v["usage"]);
            }
            "message_stop" => {
                self.done = true;
                return Control::Done;
            }
            "error" => {
                return Control::Error(ApiError::from_value(AiProviderKind::Anthropic, None, &v, &ev.data, None, None));
            }
            // ping, content_block_stop and future event types
            _ => {}
        }
        Control::Continue
    }

    /// Handle one OpenAI compatible chunk (`data: {...}` / `data: [DONE]`).
    pub fn on_openai_event(&mut self, ev: &SseEvent, on_delta: &mut dyn FnMut(&str)) -> Control {
        let data = ev.data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Control::Done;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Control::Continue;
        };
        if !v["error"].is_null() {
            return Control::Error(ApiError::from_value(AiProviderKind::OpenAiCompatible, None, &v, data, None, None));
        }
        if let Some(m) = v["model"].as_str() {
            self.model = Some(m.to_string());
        }
        let choice = &v["choices"][0];
        if let Some(t) = choice["delta"]["content"].as_str()
            && !t.is_empty()
        {
            self.text.push_str(t);
            on_delta(t);
        }
        if let Some(r) = choice["finish_reason"].as_str() {
            self.stop_reason = Some(r.to_string());
        }
        let u = &v["usage"];
        if !u.is_null() {
            if let Some(x) = u["prompt_tokens"].as_u64() {
                self.usage.input_tokens = x;
            }
            if let Some(x) = u["completion_tokens"].as_u64() {
                self.usage.output_tokens = x;
            }
            if let Some(x) = u["prompt_tokens_details"]["cached_tokens"].as_u64() {
                self.usage.cache_read_input_tokens = x;
            }
        }
        Control::Continue
    }

    /// Turn the final state into a response, mapping refusals to an error.
    pub fn finish(self, provider: AiProviderKind, requested_model: &str) -> DbResult<ChatResponse> {
        let reason = self.stop_reason.clone().unwrap_or_default();
        let refused = match provider {
            AiProviderKind::Anthropic => reason == "refusal",
            AiProviderKind::OpenAiCompatible => reason == "content_filter",
        };
        if refused {
            let category = self.stop_details.as_ref().and_then(|d| d["category"].as_str()).map(str::to_string);
            let explanation = self.stop_details.as_ref().and_then(|d| d["explanation"].as_str()).map(str::to_string);
            let msg = match &category {
                Some(c) => trf("The model declined this request (category: {})", &[c]),
                None => tr("The model declined this request").to_string(),
            };
            let mut e = DbError::query(msg).with_code("refusal");
            if let Some(x) = explanation {
                e = e.with_detail(x);
            }
            return Err(e);
        }
        let truncated = matches!(reason.as_str(), "max_tokens" | "length" | "model_context_window_exceeded");
        if !self.done && self.stop_reason.is_none() {
            return Err(DbError::connection(tr("The AI response stream ended unexpectedly")));
        }
        Ok(ChatResponse {
            text: self.text,
            stop_reason: self.stop_reason,
            truncated,
            usage: self.usage,
            model: self.model.unwrap_or_else(|| requested_model.to_string()),
            fallback_from: self.fallback_from,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    fn req() -> ChatRequest {
        ChatRequest::new("You are a SQL expert.\n\nSchema:\ntable public.users (id integer PK)", vec![ChatMessage::user("count users")])
    }

    #[test]
    fn anthropic_request_shape() {
        let cfg = AiConfig::anthropic("sk-ant-test");
        let r = build_request(&cfg, &req(), Flags::initial(&cfg)).unwrap();
        assert_eq!(r.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.header("x-api-key"), Some("sk-ant-test"));
        assert_eq!(r.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(r.header("anthropic-beta"), Some("server-side-fallback-2026-07-01"));
        assert_eq!(
            r.body,
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 16000,
                "system": [{"type": "text", "text": req().system, "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content": "count users"}],
                "stream": true,
                "output_config": {"effort": "medium"},
                "fallbacks": "default"
            })
        );
        for forbidden in ["thinking", "temperature", "top_p", "top_k"] {
            assert!(r.body.get(forbidden).is_none(), "{forbidden}");
        }
        // the key never appears in debug output
        assert!(!format!("{r:?}").contains("sk-ant-test"));
    }

    #[test]
    fn anthropic_request_without_fallback_or_effort() {
        let mut cfg = AiConfig::anthropic("k");
        cfg.model = "claude-haiku-5-5".into();
        let r = build_request(&cfg, &req(), Flags::initial(&cfg)).unwrap();
        assert!(r.header("anthropic-beta").is_none());
        assert!(r.body.get("fallbacks").is_none());
        assert_eq!(r.body["output_config"]["effort"], "medium");
        let mut proxy = AiConfig::anthropic("k");
        proxy.base_url = Some("https://gateway.example.com/anthropic".into());
        let r = build_request(&proxy, &req(), Flags::initial(&proxy)).unwrap();
        assert_eq!(r.url, "https://gateway.example.com/anthropic/v1/messages");
        assert!(r.header("anthropic-beta").is_none() && r.body.get("fallbacks").is_none());
        let mut no_effort = AiConfig::anthropic("k");
        no_effort.effort = None;
        let r = build_request(&no_effort, &req(), Flags::initial(&no_effort)).unwrap();
        assert!(r.body.get("output_config").is_none());
        let flags = Flags { fallback: false, effort: false, max_completion_tokens: false };
        let r = build_request(&AiConfig::anthropic("k"), &ChatRequest::new("", vec![ChatMessage::user("x")]), flags).unwrap();
        assert!(r.body.get("system").is_none(), "empty system prompt is omitted");
    }

    #[test]
    fn openai_request_shape() {
        let cfg = AiConfig::openai_compatible("http://localhost:11434", "qwen2.5-coder", None);
        let r = build_request(&cfg, &req(), Flags::initial(&cfg)).unwrap();
        assert_eq!(r.url, "http://localhost:11434/v1/chat/completions");
        assert!(r.header("authorization").is_none());
        assert_eq!(
            r.body,
            json!({
                "model": "qwen2.5-coder",
                "messages": [{"role": "system", "content": req().system}, {"role": "user", "content": "count users"}],
                "stream": true,
                "max_tokens": 16000
            })
        );
        let cfg = AiConfig::openai_compatible("https://api.openai.com", "gpt-x", Some("sk-o".into()));
        let r = build_request(&cfg, &req(), Flags { fallback: false, effort: false, max_completion_tokens: true }).unwrap();
        assert_eq!(r.header("authorization"), Some("Bearer sk-o"));
        assert!(r.body.get("max_tokens").is_none());
        assert_eq!(r.body["max_completion_tokens"], 16000);
        assert!(!format!("{r:?}").contains("sk-o"));
    }

    #[test]
    fn conversation_rules() {
        let cfg = AiConfig::anthropic("k");
        let f = Flags::initial(&cfg);
        assert!(build_request(&cfg, &ChatRequest::new("s", vec![]), f).is_err());
        let prefill = ChatRequest::new("s", vec![ChatMessage::user("hi"), ChatMessage::assistant("SELECT")]);
        assert!(build_request(&cfg, &prefill, f).is_err());
        assert!(build_request(&cfg, &ChatRequest::new("s", vec![ChatMessage::user("  ")]), f).is_err());
        let multi = ChatRequest::new("s", vec![ChatMessage::user("a"), ChatMessage::assistant("b"), ChatMessage::user("c")]);
        let r = build_request(&cfg, &multi, f).unwrap();
        assert_eq!(r.body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(r.body["messages"][1]["role"], "assistant");
    }

    fn feed(lines: &str) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        let mut out: Vec<SseEvent> = lines.split_inclusive('\n').filter_map(|l| p.push_line(l)).collect();
        out.extend(p.finish());
        out
    }

    #[test]
    fn sse_parsing() {
        let evs = feed("event: a\ndata: 1\n\n: comment\ndata: x\ndata: y\r\n\r\nid: 5\nretry: 10\nevent:b\ndata:z");
        assert_eq!(
            evs,
            vec![SseEvent { event: Some("a".into()), data: "1".into() }, SseEvent { event: None, data: "x\ny".into() }, SseEvent { event: Some("b".into()), data: "z".into() },]
        );
        assert!(feed("\n\n: only comments\n\n").is_empty());
    }

    const ANTHROPIC_STREAM: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":1200,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQB..."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"```sql\nSELECT count(*)"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" FROM users;\n```"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":42}}

event: message_stop
data: {"type":"message_stop"}

"#;

    fn run_anthropic(stream: &str) -> (StreamState, Vec<String>, Control) {
        let mut st = StreamState::default();
        let mut deltas = Vec::new();
        let mut last = Control::Continue;
        for ev in feed(stream) {
            last = st.on_anthropic_event(&ev, &mut |t: &str| deltas.push(t.to_string()));
            if last != Control::Continue {
                break;
            }
        }
        (st, deltas, last)
    }

    #[test]
    fn anthropic_stream_with_thinking_and_ping() {
        let (st, deltas, last) = run_anthropic(ANTHROPIC_STREAM);
        assert_eq!(last, Control::Done);
        assert_eq!(deltas, vec!["```sql\nSELECT count(*)", " FROM users;\n```"]);
        let r = st.finish(AiProviderKind::Anthropic, "claude-opus-5-5").unwrap();
        assert_eq!(r.text, "```sql\nSELECT count(*) FROM users;\n```");
        assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
        assert!(!r.truncated);
        assert_eq!(r.usage, Usage { input_tokens: 1200, output_tokens: 42, cache_read_input_tokens: 1000, cache_creation_input_tokens: 0 });
        assert_eq!(r.model, "claude-opus-5-5");
    }

    #[test]
    fn anthropic_max_tokens_refusal_error_and_fallback() {
        let truncated = ANTHROPIC_STREAM.replace("\"end_turn\"", "\"max_tokens\"");
        let (st, _, _) = run_anthropic(&truncated);
        assert!(st.finish(AiProviderKind::Anthropic, "m").unwrap().truncated);

        let refusal = r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-fable-5-1","usage":{"input_tokens":10,"output_tokens":0}}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"policy"}},"usage":{"output_tokens":0}}

event: message_stop
data: {"type":"message_stop"}
"#;
        let (st, _, _) = run_anthropic(refusal);
        let e = st.finish(AiProviderKind::Anthropic, "m").unwrap_err();
        assert_eq!(e.code.as_deref(), Some("refusal"));
        assert!(e.message.contains("declined") && e.message.contains("cyber"), "{e}");
        assert_eq!(e.detail.as_deref(), Some("policy"));

        let error = r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-5-5","usage":{}}}

event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}
"#;
        let (_, _, last) = run_anthropic(error);
        let Control::Error(err) = last else { panic!("expected error, got {last:?}") };
        assert!(err.retryable());
        assert_eq!(err.error_type, "overloaded_error");
        assert_eq!(err.to_db_error().kind, ErrorKind::Connection);

        let fallback = r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":5}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Hi"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}

event: message_stop
data: {"type":"message_stop"}
"#;
        let (st, deltas, _) = run_anthropic(fallback);
        assert_eq!(deltas, vec!["Hi"]);
        let r = st.finish(AiProviderKind::Anthropic, "claude-opus-5-5").unwrap();
        assert_eq!(r.model, "claude-opus-4-8");
        assert_eq!(r.fallback_from.as_deref(), Some("claude-opus-5-5"));

        // a stream cut before message_stop is an error
        let cut = ANTHROPIC_STREAM.split("event: message_delta").next().unwrap().to_string();
        let (st, _, _) = run_anthropic(&cut);
        assert_eq!(st.finish(AiProviderKind::Anthropic, "m").unwrap_err().kind, ErrorKind::Connection);
    }

    const OPENAI_STREAM: &str = r#"data: {"id":"c1","object":"chat.completion.chunk","model":"gpt-x","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"c1","model":"gpt-x","choices":[{"index":0,"delta":{"reasoning_content":"hmm"},"finish_reason":null}]}

data: {"id":"c1","model":"gpt-x","choices":[{"index":0,"delta":{"content":"SELECT 1"},"finish_reason":null}]}

: keep-alive

data: {"id":"c1","model":"gpt-x","choices":[{"index":0,"delta":{"content":";"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":8}}}

data: [DONE]

"#;

    fn run_openai(stream: &str) -> (StreamState, Vec<String>, Control) {
        let mut st = StreamState::default();
        let mut deltas = Vec::new();
        let mut last = Control::Continue;
        for ev in feed(stream) {
            last = st.on_openai_event(&ev, &mut |t: &str| deltas.push(t.to_string()));
            if last != Control::Continue {
                break;
            }
        }
        (st, deltas, last)
    }

    #[test]
    fn openai_stream() {
        let (st, deltas, last) = run_openai(OPENAI_STREAM);
        assert_eq!(last, Control::Done);
        assert_eq!(deltas, vec!["SELECT 1", ";"]);
        let r = st.finish(AiProviderKind::OpenAiCompatible, "x").unwrap();
        assert_eq!(r.text, "SELECT 1;");
        assert_eq!(r.model, "gpt-x");
        assert_eq!(r.usage.input_tokens, 20);
        assert_eq!(r.usage.output_tokens, 4);
        assert_eq!(r.usage.cache_read_input_tokens, 8);
        let length = OPENAI_STREAM.replace("\"stop\"", "\"length\"");
        let (st, _, _) = run_openai(&length);
        assert!(st.finish(AiProviderKind::OpenAiCompatible, "x").unwrap().truncated);
        let filtered = OPENAI_STREAM.replace("\"stop\"", "\"content_filter\"");
        let (st, _, _) = run_openai(&filtered);
        assert_eq!(st.finish(AiProviderKind::OpenAiCompatible, "x").unwrap_err().code.as_deref(), Some("refusal"));
        let (_, _, last) = run_openai("data: {\"error\":{\"message\":\"upstream timeout\",\"type\":\"server_error\",\"code\":502}}\n\n");
        let Control::Error(e) = last else { panic!("expected error") };
        assert_eq!(e.message, "upstream timeout");
        // no [DONE] but a finish reason: complete
        let no_done = OPENAI_STREAM.replace("data: [DONE]", "");
        let (st, _, _) = run_openai(&no_done);
        assert!(st.finish(AiProviderKind::OpenAiCompatible, "x").is_ok());
    }

    #[test]
    fn http_error_mapping() {
        let body = r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-nope"},"request_id":"req_011"}"#;
        let e = ApiError::parse(AiProviderKind::Anthropic, Some(404), body, None, None);
        assert!(!e.retryable());
        let d = e.to_db_error();
        assert_eq!(d.kind, ErrorKind::Config);
        assert_eq!(d.code.as_deref(), Some("not_found_error"));
        assert_eq!(d.detail.as_deref(), Some("Request id: req_011"));
        assert!(d.message.contains("404") && d.message.contains("claude-nope"), "{d}");
        let cases = [
            (400, "invalid_request_error", ErrorKind::Config, false),
            (401, "authentication_error", ErrorKind::Auth, false),
            (402, "billing_error", ErrorKind::Auth, false),
            (403, "permission_error", ErrorKind::Auth, false),
            (413, "request_too_large", ErrorKind::Config, false),
            (429, "rate_limit_error", ErrorKind::Connection, true),
            (500, "api_error", ErrorKind::Connection, true),
            (529, "overloaded_error", ErrorKind::Connection, true),
        ];
        for (status, ty, kind, retry) in cases {
            let body = format!(r#"{{"type":"error","error":{{"type":"{ty}","message":"m"}}}}"#);
            let e = ApiError::parse(AiProviderKind::Anthropic, Some(status), &body, Some("req_h".into()), None);
            assert_eq!(e.retryable(), retry, "{status}");
            assert_eq!(e.to_db_error().kind, kind, "{status}");
            assert_eq!(e.request_id.as_deref(), Some("req_h"));
        }
        let o = ApiError::parse(
            AiProviderKind::OpenAiCompatible,
            Some(401),
            r#"{"error":{"message":"Incorrect API key","type":"invalid_request_error","code":"invalid_api_key"}}"#,
            None,
            None,
        );
        assert_eq!(o.to_db_error().kind, ErrorKind::Auth);
        let html = ApiError::parse(AiProviderKind::OpenAiCompatible, Some(502), "<html>bad gateway</html>", None, None);
        assert!(html.retryable());
        assert!(html.message.contains("bad gateway"));
        let beta = ApiError::parse(
            AiProviderKind::Anthropic,
            Some(400),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `server-side-fallback-2026-07-01` for the `anthropic-beta` header."}}"#,
            None,
            None,
        );
        assert!(beta.about_fallback() && !beta.about_effort());
        let eff = ApiError::parse(
            AiProviderKind::Anthropic,
            Some(400),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"output_config.effort: not supported"}}"#,
            None,
            None,
        );
        assert!(eff.about_effort());
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(" 0.5 "), Some(Duration::from_millis(500)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }
}
