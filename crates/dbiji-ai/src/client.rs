//! HTTP client for the AI services.
//!
//! Requests run on a blocking thread (`ureq`) inside `spawn_blocking`;
//! streamed text is passed to the caller's callback as it arrives and
//! cancellation is checked between chunks (the async call returns at once
//! when cancelled; the blocking thread stops at the next chunk).

use crate::config::{AiConfig, AiProviderKind};
use crate::protocol::{ApiError, Control, Flags, HttpRequest, SseParser, StreamState, build_request, parse_retry_after};
use crate::types::{ChatRequest, ChatResponse};
use dbiji_core::{DbError, DbResult, tr, trf};
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// Client for one AI configuration. Cheap to clone; safe to share.
#[derive(Clone)]
pub struct AiClient {
    config: AiConfig,
    agent: ureq::Agent,
}

impl std::fmt::Debug for AiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiClient").field("config", &self.config).finish()
    }
}

fn agent_for(cfg: &AiConfig) -> ureq::Agent {
    // explicit crypto provider and Mozilla roots: no process-wide default needed
    let crypto = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = TlsConfig::builder().provider(TlsProvider::Rustls).root_certs(RootCerts::WebPki).unversioned_rustls_crypto_provider(crypto).build();
    let timeout = cfg.timeout();
    let mut b = ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(timeout.min(Duration::from_secs(30))))
        .user_agent(format!("DBiji/{}", env!("CARGO_PKG_VERSION")));
    if cfg.is_loopback() {
        b = b.proxy(None);
    }
    b.build().new_agent()
}

impl AiClient {
    /// Validate `config` and create a client. No connection is made.
    pub fn new(config: AiConfig) -> DbResult<AiClient> {
        config.validate()?;
        let agent = agent_for(&config);
        Ok(AiClient { config, agent })
    }

    /// The configuration the client was created with.
    pub fn config(&self) -> &AiConfig {
        &self.config
    }

    /// Send `req` and stream the answer: `on_delta` receives text as it
    /// arrives (from a background thread). Returns the complete answer.
    /// A refusal is returned as an error with code `refusal`; text already
    /// received for such an answer should be discarded.
    pub async fn chat_stream(&self, req: ChatRequest, on_delta: impl FnMut(&str) + Send + 'static, cancel: CancellationToken) -> DbResult<ChatResponse> {
        crate::protocol::validate_request(&req)?;
        let cfg = self.config.clone();
        let agent = self.agent.clone();
        let token = cancel.clone();
        let mut on_delta = on_delta;
        let task = tokio::task::spawn_blocking(move || run_blocking(&cfg, &agent, &req, Flags::initial(&cfg), &mut on_delta, &token));
        tokio::select! {
            r = task => r.map_err(|_| DbError::internal(tr("The AI request stopped unexpectedly")))?,
            _ = cancel.cancelled() => Err(DbError::cancelled()),
        }
    }

    /// [`AiClient::chat_stream`] without incremental output.
    pub async fn chat(&self, req: ChatRequest, cancel: CancellationToken) -> DbResult<ChatResponse> {
        self.chat_stream(req, |_| {}, cancel).await
    }
}

/// Sleep up to `d`, waking early (with an error) when cancelled.
fn wait(d: Duration, cancel: &CancellationToken) -> DbResult<()> {
    let end = Instant::now() + d;
    loop {
        if cancel.is_cancelled() {
            return Err(DbError::cancelled());
        }
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        std::thread::sleep((end - now).min(Duration::from_millis(50)));
    }
}

fn backoff(cfg: &AiConfig, attempt: u32, retry_after: Option<Duration>) -> Duration {
    let max = Duration::from_millis(cfg.retry.max_delay_ms);
    let exp = Duration::from_millis(cfg.retry.base_delay_ms.saturating_mul(1u64 << attempt.min(16)));
    retry_after.unwrap_or(exp).min(max)
}

fn transport_error(e: ureq::Error) -> (DbError, bool) {
    use ureq::Error as E;
    match e {
        E::Timeout(t) => (DbError::timeout(trf("The AI service did not answer in time ({})", &[&t])), false),
        E::HostNotFound => (DbError::connection(tr("The AI service host was not found")), false),
        E::ConnectionFailed => (DbError::connection(tr("Cannot connect to the AI service")), true),
        E::Io(io) => {
            let transient = matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
            );
            (DbError::connection(trf("Connection to the AI service failed: {}", &[&io])), transient)
        }
        E::Tls(m) => (DbError::tls(trf("TLS error talking to the AI service: {}", &[&m])), false),
        E::Rustls(r) => (DbError::tls(trf("TLS error talking to the AI service: {}", &[&r])), false),
        E::BadUri(u) => (DbError::config(trf("Invalid AI service URL: {}", &[&u])), false),
        other => (DbError::connection(trf("AI request failed: {}", &[&other])), false),
    }
}

fn send(agent: &ureq::Agent, r: &HttpRequest) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let mut b = agent.post(&r.url);
    for (k, v) in &r.headers {
        if k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        b = b.header(k.as_str(), v.as_str());
    }
    b.content_type("application/json").send(r.body.to_string())
}

enum Streamed {
    Done(StreamState),
    /// Error event; `emitted` tells whether text was already delivered.
    Error {
        err: ApiError,
        emitted: bool,
    },
}

fn read_stream(provider: AiProviderKind, resp: ureq::http::Response<ureq::Body>, on_delta: &mut dyn FnMut(&str), cancel: &CancellationToken) -> DbResult<Streamed> {
    let mut reader = BufReader::new(resp.into_body().into_reader());
    let mut parser = SseParser::default();
    let mut state = StreamState::default();
    let mut emitted = false;
    let mut line = Vec::with_capacity(1024);
    let mut handle = |ev, state: &mut StreamState, emitted: &mut bool| -> Control {
        let mut cb = |t: &str| {
            *emitted = true;
            on_delta(t);
        };
        match provider {
            AiProviderKind::Anthropic => state.on_anthropic_event(&ev, &mut cb),
            AiProviderKind::OpenAiCompatible => state.on_openai_event(&ev, &mut cb),
        }
    };
    loop {
        if cancel.is_cancelled() {
            return Err(DbError::cancelled());
        }
        line.clear();
        let n = reader.read_until(b'\n', &mut line).map_err(|e| DbError::connection(trf("Reading the AI response failed: {}", &[&e])))?;
        let ev = if n == 0 { parser.finish() } else { parser.push_line(&String::from_utf8_lossy(&line)) };
        if let Some(ev) = ev {
            match handle(ev, &mut state, &mut emitted) {
                Control::Continue => {}
                Control::Done => return Ok(Streamed::Done(state)),
                Control::Error(err) => return Ok(Streamed::Error { err, emitted }),
            }
        }
        if n == 0 {
            return Ok(Streamed::Done(state));
        }
    }
}

fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

fn read_error_body(resp: ureq::http::Response<ureq::Body>) -> String {
    let mut buf = Vec::new();
    let _ = resp.into_body().into_reader().take(64 * 1024).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

/// The request loop: retries transient failures, drops rejected optional
/// features once, streams the answer.
fn run_blocking(cfg: &AiConfig, agent: &ureq::Agent, req: &ChatRequest, initial: Flags, on_delta: &mut dyn FnMut(&str), cancel: &CancellationToken) -> DbResult<ChatResponse> {
    let mut flags = initial;
    let (mut fallback_dropped, mut effort_dropped, mut mct_switched) = (false, false, false);
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(DbError::cancelled());
        }
        let request = build_request(cfg, req, flags)?;
        tracing::debug!(url = %request.url, model = %cfg.model, attempt, "AI request");
        let resp = match send(agent, &request) {
            Ok(r) => r,
            Err(e) => {
                let (err, transient) = transport_error(e);
                if transient && attempt < cfg.retry.max_retries {
                    wait(backoff(cfg, attempt, None), cancel)?;
                    attempt += 1;
                    continue;
                }
                return Err(err);
            }
        };
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let retry_after = header(&resp, "retry-after").and_then(|v| parse_retry_after(&v));
            let request_id = header(&resp, "request-id").or_else(|| header(&resp, "x-request-id"));
            let body = read_error_body(resp);
            let err = ApiError::parse(cfg.provider, Some(status), &body, request_id, retry_after);
            if flags.fallback && !fallback_dropped && err.about_fallback() {
                tracing::info!("the AI service rejected the fallback beta; retrying without it");
                flags.fallback = false;
                fallback_dropped = true;
                continue;
            }
            if flags.effort && !effort_dropped && err.about_effort() {
                tracing::info!("the AI service rejected the effort setting; retrying without it");
                flags.effort = false;
                effort_dropped = true;
                continue;
            }
            if cfg.provider == AiProviderKind::OpenAiCompatible && !mct_switched && err.about_max_completion_tokens() {
                flags.max_completion_tokens = true;
                mct_switched = true;
                continue;
            }
            if err.retryable() && attempt < cfg.retry.max_retries {
                wait(backoff(cfg, attempt, err.retry_after), cancel)?;
                attempt += 1;
                continue;
            }
            return Err(err.to_db_error());
        }
        match read_stream(cfg.provider, resp, on_delta, cancel)? {
            Streamed::Done(state) => {
                let r = state.finish(cfg.provider, &cfg.model)?;
                tracing::debug!(model = %r.model, input = r.usage.input_tokens, output = r.usage.output_tokens, "AI response");
                return Ok(r);
            }
            Streamed::Error { err, emitted } => {
                if !emitted && err.retryable() && attempt < cfg.retry.max_retries {
                    wait(backoff(cfg, attempt, None), cancel)?;
                    attempt += 1;
                    continue;
                }
                return Err(err.to_db_error());
            }
        }
    }
}

#[cfg(test)]
mod tests;
