//! Full client path against an in-process HTTP server on 127.0.0.1 that
//! records requests and writes canned (optionally delayed) responses.

use super::*;
use crate::config::RetryPolicy;
use crate::types::ChatMessage;
use dbiji_core::ErrorKind;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    /// (delay before the chunk in ms, chunk)
    chunks: Vec<(u64, String)>,
}

impl Reply {
    fn sse(body: &str) -> Reply {
        Reply { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())], chunks: vec![(0, body.to_string())] }
    }
    fn json(status: u16, body: &str) -> Reply {
        Reply { status, headers: vec![("content-type".into(), "application/json".into())], chunks: vec![(0, body.to_string())] }
    }
    fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }
}

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("JSON request body")
    }
}

struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn mock(replies: Vec<Reply>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let head_end = loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find(&buf, b"\r\n\r\n") {
                    break Some(p);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let mut lines = head.split("\r\n");
            let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_string();
            let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
            let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
            let mut body = buf[head_end + 4..].to_vec();
            while body.len() < len {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            log.lock().push(Seen { path, headers, body: String::from_utf8_lossy(&body).to_string() });
            let mut out = format!("HTTP/1.1 {} Mock\r\n", reply.status);
            for (k, v) in &reply.headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("transfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            if sock.write_all(out.as_bytes()).await.is_err() {
                continue;
            }
            for (delay, chunk) in &reply.chunks {
                if *delay > 0 {
                    tokio::time::sleep(Duration::from_millis(*delay)).await;
                }
                if chunk.is_empty() {
                    continue;
                }
                let framed = format!("{:x}\r\n{chunk}\r\n", chunk.len());
                if sock.write_all(framed.as_bytes()).await.is_err() {
                    break;
                }
                let _ = sock.flush().await;
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.shutdown().await;
        }
    });
    Mock { base, seen }
}

fn anthropic_cfg(base: &str) -> AiConfig {
    AiConfig {
        base_url: Some(base.to_string()),
        api_key: Some("sk-ant-test-key".into()),
        retry: RetryPolicy { max_retries: 2, base_delay_ms: 10, max_delay_ms: 2_000 },
        timeout_secs: 20,
        ..Default::default()
    }
}

fn request() -> ChatRequest {
    ChatRequest::new("system prompt with schema", vec![ChatMessage::user("How many users?")])
}

fn text_stream(model: &str, parts: &[&str], stop: &str) -> String {
    let mut s = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"model\":\"{model}\",\"usage\":{{\"input_tokens\":100,\"cache_read_input_tokens\":80,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: ping\ndata: {{\"type\":\"ping\"}}\n\n"
    );
    for p in parts {
        s.push_str(&format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n",
            serde_json::to_string(p).unwrap()
        ));
    }
    s.push_str(&format!(
        "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\"}},\"usage\":{{\"output_tokens\":12}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    ));
    s
}

fn collector() -> (Arc<Mutex<Vec<String>>>, impl FnMut(&str) + Send + 'static) {
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let d = deltas.clone();
    (deltas, move |t: &str| d.lock().push(t.to_string()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_streaming_end_to_end() {
    let m = mock(vec![Reply::sse(&text_stream("claude-opus-5-5", &["There are ", "42 users."], "end_turn"))]).await;
    let client = AiClient::new(anthropic_cfg(&m.base)).unwrap();
    let (deltas, cb) = collector();
    let r = client.chat_stream(request(), cb, CancellationToken::new()).await.unwrap();
    assert_eq!(r.text, "There are 42 users.");
    assert_eq!(*deltas.lock(), vec!["There are ", "42 users."]);
    assert_eq!(r.model, "claude-opus-5-5");
    assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!((r.usage.input_tokens, r.usage.output_tokens, r.usage.cache_read_input_tokens), (100, 12, 80));
    let seen = m.seen.lock().clone();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert_eq!(s.path, "/v1/messages");
    assert_eq!(s.header("x-api-key"), Some("sk-ant-test-key"));
    assert_eq!(s.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(s.header("content-type"), Some("application/json"));
    assert!(s.header("anthropic-beta").is_none(), "not the first-party API");
    let body = s.json();
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 16000);
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["output_config"]["effort"], "medium");
    assert_eq!(body["messages"][0]["content"], "How many users?");
    assert!(body.get("fallbacks").is_none() && body.get("thinking").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_rate_limits_honouring_retry_after() {
    let error = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"},"request_id":"req_1"}"#;
    let m = mock(vec![
        Reply::json(429, error).header("retry-after", "0.3"),
        Reply::json(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#),
        Reply::sse(&text_stream("claude-opus-5-5", &["ok"], "end_turn")),
    ])
    .await;
    let client = AiClient::new(anthropic_cfg(&m.base)).unwrap();
    let start = Instant::now();
    let r = client.chat(request(), CancellationToken::new()).await.unwrap();
    assert_eq!(r.text, "ok");
    assert!(start.elapsed() >= Duration::from_millis(300), "{:?}", start.elapsed());
    assert_eq!(m.seen.lock().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gives_up_after_two_retries_and_maps_errors() {
    let busy = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_9"}"#;
    let m = mock(vec![Reply::json(529, busy), Reply::json(529, busy), Reply::json(529, busy), Reply::json(529, busy)]).await;
    let client = AiClient::new(anthropic_cfg(&m.base)).unwrap();
    let e = client.chat(request(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Connection);
    assert_eq!(e.code.as_deref(), Some("overloaded_error"));
    assert_eq!(e.detail.as_deref(), Some("Request id: req_9"));
    assert_eq!(m.seen.lock().len(), 3, "one request plus two retries");

    let auth = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let m = mock(vec![Reply::json(401, auth).header("request-id", "req_hdr")]).await;
    let e = AiClient::new(anthropic_cfg(&m.base)).unwrap().chat(request(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert_eq!(e.detail.as_deref(), Some("Request id: req_hdr"));
    assert!(!e.message.contains("sk-ant-test-key"));
    assert_eq!(m.seen.lock().len(), 1, "not retried");

    let missing = r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-nope"}}"#;
    let m = mock(vec![Reply::json(404, missing)]).await;
    let mut cfg = anthropic_cfg(&m.base);
    cfg.model = "claude-nope".into();
    let e = AiClient::new(cfg).unwrap().chat(request(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    assert!(e.message.contains("claude-nope"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_fallback_beta_is_retried_without_it() {
    let rejected =
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `server-side-fallback-2026-07-01` for the `anthropic-beta` header."}}"#;
    let m = mock(vec![Reply::json(400, rejected), Reply::sse(&text_stream("claude-opus-5-5", &["fine"], "end_turn"))]).await;
    let cfg = anthropic_cfg(&m.base);
    let agent = agent_for(&cfg);
    let flags = Flags { fallback: true, effort: true, max_completion_tokens: false };
    let mut deltas = Vec::new();
    let r = run_blocking(&cfg, &agent, &request(), flags, &mut |t: &str| deltas.push(t.to_string()), &CancellationToken::new()).unwrap();
    assert_eq!(r.text, "fine");
    let seen = m.seen.lock().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].header("anthropic-beta"), Some("server-side-fallback-2026-07-01"));
    assert_eq!(seen[0].json()["fallbacks"], "default");
    assert!(seen[1].header("anthropic-beta").is_none());
    assert!(seen[1].json().get("fallbacks").is_none());
    assert_eq!(seen[1].json()["output_config"]["effort"], "medium", "effort is kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_effort_is_retried_without_it() {
    let rejected = r#"{"type":"error","error":{"type":"invalid_request_error","message":"output_config.effort: Extra inputs are not permitted"}}"#;
    let m = mock(vec![Reply::json(400, rejected), Reply::sse(&text_stream("claude-haiku-4-5", &["x"], "end_turn"))]).await;
    let mut cfg = anthropic_cfg(&m.base);
    cfg.model = "claude-haiku-4-5".into();
    let r = AiClient::new(cfg).unwrap().chat(request(), CancellationToken::new()).await.unwrap();
    assert_eq!(r.model, "claude-haiku-4-5");
    let seen = m.seen.lock().clone();
    assert!(seen[0].json().get("output_config").is_some());
    assert!(seen[1].json().get("output_config").is_none());
    // any other 400 is final
    let m = mock(vec![Reply::json(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: roles must alternate"}}"#)]).await;
    let e = AiClient::new(anthropic_cfg(&m.base)).unwrap().chat(request(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    assert_eq!(m.seen.lock().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusal_truncation_and_mid_stream_errors() {
    let refusal = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":9}}}\n\n\
                   event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"type\":\"refusal\",\"category\":\"bio\"}}}\n\n\
                   event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let m = mock(vec![Reply::sse(refusal)]).await;
    let e = AiClient::new(anthropic_cfg(&m.base)).unwrap().chat(request(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("refusal"));
    assert!(e.message.contains("declined") && e.message.contains("bio"));

    let m = mock(vec![Reply::sse(&text_stream("claude-opus-5-5", &["partial"], "max_tokens"))]).await;
    let r = AiClient::new(anthropic_cfg(&m.base)).unwrap().chat(request(), CancellationToken::new()).await.unwrap();
    assert!(r.truncated);

    // an overloaded error before any text is retried
    let early = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{}}}\n\n\
                 event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let m = mock(vec![Reply::sse(early), Reply::sse(&text_stream("claude-opus-5-5", &["second try"], "end_turn"))]).await;
    let r = AiClient::new(anthropic_cfg(&m.base)).unwrap().chat(request(), CancellationToken::new()).await.unwrap();
    assert_eq!(r.text, "second try");
    assert_eq!(m.seen.lock().len(), 2);

    // after text was delivered the error is final
    let late = text_stream("claude-opus-5-5", &["half"], "end_turn").split("event: content_block_stop").next().unwrap().to_string()
        + "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Internal\"}}\n\n";
    let m = mock(vec![Reply::sse(&late), Reply::sse(&text_stream("claude-opus-5-5", &["unused"], "end_turn"))]).await;
    let (deltas, cb) = collector();
    let e = AiClient::new(anthropic_cfg(&m.base)).unwrap().chat_stream(request(), cb, CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.code.as_deref(), Some("api_error"));
    assert_eq!(*deltas.lock(), vec!["half"]);
    assert_eq!(m.seen.lock().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_compatible_end_to_end() {
    let stream = "data: {\"model\":\"qwen2.5-coder:7b\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
                  data: {\"model\":\"qwen2.5-coder:7b\",\"choices\":[{\"delta\":{\"content\":\"SELECT \"}}]}\n\n\
                  data: {\"model\":\"qwen2.5-coder:7b\",\"choices\":[{\"delta\":{\"content\":\"1\"},\"finish_reason\":\"stop\"}]}\n\n\
                  data: [DONE]\n\n";
    let m = mock(vec![Reply::sse(stream)]).await;
    let cfg = AiConfig::openai_compatible(m.base.clone(), "qwen2.5-coder:7b", None);
    let (deltas, cb) = collector();
    let r = AiClient::new(cfg).unwrap().chat_stream(request(), cb, CancellationToken::new()).await.unwrap();
    assert_eq!(r.text, "SELECT 1");
    assert_eq!(*deltas.lock(), vec!["SELECT ", "1"]);
    let s = m.seen.lock()[0].clone();
    assert_eq!(s.path, "/v1/chat/completions");
    assert!(s.header("authorization").is_none(), "no key for a local server");
    let body = s.json();
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["max_tokens"], 16000);
    assert!(body.get("output_config").is_none());

    // a key is sent as bearer token; max_tokens is switched when rejected
    let reject = r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","type":"invalid_request_error","code":"unsupported_parameter"}}"#;
    let m = mock(vec![Reply::json(400, reject), Reply::sse(stream)]).await;
    let cfg = AiConfig::openai_compatible(m.base.clone(), "o-model", Some("sk-openai".into()));
    let r = AiClient::new(cfg).unwrap().chat(request(), CancellationToken::new()).await.unwrap();
    assert_eq!(r.text, "SELECT 1");
    let seen = m.seen.lock().clone();
    assert_eq!(seen[0].header("authorization"), Some("Bearer sk-openai"));
    assert!(seen[1].json().get("max_tokens").is_none());
    assert_eq!(seen[1].json()["max_completion_tokens"], 16000);

    let m = mock(vec![Reply::json(401, r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#)]).await;
    let e = AiClient::new(AiConfig::openai_compatible(m.base.clone(), "m", Some("bad".into()))).unwrap().chat(request(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("Incorrect API key"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_returns_promptly() {
    let full = text_stream("claude-opus-5-5", &["first", "second"], "end_turn");
    let split = full.find("event: content_block_delta").unwrap() + full[full.find("event: content_block_delta").unwrap()..].find("\n\n").unwrap() + 2;
    let (head, tail) = full.split_at(split);
    let reply = Reply { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())], chunks: vec![(0, head.to_string()), (5_000, tail.to_string())] };
    let m = mock(vec![reply]).await;
    let client = AiClient::new(anthropic_cfg(&m.base)).unwrap();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let start = Instant::now();
    let e = client
        .chat_stream(
            request(),
            move |t: &str| {
                s.lock().push(t.to_string());
                c.cancel();
            },
            cancel,
        )
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
    assert_eq!(*seen.lock(), vec!["first"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_errors_and_local_validation() {
    // a port with nothing listening
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut cfg = anthropic_cfg(&format!("http://127.0.0.1:{port}"));
    cfg.retry.max_retries = 0;
    let e = AiClient::new(cfg).unwrap().chat(request(), CancellationToken::new()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Connection, "{e}");
    // invalid conversations are rejected before any request
    let client = AiClient::new(anthropic_cfg("http://127.0.0.1:9")).unwrap();
    let prefill = ChatRequest::new("s", vec![ChatMessage::user("q"), ChatMessage::assistant("SELECT")]);
    assert_eq!(client.chat(prefill, CancellationToken::new()).await.unwrap_err().kind, ErrorKind::Config);
    assert!(AiClient::new(AiConfig::default()).is_err(), "first-party API needs a key");
    let dbg = format!("{client:?}");
    assert!(!dbg.contains("sk-ant-test-key"));
}
