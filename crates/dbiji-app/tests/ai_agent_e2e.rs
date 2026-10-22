//! The AI assistant's queries on real servers (DBIJI_TEST_PG,
//! DBIJI_TEST_COCKROACH, DBIJI_TEST_MYSQL, DBIJI_TEST_MARIADB): they read, and
//! their session cannot write even what the statement check would let through.

use dbiji_ai::ToolCall;
use dbiji_app::conn::session::SessionHandle;
use dbiji_app::tabs::ai_agent::{Agent, TOOL};
use dbiji_core::driver::ConnectParams;
use dbiji_core::{DbResult, Secret};

// The type `Agent` takes for its session.
#[allow(clippy::result_large_err)]
async fn open(url: String) -> DbResult<SessionHandle> {
    let (cfg, password) = dbiji_config::parse_connection_url(&url).expect("valid test URL");
    let registry = dbiji_app::driver_registry();
    let password = password.map(|p| Secret::new(p.expose().to_string()));
    let s = registry.get(cfg.kind).unwrap().connect(&ConnectParams::from_config(&cfg, password)).await?;
    Ok(SessionHandle::new(s, "ai", cfg.database.clone()))
}

fn call(sql: &str) -> ToolCall {
    ToolCall { id: "c".into(), name: TOOL.into(), arguments: serde_json::json!({ "sql": sql }).to_string() }
}

#[tokio::test]
async fn the_assistants_session_only_reads() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    for var in ["DBIJI_TEST_PG", "DBIJI_TEST_COCKROACH", "DBIJI_TEST_MYSQL", "DBIJI_TEST_MARIADB"] {
        let Some(url) = std::env::var(var).ok().filter(|u| !u.trim().is_empty()) else { continue };
        let kind = dbiji_config::parse_connection_url(&url).unwrap().0.kind;
        let syntax = dbiji_app::driver_registry().dialect(kind).syntax().clone();
        let mut agent = Agent::new(kind, syntax, None, Box::pin(open(url.clone())));
        let step = agent.answer(&call("SELECT 1 + 1 AS two")).await;
        assert!(step.ok, "[{var}] {}", step.summary);
        assert_eq!(step.rows, vec![vec!["2".to_string()]], "[{var}]");
        assert!(!agent.answer(&call("CREATE TABLE ai_agent_probe (id int)")).await.ok, "[{var}] refused");
        // Past the check, the server refuses: the session is read-only.
        let session = agent.session().await.unwrap();
        let err = session.execute_update("CREATE TABLE ai_agent_probe (id int)", &[]).await.expect_err(var);
        assert!(err.full_text().to_lowercase().contains("read"), "[{var}] {}", err.full_text());
    }
}
