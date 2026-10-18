//! End-to-end tests: local forwarder -> (proxy) -> in-process SSH server(s) -> echo server.

mod common;

use common::*;
use dbiji_core::config::{HostKeyPolicy, ProxyConfig, ProxyKind, SshAuth, SshConfig};
use dbiji_core::{ErrorKind, Secret};
use dbiji_net::{Hop, Tunnel, TunnelSpec, open_tunnel};
use russh::keys::ssh_key::{Algorithm, EcdsaCurve, LineEnding, PrivateKey};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USER: &str = "deploy";
const PASSWORD: &str = "correct-horse-battery";

fn ssh_cfg(port: u16, auth: SshAuth) -> SshConfig {
    SshConfig {
        enabled: true,
        host: "127.0.0.1".into(),
        port,
        user: USER.into(),
        auth,
        keepalive_secs: 0,
        connect_timeout_secs: 10,
        host_key_policy: HostKeyPolicy::AcceptNew,
        ..Default::default()
    }
}

fn spec(target_port: u16, kh: &Path) -> TunnelSpec {
    TunnelSpec::new("127.0.0.1", target_port, kh).with_system_known_hosts(false)
}

fn known_hosts(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("known_hosts")
}

async fn password_tunnel(server: &TestSshServer, echo: &EchoServer, kh: &Path) -> Tunnel {
    let s = spec(echo.port(), kh).with_ssh(
        ssh_cfg(server.port(), SshAuth::Password),
        Some(Secret::new(PASSWORD)),
        None,
    );
    open_tunnel(s).await.expect("tunnel")
}

fn write_key(dir: &Path, name: &str, pem: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, pem).unwrap();
    p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn password_tunnel_shares_one_session_for_concurrent_connections() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;
    let tunnel = password_tunnel(&server, &echo, &known_hosts(&dir)).await;
    assert!(tunnel.is_alive());
    assert!(tunnel.ssh_connected());
    assert_eq!(tunnel.local_addr().ip().to_string(), "127.0.0.1");

    let port = tunnel.local_port();
    let mut tasks = Vec::new();
    for i in 0..8u8 {
        tasks.push(tokio::spawn(async move {
            // Large enough to exercise SSH window flow control.
            let payload: Vec<u8> = (0..300_000u32)
                .map(|n| (n as u8).wrapping_mul(31).wrapping_add(i))
                .collect();
            let back = echo_roundtrip(port, &payload).await.expect("roundtrip");
            assert_eq!(back, payload, "connection {i} corrupted data");
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(
        server.sessions(),
        1,
        "all forwarded connections must share one SSH session"
    );
    assert_eq!(server.channels(), 8);
    assert_eq!(
        echo.connections.load(std::sync::atomic::Ordering::SeqCst),
        8
    );
    let stats = tunnel.stats();
    assert_eq!(stats.total_connections, 8);
    assert_eq!(stats.failed_connections, 0);
    assert_eq!(stats.ssh_reconnects, 0);
    // The SSH server saw the real target.
    let targets = server.stats.targets.lock().unwrap().clone();
    assert!(
        targets
            .iter()
            .all(|(h, p)| h == "127.0.0.1" && *p == u32::from(echo.port()))
    );
    tunnel.close().await;
    assert!(!tunnel.is_alive());
    assert!(
        closed_without_data(port, Duration::from_secs(2)).await,
        "listener must be closed"
    );
}

#[tokio::test]
async fn wrong_password_lists_tried_methods() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;
    let s = spec(echo.port(), &known_hosts(&dir)).with_ssh(
        ssh_cfg(server.port(), SshAuth::Password),
        Some(Secret::new("nope-wrong")),
        None,
    );
    let err = open_tunnel(s).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(
        err.message
            .contains("SSH authentication failed for deploy@127.0.0.1"),
        "{}",
        err.message
    );
    assert!(err.message.contains("Tried: password"), "{}", err.message);
    assert!(
        err.message.contains("The server accepts:"),
        "{}",
        err.message
    );
    assert!(
        !err.full_text().contains("nope-wrong"),
        "password leaked into error"
    );

    // Missing password is reported before contacting the server's auth.
    let s = spec(echo.port(), &known_hosts(&dir)).with_ssh(
        ssh_cfg(server.port(), SshAuth::Password),
        None,
        None,
    );
    let err = open_tunnel(s).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(
        err.message.contains("password is required"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn keyboard_interactive_fallback_uses_password() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let mut opts = SshServerOptions::password(USER, PASSWORD);
    opts.keyboard_interactive_only = true;
    let server = TestSshServer::start(opts).await;
    let tunnel = password_tunnel(&server, &echo, &known_hosts(&dir)).await;
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"kbd").await.unwrap(),
        b"kbd"
    );
}

#[tokio::test]
async fn public_key_formats_and_passphrases() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;

    let ed = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let ec = PrivateKey::random(
        &mut rand::rng(),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
    )
    .unwrap();
    let rsa = russh::keys::decode_secret_key(RSA_TEST_KEY, None).unwrap();

    let mut opts = SshServerOptions::password(USER, "unused-password");
    opts.authorized_keys = vec![
        ed.public_key().clone(),
        ec.public_key().clone(),
        rsa.public_key().clone(),
    ];
    let server = TestSshServer::start(opts).await;

    let ed_plain = write_key(
        dir.path(),
        "id_ed25519",
        &ed.to_openssh(LineEnding::LF).unwrap(),
    );
    let ed_enc = write_key(
        dir.path(),
        "id_ed25519_enc",
        &ed.encrypt(&mut rand::rng(), "key pass")
            .unwrap()
            .to_openssh(LineEnding::LF)
            .unwrap(),
    );
    let mut pkcs8 = Vec::new();
    russh::keys::encode_pkcs8_pem(&ec, &mut pkcs8).unwrap();
    let ec_pkcs8 = write_key(
        dir.path(),
        "id_ecdsa.pem",
        std::str::from_utf8(&pkcs8).unwrap(),
    );
    let mut pkcs8_enc = Vec::new();
    russh::keys::encode_pkcs8_pem_encrypted(&ec, b"pem pass", 10, &mut pkcs8_enc).unwrap();
    let ec_pkcs8_enc = write_key(
        dir.path(),
        "id_ecdsa_enc.pem",
        std::str::from_utf8(&pkcs8_enc).unwrap(),
    );
    let rsa_path = write_key(dir.path(), "id_rsa", RSA_TEST_KEY);

    let cases: Vec<(String, Option<&str>)> = vec![
        (ed_plain, None),
        (ed_enc.clone(), Some("key pass")),
        (ec_pkcs8, None),
        (ec_pkcs8_enc, Some("pem pass")),
        (rsa_path, None),
    ];
    for (path, pass) in cases {
        let s = spec(echo.port(), &known_hosts(&dir)).with_ssh(
            ssh_cfg(
                server.port(),
                SshAuth::PublicKey {
                    key_path: path.clone(),
                },
            ),
            None,
            pass.map(Secret::new),
        );
        let tunnel = open_tunnel(s)
            .await
            .unwrap_or_else(|e| panic!("{path}: {e:?}"));
        assert_eq!(
            echo_roundtrip(tunnel.local_port(), path.as_bytes())
                .await
                .unwrap(),
            path.as_bytes()
        );
        tunnel.close().await;
    }

    // Wrong passphrase.
    let s = spec(echo.port(), &known_hosts(&dir)).with_ssh(
        ssh_cfg(
            server.port(),
            SshAuth::PublicKey {
                key_path: ed_enc.clone(),
            },
        ),
        None,
        Some(Secret::new("bad pass")),
    );
    let err = open_tunnel(s).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(err.message.contains("wrong passphrase"), "{}", err.message);

    // A key the server does not accept.
    let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let other_path = write_key(
        dir.path(),
        "other",
        &other.to_openssh(LineEnding::LF).unwrap(),
    );
    let s = spec(echo.port(), &known_hosts(&dir)).with_ssh(
        ssh_cfg(
            server.port(),
            SshAuth::PublicKey {
                key_path: other_path,
            },
        ),
        None,
        None,
    );
    let err = open_tunnel(s).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(err.message.contains("Tried: public key"), "{}", err.message);
    assert!(err.message.contains("ssh-ed25519"), "{}", err.message);
}

#[cfg(unix)]
#[tokio::test]
async fn ssh_agent_authentication() {
    use futures::StreamExt;
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let mut opts = SshServerOptions::password(USER, "unused");
    opts.authorized_keys = vec![key.public_key().clone()];
    let server = TestSshServer::start(opts).await;

    let sock = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let incoming = futures::stream::unfold(listener, |l| async move {
        let r = l.accept().await.map(|(s, _)| s);
        Some((r, l))
    })
    .boxed();
    let agent_task = tokio::spawn(russh::keys::agent::server::serve(incoming, ()));
    // An unrelated identity first, then the authorized one.
    let mut client = russh::keys::agent::client::AgentClient::connect_uds(&sock)
        .await
        .unwrap();
    let decoy = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    client.add_identity(&decoy, &[]).await.unwrap();
    client.add_identity(&key, &[]).await.unwrap();

    let s = spec(echo.port(), &known_hosts(&dir))
        .with_ssh(ssh_cfg(server.port(), SshAuth::Agent), None, None)
        .with_agent_socket(Some(sock.clone()));
    let tunnel = open_tunnel(s).await.expect("agent auth");
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"agent").await.unwrap(),
        b"agent"
    );
    tunnel.close().await;

    // Agent without the right key.
    let empty_sock = dir.path().join("missing.sock");
    let s = spec(echo.port(), &known_hosts(&dir))
        .with_ssh(ssh_cfg(server.port(), SshAuth::Agent), None, None)
        .with_agent_socket(Some(empty_sock));
    let err = open_tunnel(s).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(
        err.message.contains("Cannot connect to the SSH agent"),
        "{}",
        err.message
    );
    agent_task.abort();
}

#[tokio::test]
async fn host_key_policies() {
    let dir = tempfile::tempdir().unwrap();
    let kh = known_hosts(&dir);
    let echo = EchoServer::start().await;
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;
    let pw = || Some(Secret::new(PASSWORD));

    // Strict with an empty known_hosts: rejected, nothing written.
    let mut strict = ssh_cfg(server.port(), SshAuth::Password);
    strict.host_key_policy = HostKeyPolicy::Strict;
    let err = open_tunnel(spec(echo.port(), &kh).with_ssh(strict.clone(), pw(), None))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Ssh);
    assert!(err.message.contains("is not known"), "{}", err.message);
    assert!(err.message.contains("SHA256:"), "{}", err.message);
    assert!(!kh.exists());

    // AcceptNew: trust on first use and persist.
    let tunnel = open_tunnel(spec(echo.port(), &kh).with_ssh(
        ssh_cfg(server.port(), SshAuth::Password),
        pw(),
        None,
    ))
    .await
    .unwrap();
    tunnel.close().await;
    let content = std::fs::read_to_string(&kh).unwrap();
    assert!(
        content.contains(&format!("[127.0.0.1]:{} ssh-ed25519 ", server.port())),
        "{content}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&kh).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Now Strict accepts the recorded key.
    let tunnel = open_tunnel(spec(echo.port(), &kh).with_ssh(strict.clone(), pw(), None))
        .await
        .unwrap();
    tunnel.close().await;

    // Changed key on the same host:port is rejected by AcceptNew and Strict.
    let port = server.port();
    server.stop().await;
    let mut opts = SshServerOptions::password(USER, PASSWORD);
    opts.port = port;
    let impostor = TestSshServer::start(opts).await;
    for policy in [HostKeyPolicy::AcceptNew, HostKeyPolicy::Strict] {
        let mut cfg = ssh_cfg(port, SshAuth::Password);
        cfg.host_key_policy = policy;
        let err = open_tunnel(spec(echo.port(), &kh).with_ssh(cfg, pw(), None))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Ssh);
        assert!(err.message.contains("HOST KEY MISMATCH"), "{}", err.message);
        assert!(
            err.hint
                .as_deref()
                .unwrap_or("")
                .contains("remove the old entry"),
            "{:?}",
            err.hint
        );
    }
    assert_eq!(
        impostor
            .stats
            .authenticated
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no credentials sent to impostor"
    );

    // AcceptAll connects anyway (insecure, explicit opt-in).
    let mut cfg = ssh_cfg(port, SshAuth::Password);
    cfg.host_key_policy = HostKeyPolicy::AcceptAll;
    let tunnel = open_tunnel(spec(echo.port(), &kh).with_ssh(cfg, pw(), None))
        .await
        .unwrap();
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"x").await.unwrap(),
        b"x"
    );
}

#[tokio::test]
async fn channel_open_failure_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;
    let closed_port = refused_port();
    let s = spec(closed_port, &known_hosts(&dir)).with_ssh(
        ssh_cfg(server.port(), SshAuth::Password),
        Some(Secret::new(PASSWORD)),
        None,
    );
    let tunnel = open_tunnel(s).await.unwrap();
    assert!(closed_without_data(tunnel.local_port(), Duration::from_secs(5)).await);
    // The failure is recorded asynchronously by the forwarding task.
    let mut err = None;
    for _ in 0..50 {
        err = tunnel.last_error();
        if err.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = err.expect("last error recorded");
    assert_eq!(err.kind, ErrorKind::Connection);
    assert!(
        err.message.contains("could not open a connection"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains(&format!("127.0.0.1:{closed_port}")),
        "{}",
        err.message
    );
    assert_eq!(tunnel.stats().failed_connections, 1);
    // The session itself is still fine.
    assert!(tunnel.ssh_connected());
}

#[tokio::test]
async fn unreachable_ssh_host() {
    let dir = tempfile::tempdir().unwrap();
    let port = refused_port();
    let s = spec(5432, &known_hosts(&dir)).with_ssh(
        ssh_cfg(port, SshAuth::Password),
        Some(Secret::new("x")),
        None,
    );
    let err = open_tunnel(s).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Connection);
    assert!(
        err.message.contains("Cannot reach the SSH server"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains(&format!("127.0.0.1:{port}")),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn reconnects_after_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let opts = SshServerOptions::password(USER, PASSWORD);
    let host_key = opts.host_key.clone();
    let server = TestSshServer::start(opts).await;
    let port = server.port();
    let tunnel = password_tunnel(&server, &echo, &known_hosts(&dir)).await;
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"before")
            .await
            .unwrap(),
        b"before"
    );

    server.stop().await;
    let mut opts = SshServerOptions::password(USER, PASSWORD);
    opts.port = port;
    opts.host_key = host_key;
    let restarted = TestSshServer::start(opts).await;

    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"after").await.unwrap(),
        b"after"
    );
    assert_eq!(restarted.sessions(), 1);
    assert_eq!(tunnel.stats().ssh_reconnects, 1);
    assert!(tunnel.ssh_connected());
}

#[tokio::test]
async fn keepalive_keeps_idle_session_open() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let mut opts = SshServerOptions::password(USER, PASSWORD);
    opts.inactivity_timeout = Some(Duration::from_millis(2500));
    let server = TestSshServer::start(opts).await;

    let mut with_keepalive = ssh_cfg(server.port(), SshAuth::Password);
    with_keepalive.keepalive_secs = 1;
    let t1 = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_ssh(
        with_keepalive,
        Some(Secret::new(PASSWORD)),
        None,
    ))
    .await
    .unwrap();
    let t2 = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_ssh(
        ssh_cfg(server.port(), SshAuth::Password),
        Some(Secret::new(PASSWORD)),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(server.sessions(), 2);
    tokio::time::sleep(Duration::from_millis(4500)).await;

    // The idle session without keepalive was dropped by the server and is re-established transparently.
    assert!(!t2.ssh_connected());
    assert_eq!(echo_roundtrip(t2.local_port(), b"t2").await.unwrap(), b"t2");
    assert_eq!(t2.stats().ssh_reconnects, 1);

    // The keepalive session survived.
    assert!(t1.ssh_connected());
    assert_eq!(echo_roundtrip(t1.local_port(), b"t1").await.unwrap(), b"t1");
    assert_eq!(t1.stats().ssh_reconnects, 0);
    assert_eq!(server.sessions(), 3);
}

#[tokio::test]
async fn ssh_through_socks5_and_http_proxies() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;

    let socks = MockProxy::socks5(Some(("pu", "pp"))).await;
    let proxy = ProxyConfig {
        enabled: true,
        kind: ProxyKind::Socks5,
        host: "127.0.0.1".into(),
        port: socks.port(),
        user: Some("pu".into()),
        save_credentials: true,
    };
    let s = spec(echo.port(), &known_hosts(&dir))
        .with_proxy(proxy, Some(Secret::new("pp")))
        .with_ssh(
            ssh_cfg(server.port(), SshAuth::Password),
            Some(Secret::new(PASSWORD)),
            None,
        );
    let tunnel = open_tunnel(s).await.unwrap();
    for _ in 0..3 {
        assert_eq!(
            echo_roundtrip(tunnel.local_port(), b"via socks")
                .await
                .unwrap(),
            b"via socks"
        );
    }
    // The SSH session is made once through the proxy and then reused.
    assert_eq!(
        socks.connections.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(tunnel.description().contains("SOCKS5 proxy"));
    tunnel.close().await;

    let http = MockProxy::http(None).await;
    let proxy = ProxyConfig {
        enabled: true,
        kind: ProxyKind::Http,
        host: "127.0.0.1".into(),
        port: http.port(),
        user: None,
        save_credentials: true,
    };
    let s = spec(echo.port(), &known_hosts(&dir))
        .with_proxy(proxy, None)
        .with_ssh(
            ssh_cfg(server.port(), SshAuth::Password),
            Some(Secret::new(PASSWORD)),
            None,
        );
    let tunnel = open_tunnel(s).await.unwrap();
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"via http")
            .await
            .unwrap(),
        b"via http"
    );
}

#[tokio::test]
async fn jump_host_chain() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let jump = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;
    let inner = TestSshServer::start(SshServerOptions::password("inner", "inner-pw")).await;
    let mut inner_cfg = ssh_cfg(inner.port(), SshAuth::Password);
    inner_cfg.user = "inner".into();
    let s = spec(echo.port(), &known_hosts(&dir))
        .with_ssh(
            ssh_cfg(jump.port(), SshAuth::Password),
            Some(Secret::new(PASSWORD)),
            None,
        )
        .with_ssh(inner_cfg, Some(Secret::new("inner-pw")), None);
    let tunnel = open_tunnel(s).await.unwrap();
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"jump").await.unwrap(),
        b"jump"
    );
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"jump2").await.unwrap(),
        b"jump2"
    );
    // The jump host carried exactly one channel: the inner SSH connection.
    assert_eq!(jump.channels(), 1);
    assert_eq!(inner.channels(), 2);
    let targets = jump.stats.targets.lock().unwrap().clone();
    assert_eq!(
        targets,
        vec![("127.0.0.1".to_string(), u32::from(inner.port()))]
    );
}

#[tokio::test]
async fn proxy_only_tunnels() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let socks = MockProxy::socks5(Some(("alice", "s3cret"))).await;
    let cfg = |port, kind, user: Option<&str>| ProxyConfig {
        enabled: true,
        kind,
        host: "127.0.0.1".into(),
        port,
        user: user.map(str::to_string),
        save_credentials: true,
    };

    let t = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_proxy(
        cfg(socks.port(), ProxyKind::Socks5, Some("alice")),
        Some(Secret::new("s3cret")),
    ))
    .await
    .unwrap();
    assert_eq!(
        echo_roundtrip(t.local_port(), b"socks").await.unwrap(),
        b"socks"
    );
    assert_eq!(
        echo_roundtrip(t.local_port(), b"socks2").await.unwrap(),
        b"socks2"
    );
    t.close().await;

    // Wrong SOCKS password is detected while opening (auth probe).
    let err = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_proxy(
        cfg(socks.port(), ProxyKind::Socks5, Some("alice")),
        Some(Secret::new("bad")),
    ))
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(!err.full_text().contains("bad\""));

    // HTTP CONNECT with Basic auth.
    let http = MockProxy::http(Some(("bob", "hunter2"))).await;
    let t = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_proxy(
        cfg(http.port(), ProxyKind::Http, Some("bob")),
        Some(Secret::new("hunter2")),
    ))
    .await
    .unwrap();
    assert_eq!(
        echo_roundtrip(t.local_port(), b"http").await.unwrap(),
        b"http"
    );
    t.close().await;

    // Wrong HTTP credentials: reported per connection through last_error.
    let t = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_proxy(
        cfg(http.port(), ProxyKind::Http, Some("bob")),
        Some(Secret::new("nope")),
    ))
    .await
    .unwrap();
    assert!(closed_without_data(t.local_port(), Duration::from_secs(5)).await);
    let mut err = None;
    for _ in 0..50 {
        err = t.last_error();
        if err.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(err.expect("error").kind, ErrorKind::Auth);
}

#[tokio::test]
async fn local_port_handling_and_validation() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;

    // Another process may grab a "free" port in between; try a few.
    let mut opened = None;
    for _ in 0..5 {
        let fixed = free_port();
        let mut cfg = ssh_cfg(server.port(), SshAuth::Password);
        cfg.local_port = Some(fixed);
        let spec = spec(echo.port(), &known_hosts(&dir)).with_ssh(
            cfg.clone(),
            Some(Secret::new(PASSWORD)),
            None,
        );
        if let Ok(t) = open_tunnel(spec).await {
            opened = Some((t, fixed, cfg));
            break;
        }
    }
    let (t, fixed, cfg) = opened.expect("a free fixed port");
    assert_eq!(t.local_port(), fixed);

    // Same port again: already in use.
    let err = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_ssh(
        cfg,
        Some(Secret::new(PASSWORD)),
        None,
    ))
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);
    assert!(err.message.contains("already in use"), "{}", err.message);
    drop(t);
    // Re-opening on the same fixed port right after dropping works (reconnect flow).
    let t2 = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_ssh(
        {
            let mut c = ssh_cfg(server.port(), SshAuth::Password);
            c.local_port = Some(fixed);
            c
        },
        Some(Secret::new(PASSWORD)),
        None,
    ))
    .await
    .unwrap();
    assert_eq!(t2.local_port(), fixed);
    assert_eq!(echo_roundtrip(fixed, b"again").await.unwrap(), b"again");
    drop(t2);

    // No enabled hop.
    let mut disabled = ssh_cfg(server.port(), SshAuth::Password);
    disabled.enabled = false;
    let err = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_ssh(disabled, None, None))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);

    // Missing user.
    let mut nouser = ssh_cfg(server.port(), SshAuth::Password);
    nouser.user = String::new();
    let err = open_tunnel(spec(echo.port(), &known_hosts(&dir)).with_ssh(nouser, None, None))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);

    assert!(dbiji_net::needs_tunnel(
        Some(&ssh_cfg(1, SshAuth::Password)),
        None
    ));
    assert!(!dbiji_net::needs_tunnel(
        None,
        Some(&ProxyConfig::default())
    ));
    let h = Hop::Proxy {
        cfg: ProxyConfig {
            host: "p".into(),
            port: 1,
            ..Default::default()
        },
        password: Some(Secret::new("x")),
    };
    assert!(!format!("{h:?}").contains("\"x\""));
}

#[tokio::test]
async fn dropping_tunnel_stops_listener() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;
    let tunnel = password_tunnel(&server, &echo, &known_hosts(&dir)).await;
    let port = tunnel.local_port();
    assert_eq!(echo_roundtrip(port, b"alive").await.unwrap(), b"alive");
    drop(tunnel);
    let mut refused = false;
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "listener still accepting after drop");
}

#[tokio::test]
async fn black_holed_session_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let server = TestSshServer::start(SshServerOptions::password(USER, PASSWORD)).await;
    let relay = FreezableRelay::start(server.addr).await;
    let mut cfg = ssh_cfg(relay.port(), SshAuth::Password);
    cfg.connect_timeout_secs = 1;
    let s = spec(echo.port(), &known_hosts(&dir)).with_ssh(cfg, Some(Secret::new(PASSWORD)), None);
    let tunnel = open_tunnel(s).await.unwrap();
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"one").await.unwrap(),
        b"one"
    );

    // The network path of the session dies silently: the next connection
    // times out opening its channel, the probe fails, and a new session is used.
    relay.freeze_existing();
    assert!(tunnel.ssh_connected(), "nothing noticed yet");
    let back = tokio::time::timeout(
        Duration::from_secs(15),
        echo_roundtrip(tunnel.local_port(), b"two"),
    )
    .await
    .expect("no hang")
    .unwrap();
    assert_eq!(back, b"two");
    assert_eq!(tunnel.stats().ssh_reconnects, 1);

    // ensure_connected() also detects an unresponsive session.
    relay.freeze_existing();
    tokio::time::timeout(Duration::from_secs(15), tunnel.ensure_connected())
        .await
        .expect("no hang")
        .unwrap();
    assert_eq!(tunnel.stats().ssh_reconnects, 2);
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"three").await.unwrap(),
        b"three"
    );
    // A healthy session is kept.
    tunnel.ensure_connected().await.unwrap();
    assert_eq!(tunnel.stats().ssh_reconnects, 2);
}

#[tokio::test]
async fn ensure_connected_after_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let echo = EchoServer::start().await;
    let opts = SshServerOptions::password(USER, PASSWORD);
    let host_key = opts.host_key.clone();
    let server = TestSshServer::start(opts).await;
    let port = server.port();
    let tunnel = password_tunnel(&server, &echo, &known_hosts(&dir)).await;
    server.stop().await;
    // While the server is down the error is reported directly.
    let err = tunnel.ensure_connected().await.unwrap_err();
    assert!(
        err.message.contains("Cannot reach the SSH server"),
        "{}",
        err.message
    );
    let mut opts = SshServerOptions::password(USER, PASSWORD);
    opts.port = port;
    opts.host_key = host_key;
    let _restarted = TestSshServer::start(opts).await;
    tunnel.ensure_connected().await.unwrap();
    assert!(tunnel.ssh_connected());
    assert_eq!(
        echo_roundtrip(tunnel.local_port(), b"back").await.unwrap(),
        b"back"
    );
}

/// RSA 2048 test key from the russh test-suite (OpenSSH format, unencrypted).
const RSA_TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAABFwAAAAdzc2gtcn
NhAAAAAwEAAQAAAQEAuSvQ9m76zhRB4m0BUKPf17lwccj7KQ1Qtse63AOqP/VYItqEH8un
rxPogXNBgrcCEm/ccLZZsyE3qgp3DRQkkqvJhZ6O8VBPsXxjZesRCqoFNCczy+Mf0R/Qmv
Rnpu5+4DDLz0p7vrsRZW9ji/c98KzxeUonWgkplQaCBYLN875WdeUYMGtb1MLfNCEj177j
gZl3CzttLRK3su6dckowXcXYv1gPTPZAwJb49J43o1QhV7+1zdwXvuFM6zuYHdu9ZHSKir
6k1dXOET3/U+LWG5ofAo8oxUWv/7vs6h7MeajwkUeIBOWYtD+wGYRvVpxvj7nyOoWtg+jm
0X6ndnsD+QAAA8irV+ZAq1fmQAAAAAdzc2gtcnNhAAABAQC5K9D2bvrOFEHibQFQo9/XuX
BxyPspDVC2x7rcA6o/9Vgi2oQfy6evE+iBc0GCtwISb9xwtlmzITeqCncNFCSSq8mFno7x
UE+xfGNl6xEKqgU0JzPL4x/RH9Ca9Gem7n7gMMvPSnu+uxFlb2OL9z3wrPF5SidaCSmVBo
IFgs3zvlZ15Rgwa1vUwt80ISPXvuOBmXcLO20tErey7p1ySjBdxdi/WA9M9kDAlvj0njej
VCFXv7XN3Be+4UzrO5gd271kdIqKvqTV1c4RPf9T4tYbmh8CjyjFRa//u+zqHsx5qPCRR4
gE5Zi0P7AZhG9WnG+PufI6ha2D6ObRfqd2ewP5AAAAAwEAAQAAAQAdELqhI/RsSpO45eFR
9hcZtnrm8WQzImrr9dfn1w9vMKSf++rHTuFIQvi48Q10ZiOGH1bbvlPAIVOqdjAPtnyzJR
HhzmyjhjasJlk30zj+kod0kz63HzSMT9EfsYNfmYoCyMYFCKz52EU3xc87Vhi74XmZz0D0
CgIj6TyZftmzC4YJCiwwU8K+29nxBhcbFRxpgwAksFL6PCSQsPl4y7yvXGcX+7lpZD8547
v58q3jIkH1g2tBOusIuaiphDDStVJhVdKA55Z0Kju2kvCqsRIlf1efrq43blRgJFFFCxNZ
8Cpolt4lOHhg+o3ucjILlCOgjDV8dB21YLxmgN5q+xFNAAAAgQC1P+eLUkHDFXnleCEVrW
xL/DFxEyneLQz3IawGdw7cyAb7vxsYrGUvbVUFkxeiv397pDHLZ5U+t5cOYDBZ7G43Mt2g
YfWBuRNvYhHA9Sdf38m5qPA6XCvm51f+FxInwd/kwRKH01RHJuRGsl/4Apu4DqVob8y00V
WTYyV6JBNDkQAAAIEA322lj7ZJXfK/oLhMM/RS+DvaMea1g/q43mdRJFQQso4XRCL6IIVn
oZXFeOxrMIRByVZBw+FSeB6OayWcZMySpJQBo70GdJOc3pJb3js0T+P2XA9+/jwXS58K9a
+IkgLkv9XkfxNGNKyPEEzXC8QQzvjs1LbmO59VLko8ypwHq/cAAACBANQqaULI0qdwa0vm
d3Ae1+k3YLZ0kapSQGVIMT2lkrhKV35tj7HIFpUPa4vitHzcUwtjYhqFezVF+JyPbJ/Fsp
XmEc0g1fFnQp5/SkUwoN2zm8Up52GBelkq2Jk57mOMzWO0QzzNuNV/feJk02b2aE8rrAqP
QR+u0AypRPmzHnOPAAAAEXJvb3RAMTQwOTExNTQ5NDBkAQ==
-----END OPENSSH PRIVATE KEY-----";
