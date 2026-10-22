//! AI provider configuration.

use dbiji_core::{DbError, DbResult, Secret, tr, trf};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Wire protocol of the AI service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProviderKind {
    /// Anthropic Messages API (`POST /v1/messages`).
    #[default]
    Anthropic,
    /// OpenAI compatible chat completions (`POST /v1/chat/completions`):
    /// OpenAI, Ollama, LM Studio, OpenRouter, vLLM ...
    OpenAiCompatible,
}

impl AiProviderKind {
    /// English label for the settings UI.
    pub fn label(self) -> &'static str {
        match self {
            AiProviderKind::Anthropic => "Anthropic",
            AiProviderKind::OpenAiCompatible => "OpenAI compatible",
        }
    }
}

/// First-party Anthropic API.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// OpenAI API.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com";
/// Local Ollama server.
pub const OLLAMA_BASE_URL: &str = "http://localhost:11434";
/// Local LM Studio server.
pub const LM_STUDIO_BASE_URL: &str = "http://localhost:1234";

/// Default Anthropic model.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5-5";
/// Anthropic models offered in the settings.
pub const ANTHROPIC_MODEL_PRESETS: &[&str] = &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"];
/// Models supporting the server-side refusal fallback (`fallbacks: "default"`).
pub const SERVER_FALLBACK_MODELS: &[&str] = &["claude-opus-5-5", "claude-opus-5", "claude-fable-5-1", "claude-sonnet-5-5"];
/// Accepted `effort` values (Anthropic).
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Retry behaviour for transient errors (429, 500, 529, network).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryPolicy {
    /// Retries after the first attempt (default 2).
    pub max_retries: u32,
    /// First backoff delay; doubles with every retry.
    pub base_delay_ms: u64,
    /// Upper bound of any wait, including `retry-after`.
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_retries: 2, base_delay_ms: 1000, max_delay_ms: 60_000 }
    }
}

/// Settings of the AI assistant. The API key is not serialised: the
/// application keeps it in the secret store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Wire protocol of the service.
    pub provider: AiProviderKind,
    /// Service base URL; `None` uses the provider default
    /// ([`ANTHROPIC_BASE_URL`], [`OPENAI_BASE_URL`]). A trailing `/v1` is accepted.
    pub base_url: Option<String>,
    /// Model id, e.g. `claude-opus-5-5` (see [`ANTHROPIC_MODEL_PRESETS`]) or `llama3.1`.
    pub model: String,
    /// API key (from the secret store; optional for local servers). Not serialised.
    #[serde(skip)]
    pub api_key: Option<Secret>,
    /// Maximum tokens of an answer (default 16000).
    pub max_tokens: u32,
    /// Anthropic `output_config.effort` (`low`, `medium`, `high`, `xhigh`, `max`).
    pub effort: Option<String>,
    /// Ask the first-party Anthropic API to re-run declined requests on a
    /// fallback model (`fallbacks: "default"`), for the models in
    /// [`SERVER_FALLBACK_MODELS`].
    pub server_fallback: bool,
    /// Limit for a whole request, streaming included.
    pub timeout_secs: u64,
    /// Retries of transient failures.
    pub retry: RetryPolicy,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            provider: AiProviderKind::Anthropic,
            base_url: None,
            model: DEFAULT_ANTHROPIC_MODEL.to_string(),
            api_key: None,
            max_tokens: 16_000,
            effort: Some("medium".into()),
            server_fallback: true,
            timeout_secs: 900,
            retry: RetryPolicy::default(),
        }
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let authority = rest.split('/').next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(stripped) = authority.strip_prefix('[') { stripped.split(']').next()?.to_string() } else { authority.split(':').next()?.to_string() };
    Some(host.to_ascii_lowercase())
}

impl AiConfig {
    /// Anthropic with the default model.
    pub fn anthropic(api_key: impl Into<Secret>) -> Self {
        AiConfig { api_key: Some(api_key.into()), ..Default::default() }
    }

    /// OpenAI compatible service (`base_url` e.g. [`OLLAMA_BASE_URL`]).
    pub fn openai_compatible(base_url: impl Into<String>, model: impl Into<String>, api_key: Option<Secret>) -> Self {
        AiConfig {
            provider: AiProviderKind::OpenAiCompatible,
            base_url: Some(base_url.into()),
            model: model.into(),
            api_key,
            effort: None,
            server_fallback: false,
            ..Default::default()
        }
    }

    /// [`AiConfig::timeout_secs`] as a duration.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }

    /// Base URL without a trailing slash.
    pub fn base(&self) -> String {
        let default = match self.provider {
            AiProviderKind::Anthropic => ANTHROPIC_BASE_URL,
            AiProviderKind::OpenAiCompatible => OPENAI_BASE_URL,
        };
        self.base_url.as_deref().map(str::trim).filter(|b| !b.is_empty()).unwrap_or(default).trim_end_matches('/').to_string()
    }

    /// Full URL of the chat endpoint. The base may be the endpoint itself
    /// (`…/v1/chat/completions`, as providers' docs show it), a versioned base
    /// (`…/v1`, `…/v1beta/openai`) or the bare host (`/v1/…` is added).
    pub fn endpoint(&self) -> String {
        let base = self.base();
        let path = match self.provider {
            AiProviderKind::Anthropic => "messages",
            AiProviderKind::OpenAiCompatible => "chat/completions",
        };
        if base.ends_with(&format!("/{path}")) {
            return base;
        }
        // A version segment in the path (`v1`, `v1beta`, `v3`): the path goes after the base.
        let after_host = base.split_once("://").map_or(base.as_str(), |(_, rest)| rest);
        let versioned = after_host.split('/').skip(1).any(|seg| {
            let mut c = seg.chars();
            c.next() == Some('v') && c.next().is_some_and(|d| d.is_ascii_digit())
        });
        if versioned { format!("{base}/{path}") } else { format!("{base}/v1/{path}") }
    }

    /// The base URL is the first-party Anthropic API.
    pub fn is_first_party(&self) -> bool {
        self.provider == AiProviderKind::Anthropic && {
            let b = self.base().to_ascii_lowercase();
            b == ANTHROPIC_BASE_URL || b == format!("{ANTHROPIC_BASE_URL}/v1")
        }
    }

    /// Server-side refusal fallback applies to this configuration.
    pub fn fallback_eligible(&self) -> bool {
        self.server_fallback && self.is_first_party() && SERVER_FALLBACK_MODELS.contains(&self.model.as_str())
    }

    /// The service runs on this machine (no proxy, plain HTTP acceptable).
    pub fn is_loopback(&self) -> bool {
        matches!(host_of(&self.base()).as_deref(), Some("localhost" | "127.0.0.1" | "::1")) || host_of(&self.base()).is_some_and(|h| h.starts_with("127."))
    }

    fn api_key(&self) -> Option<&str> {
        self.api_key.as_ref().map(Secret::expose).filter(|k| !k.trim().is_empty())
    }

    /// Check the settings before sending anything.
    pub fn validate(&self) -> DbResult<()> {
        if self.model.trim().is_empty() {
            return Err(DbError::config(tr("Choose a model for the AI assistant")));
        }
        if self.max_tokens == 0 {
            return Err(DbError::config(tr("The maximum number of tokens must be at least 1")));
        }
        let base = self.base();
        let lower = base.to_ascii_lowercase();
        let https = lower.starts_with("https://");
        if !https && !lower.starts_with("http://") {
            return Err(DbError::config(trf("The AI service URL must start with http:// or https://: {}", &[&base])));
        }
        if host_of(&base).is_none_or(|h| h.is_empty()) {
            return Err(DbError::config(trf("Invalid AI service URL: {}", &[&base])));
        }
        if self.provider == AiProviderKind::Anthropic {
            if let Some(e) = &self.effort
                && !EFFORT_LEVELS.contains(&e.as_str())
            {
                return Err(DbError::config(trf("Invalid effort '{}' (use low, medium, high, xhigh or max)", &[e])));
            }
            if self.is_first_party() && self.api_key().is_none() {
                return Err(DbError::auth(tr("An API key is required for the Anthropic API")));
            }
        }
        if self.api_key().is_some() && !https && !self.is_loopback() {
            return Err(DbError::config(tr("Refusing to send the API key over unencrypted HTTP to a remote host; use https")));
        }
        Ok(())
    }

    pub(crate) fn key(&self) -> Option<&str> {
        self.api_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_endpoints() {
        let c = AiConfig::anthropic("sk-test");
        assert_eq!(c.model, "claude-opus-5-5");
        assert_eq!(c.max_tokens, 16_000);
        assert_eq!(c.effort.as_deref(), Some("medium"));
        assert!(c.server_fallback);
        assert_eq!(c.endpoint(), "https://api.anthropic.com/v1/messages");
        assert!(c.is_first_party() && c.fallback_eligible());
        c.validate().unwrap();
        let o = AiConfig::openai_compatible("http://localhost:11434/", "llama3.1", None);
        assert_eq!(o.endpoint(), "http://localhost:11434/v1/chat/completions");
        assert!(o.is_loopback());
        o.validate().unwrap();
        let o = AiConfig::openai_compatible("https://openrouter.ai/api/v1", "x", Some("k".into()));
        assert_eq!(o.endpoint(), "https://openrouter.ai/api/v1/chat/completions");
        let d = AiConfig { provider: AiProviderKind::OpenAiCompatible, base_url: None, ..Default::default() };
        assert_eq!(d.endpoint(), "https://api.openai.com/v1/chat/completions");
        // The endpoint itself, a versioned base, or the host.
        let at = |base: &str| AiConfig { provider: AiProviderKind::OpenAiCompatible, base_url: Some(base.into()), ..AiConfig::default() }.endpoint();
        assert_eq!(at("https://api.xiaomimimo.com/v1/chat/completions"), "https://api.xiaomimimo.com/v1/chat/completions");
        assert_eq!(at("https://api.xiaomimimo.com/v1/"), "https://api.xiaomimimo.com/v1/chat/completions");
        assert_eq!(at("https://api.xiaomimimo.com"), "https://api.xiaomimimo.com/v1/chat/completions");
        assert_eq!(at("https://generativelanguage.googleapis.com/v1beta/openai"), "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions");
        assert_eq!(at("https://open.bigmodel.cn/api/paas/v4"), "https://open.bigmodel.cn/api/paas/v4/chat/completions");
        let anthropic = AiConfig { base_url: Some("https://proxy.example/v1/messages".into()), ..AiConfig::default() };
        assert_eq!(anthropic.endpoint(), "https://proxy.example/v1/messages");
    }

    #[test]
    fn fallback_eligibility() {
        let mut c = AiConfig::anthropic("k");
        for m in ["claude-opus-5-5", "claude-opus-5", "claude-fable-5-1", "claude-sonnet-5-5"] {
            c.model = m.into();
            assert!(c.fallback_eligible(), "{m}");
        }
        c.model = "claude-haiku-5-5".into();
        assert!(!c.fallback_eligible());
        c.model = "claude-opus-5-5".into();
        c.base_url = Some("https://proxy.example.com".into());
        assert!(!c.fallback_eligible(), "only on the first-party API");
        c.base_url = Some("https://api.anthropic.com/".into());
        assert!(c.fallback_eligible());
        c.server_fallback = false;
        assert!(!c.fallback_eligible());
    }

    #[test]
    fn validation() {
        assert!(AiConfig::default().validate().is_err(), "key required");
        let mut c = AiConfig::anthropic("k");
        c.effort = Some("extreme".into());
        assert!(c.validate().is_err());
        c.effort = None;
        c.validate().unwrap();
        c.model = " ".into();
        assert!(c.validate().is_err());
        let o = AiConfig::openai_compatible("http://10.0.0.5:11434", "m", Some("secret".into()));
        assert!(o.validate().is_err(), "no key over plain http to a remote host");
        let o = AiConfig::openai_compatible("ftp://x", "m", None);
        assert!(o.validate().is_err());
        assert!(AiConfig::openai_compatible("http://127.0.0.1:8080", "m", Some("k".into())).validate().is_ok());
        assert!(AiConfig::openai_compatible("http://[::1]:8080", "m", Some("k".into())).is_loopback());
    }

    #[test]
    fn serde_never_contains_the_key() {
        let c = AiConfig::anthropic("sk-very-secret");
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("sk-very-secret"));
        assert!(!format!("{c:?}").contains("sk-very-secret"));
        let back: AiConfig = serde_json::from_str(&json).unwrap();
        assert!(back.api_key.is_none());
        assert_eq!(back.model, c.model);
    }
}
