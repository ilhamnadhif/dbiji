//! Runs the generated `mysqldump` / `mariadb-dump` and `mysql` / `mariadb`
//! command lines inside the test containers (the client tools are not
//! installed locally). Enabled with `DBIJI_TEST_DOCKER_MYSQL` /
//! `DBIJI_TEST_DOCKER_MARIADB` = container name, in addition to the server
//! URLs; the tools connect to the server inside the container.

mod common;

use common::*;
use dbiji_core::admin::{BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::config::SslMode;
use dbiji_core::{Driver, Session, Value};
use dbiji_driver_mysql::{MariaDbDriver, MySqlDriver};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn docker_exec(container: &str, spec: &CommandSpec, stdin: Option<&[u8]>) -> std::process::Output {
    let mut cmd = Command::new("docker");
    cmd.arg("exec");
    if stdin.is_some() {
        cmd.arg("-i");
    }
    for (k, v) in &spec.env {
        cmd.arg("-e").arg(format!("{k}={}", v.expose()));
    }
    cmd.arg(container).arg(&spec.program).args(&spec.args);
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("docker exec");
    if let Some(data) = stdin {
        let mut s = child.stdin.take().expect("stdin");
        s.write_all(data).expect("write stdin");
    }
    child.wait_with_output().expect("docker exec output")
}

#[tokio::test]
async fn dump_and_restore_with_vendor_tools() {
    for srv in servers() {
        let var = if srv.is_mariadb() {
            "DBIJI_TEST_DOCKER_MARIADB"
        } else {
            "DBIJI_TEST_DOCKER_MYSQL"
        };
        let Ok(container) = std::env::var(var) else {
            continue;
        };
        let driver: Box<dyn Driver> = if srv.is_mariadb() {
            Box::new(MariaDbDriver::new())
        } else {
            Box::new(MySqlDriver::new())
        };
        let src = TestDb::create(&srv).await;
        let dst = TestDb::create(&srv).await;
        let mut s = src.session().await;
        exec(&mut s, "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20)); INSERT INTO t VALUES (1, 'a''b'), (2, 'é')").await;
        exec(
            &mut s,
            "CREATE FUNCTION f_one() RETURNS INT DETERMINISTIC RETURN 1",
        )
        .await;
        s.close().await.unwrap();

        // the tools run inside the container: connect to its own server
        let mut inside = srv.params_for(&src.name);
        inside.host = "127.0.0.1".into();
        inside.port = 3306;
        inside.ssl.mode = SslMode::Disable;
        let dump_file = PathBuf::from(format!("/tmp/dbiji_dump_{}.sql", suffix()));
        let backup = driver
            .backup_command(
                &inside,
                &BackupOptions {
                    output: dump_file.clone(),
                    single_transaction: true,
                    routines: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let out = docker_exec(&container, &backup, None);
        assert!(
            out.status.success(),
            "[{}] {}: {}",
            srv.name,
            backup.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        let cat = Command::new("docker")
            .args(["exec", &container, "cat"])
            .arg(&dump_file)
            .output()
            .unwrap();
        let _ = Command::new("docker")
            .args(["exec", &container, "rm", "-f"])
            .arg(&dump_file)
            .output();
        let dump = cat.stdout;
        let text = String::from_utf8_lossy(&dump);
        assert!(
            text.contains("CREATE TABLE `t`") && text.contains("f_one"),
            "[{}] dump:\n{text}",
            srv.name
        );

        let mut target = inside.clone();
        target.database = Some(dst.name.clone());
        let restore = driver
            .restore_command(
                &target,
                &RestoreOptions {
                    input: PathBuf::from("/dev/stdin"),
                    exit_on_error: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let out = docker_exec(&container, &restore, Some(&dump));
        assert!(
            out.status.success(),
            "[{}] {}: {}",
            srv.name,
            restore.display(),
            String::from_utf8_lossy(&out.stderr)
        );

        let mut d = dst.session().await;
        assert_eq!(
            query(
                &mut d,
                "SELECT GROUP_CONCAT(name ORDER BY id SEPARATOR '|') FROM t"
            )
            .await[0][0],
            Value::text("a'b|é")
        );
        assert_eq!(query(&mut d, "SELECT f_one()").await[0][0], Value::Int(1));

        // execute a script with the local client
        let script = b"SELECT COUNT(*) AS n FROM t;\n";
        let client = driver
            .client_command(&target, std::path::Path::new("/dev/stdin"))
            .unwrap();
        let out = docker_exec(&container, &client, Some(script));
        assert!(
            out.status.success(),
            "[{}] {}",
            srv.name,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains('2'),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        d.close().await.unwrap();
        dst.drop_now().await;
        src.drop_now().await;
    }
}
