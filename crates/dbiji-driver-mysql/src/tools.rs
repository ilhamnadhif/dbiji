//! Command lines for the vendor tools: `mysqldump` / `mariadb-dump` (backup),
//! `mysql` / `mariadb` (restore and "execute with local client").
//!
//! The password is never put on the command line or written to disk: it is
//! passed in the `MYSQL_PWD` environment variable of [`CommandSpec::env`]
//! (supported by the MySQL and MariaDB client tools; MySQL marks it as
//! deprecated but still honours it).

use crate::flavor::Flavor;
use dbiji_core::admin::{BackupFormat, BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::config::SslMode;
use dbiji_core::{ConnectParams, DbError, DbResult};
use std::path::Path;

/// Environment variable carrying the password.
pub const PASSWORD_ENV: &str = "MYSQL_PWD";

fn tool(flavor: Flavor, custom: Option<&str>, mysql: &str, mariadb: &str) -> String {
    tool_with(flavor, custom, mysql, mariadb, on_path)
}

/// MariaDB prefers its own `mariadb-*` tools and falls back to the MySQL
/// names when only those are installed (they work with MariaDB servers).
fn tool_with(
    flavor: Flavor,
    custom: Option<&str>,
    mysql: &str,
    mariadb: &str,
    installed: impl Fn(&str) -> bool,
) -> String {
    match custom.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => p.to_string(),
        None => match flavor {
            Flavor::MySql => mysql.to_string(),
            Flavor::MariaDb if !installed(mariadb) && installed(mysql) => mysql.to_string(),
            Flavor::MariaDb => mariadb.to_string(),
        },
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let p = dir.join(program);
            p.is_file() || (cfg!(windows) && p.with_extension("exe").is_file())
        })
    })
}

/// Connection options shared by all tools.
pub fn connection_args(flavor: Flavor, params: &ConnectParams) -> Vec<String> {
    let mut a = vec![
        format!("--host={}", params.host),
        format!("--port={}", params.port),
        "--protocol=TCP".to_string(),
    ];
    if let Some(u) = params.user.as_deref().filter(|u| !u.is_empty()) {
        a.push(format!("--user={u}"));
    }
    let mode = params.ssl.mode;
    match flavor {
        Flavor::MySql => {
            let m = match mode {
                SslMode::Disable => "DISABLED",
                SslMode::Prefer => "PREFERRED",
                SslMode::Require => "REQUIRED",
                SslMode::VerifyCa => "VERIFY_CA",
                SslMode::VerifyFull => "VERIFY_IDENTITY",
            };
            a.push(format!("--ssl-mode={m}"));
        }
        Flavor::MariaDb => match mode {
            SslMode::Disable => a.push("--skip-ssl".into()),
            SslMode::Prefer => a.push("--skip-ssl-verify-server-cert".into()),
            SslMode::Require => {
                a.push("--ssl".into());
                a.push("--skip-ssl-verify-server-cert".into());
            }
            SslMode::VerifyCa | SslMode::VerifyFull => {
                a.push("--ssl".into());
                a.push("--ssl-verify-server-cert".into());
            }
        },
    }
    if mode.uses_tls() {
        if let Some(ca) = params.ssl.ca_cert.as_deref().filter(|s| !s.is_empty()) {
            a.push(format!("--ssl-ca={ca}"));
        }
        if let Some(c) = params.ssl.client_cert.as_deref().filter(|s| !s.is_empty()) {
            a.push(format!("--ssl-cert={c}"));
        }
        if let Some(k) = params.ssl.client_key.as_deref().filter(|s| !s.is_empty()) {
            a.push(format!("--ssl-key={k}"));
        }
    }
    let charset = params
        .property("charset")
        .map(str::trim)
        .filter(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
        .unwrap_or("utf8mb4");
    a.push(format!("--default-character-set={charset}"));
    a
}

fn password_env(params: &ConnectParams) -> Vec<(String, dbiji_core::Secret)> {
    match &params.password {
        Some(p) if !p.is_empty() => vec![(PASSWORD_ENV.to_string(), p.clone())],
        _ => Vec::new(),
    }
}

/// `mysqldump` / `mariadb-dump` command.
pub fn backup_command(
    flavor: Flavor,
    params: &ConnectParams,
    opts: &BackupOptions,
) -> DbResult<CommandSpec> {
    if opts.format != BackupFormat::Plain {
        return Err(DbError::unsupported(
            "MySQL backups are plain SQL scripts; choose the plain format",
        ));
    }
    if opts.schema_only && opts.data_only {
        return Err(DbError::config(
            "'Schema only' and 'data only' cannot be combined",
        ));
    }
    if opts.output.as_os_str().is_empty() {
        return Err(DbError::config("No output file given for the backup"));
    }
    let program = tool(
        flavor,
        opts.tool_path.as_deref(),
        "mysqldump",
        "mariadb-dump",
    );
    let mut args = connection_args(flavor, params);
    if opts.single_transaction {
        args.push("--single-transaction".into());
    }
    if opts.routines {
        args.push("--routines".into());
        args.push("--events".into());
    }
    if opts.data_only {
        args.push("--no-create-info".into());
        args.push("--skip-triggers".into());
    } else {
        args.push("--triggers".into());
    }
    if opts.schema_only {
        args.push("--no-data".into());
    }
    if opts.clean {
        args.push("--add-drop-table".into());
        if opts.create_database {
            args.push("--add-drop-database".into());
        }
    } else {
        args.push("--skip-add-drop-table".into());
    }
    if opts.inserts {
        args.push("--skip-extended-insert".into());
    }
    args.push("--no-tablespaces".into());
    args.push(format!("--result-file={}", opts.output.display()));
    args.extend(opts.extra_args.iter().cloned());

    let databases: Vec<String> = if !opts.schemas.is_empty() {
        opts.schemas.clone()
    } else {
        opts.database
            .clone()
            .or_else(|| params.database.clone())
            .into_iter()
            .filter(|d| !d.is_empty())
            .collect()
    };
    if !opts.tables.is_empty() {
        if databases.len() != 1 {
            return Err(DbError::config("A table list needs exactly one database"));
        }
        args.push(databases[0].clone());
        args.extend(opts.tables.iter().cloned());
    } else if databases.is_empty() {
        args.push("--all-databases".into());
    } else if opts.create_database || databases.len() > 1 {
        args.push("--databases".into());
        args.extend(databases);
    } else {
        args.extend(databases);
    }
    Ok(CommandSpec {
        program,
        args,
        env: password_env(params),
        stdin_file: None,
        stdout_file: None,
    })
}

/// `mysql` / `mariadb` command reading a plain SQL dump from stdin.
///
/// With `create_database` no database is selected: the script is expected to
/// contain `CREATE DATABASE` / `USE` (dumps made with `--databases`).
pub fn restore_command(
    flavor: Flavor,
    params: &ConnectParams,
    opts: &RestoreOptions,
) -> DbResult<CommandSpec> {
    if opts.format != BackupFormat::Plain {
        return Err(DbError::unsupported(
            "MySQL restores read plain SQL scripts",
        ));
    }
    if opts.input.as_os_str().is_empty() {
        return Err(DbError::config("No input file given for the restore"));
    }
    let program = tool(flavor, opts.tool_path.as_deref(), "mysql", "mariadb");
    let mut args = connection_args(flavor, params);
    if !opts.exit_on_error {
        args.push("--force".into());
    }
    args.extend(opts.extra_args.iter().cloned());
    if !opts.create_database
        && let Some(db) = opts
            .database
            .clone()
            .or_else(|| params.database.clone())
            .filter(|d| !d.is_empty())
    {
        args.push(db);
    }
    Ok(CommandSpec {
        program,
        args,
        env: password_env(params),
        stdin_file: Some(opts.input.clone()),
        stdout_file: None,
    })
}

/// `mysql` / `mariadb` command executing `script` (stdin).
pub fn client_command(
    flavor: Flavor,
    params: &ConnectParams,
    script: &Path,
) -> DbResult<CommandSpec> {
    let program = tool(flavor, params.property("client_path"), "mysql", "mariadb");
    let mut args = connection_args(flavor, params);
    args.push("--table".into());
    args.push("--show-warnings".into());
    if let Some(db) = params.database.clone().filter(|d| !d.is_empty()) {
        args.push(db);
    }
    Ok(CommandSpec {
        program,
        args,
        env: password_env(params),
        stdin_file: Some(script.to_path_buf()),
        stdout_file: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::config::SslConfig;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn params() -> ConnectParams {
        ConnectParams {
            kind: dbiji_core::DbKind::MySql,
            host: "db.example.com".into(),
            port: 3307,
            tls_server_name: None,
            database: Some("shop".into()),
            user: Some("admin".into()),
            password: Some("s3cret pass".into()),
            file_path: None,
            ssl: SslConfig {
                mode: SslMode::Require,
                ..Default::default()
            },
            properties: BTreeMap::new(),
            application_name: "DBiji".into(),
            connect_timeout: Duration::from_secs(5),
            read_only: false,
            auto_commit: true,
        }
    }

    #[test]
    fn backup() {
        let opts = BackupOptions {
            output: PathBuf::from("/tmp/out dir/shop.sql"),
            single_transaction: true,
            routines: true,
            clean: true,
            ..Default::default()
        };
        let c = backup_command(Flavor::MySql, &params(), &opts).unwrap();
        assert_eq!(c.program, "mysqldump");
        assert_eq!(
            c.args,
            vec![
                "--host=db.example.com",
                "--port=3307",
                "--protocol=TCP",
                "--user=admin",
                "--ssl-mode=REQUIRED",
                "--default-character-set=utf8mb4",
                "--single-transaction",
                "--routines",
                "--events",
                "--triggers",
                "--add-drop-table",
                "--no-tablespaces",
                "--result-file=/tmp/out dir/shop.sql",
                "shop",
            ]
        );
        // password only in the environment, never in args or the display string
        assert_eq!(c.env.len(), 1);
        assert_eq!(c.env[0].0, "MYSQL_PWD");
        assert_eq!(c.env[0].1.expose(), "s3cret pass");
        assert!(!c.display().contains("s3cret"));
        assert!(!c.args.iter().any(|a| a.contains("s3cret")));

        let opts = BackupOptions {
            output: PathBuf::from("/b.sql"),
            schemas: vec!["a".into(), "b".into()],
            schema_only: true,
            ..Default::default()
        };
        let c = backup_command(Flavor::MariaDb, &params(), &opts).unwrap();
        assert!(
            c.program == "mariadb-dump" || c.program == "mysqldump",
            "{}",
            c.program
        );
        assert!(c.args.contains(&"--no-data".to_string()));
        assert!(
            c.args.contains(&"--ssl".to_string())
                && c.args
                    .contains(&"--skip-ssl-verify-server-cert".to_string())
        );
        assert_eq!(&c.args[c.args.len() - 3..], &["--databases", "a", "b"]);

        let opts = BackupOptions {
            output: PathBuf::from("/c.sql"),
            tables: vec!["t1".into(), "t2".into()],
            data_only: true,
            inserts: true,
            ..Default::default()
        };
        let c = backup_command(Flavor::MySql, &params(), &opts).unwrap();
        assert_eq!(&c.args[c.args.len() - 3..], &["shop", "t1", "t2"]);
        assert!(
            c.args.contains(&"--no-create-info".to_string())
                && c.args.contains(&"--skip-extended-insert".to_string())
        );

        let mut p = params();
        p.database = None;
        let opts = BackupOptions {
            output: PathBuf::from("/d.sql"),
            tool_path: Some("/opt/mysql/bin/mysqldump".into()),
            ..Default::default()
        };
        let c = backup_command(Flavor::MySql, &p, &opts).unwrap();
        assert_eq!(c.program, "/opt/mysql/bin/mysqldump");
        assert_eq!(c.args.last().map(String::as_str), Some("--all-databases"));

        assert!(
            backup_command(
                Flavor::MySql,
                &params(),
                &BackupOptions {
                    output: "/x".into(),
                    format: BackupFormat::Custom,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(backup_command(Flavor::MySql, &params(), &BackupOptions::default()).is_err());
    }

    #[test]
    fn restore_and_client() {
        let opts = RestoreOptions {
            input: PathBuf::from("/in.sql"),
            exit_on_error: false,
            ..Default::default()
        };
        let c = restore_command(Flavor::MySql, &params(), &opts).unwrap();
        assert_eq!(c.program, "mysql");
        assert_eq!(c.stdin_file.as_deref(), Some(Path::new("/in.sql")));
        assert!(c.args.contains(&"--force".to_string()));
        assert_eq!(c.args.last().map(String::as_str), Some("shop"));
        assert!(c.display().ends_with("< /in.sql"));

        let opts = RestoreOptions {
            input: PathBuf::from("/in.sql"),
            create_database: true,
            exit_on_error: true,
            ..Default::default()
        };
        let c = restore_command(Flavor::MariaDb, &params(), &opts).unwrap();
        assert_eq!(c.program, "mariadb");
        assert!(!c.args.contains(&"shop".to_string()) && !c.args.contains(&"--force".to_string()));

        let mut p = params();
        p.ssl.mode = SslMode::VerifyFull;
        p.ssl.ca_cert = Some("/ca.pem".into());
        let c = client_command(Flavor::MySql, &p, Path::new("/script.sql")).unwrap();
        assert!(
            c.args.contains(&"--ssl-mode=VERIFY_IDENTITY".to_string())
                && c.args.contains(&"--ssl-ca=/ca.pem".to_string())
        );
        assert_eq!(c.stdin_file.as_deref(), Some(Path::new("/script.sql")));
        let mut p = params();
        p.ssl.mode = SslMode::Disable;
        p.password = None;
        let c = client_command(Flavor::MariaDb, &p, Path::new("/s.sql")).unwrap();
        assert!(c.args.contains(&"--skip-ssl".to_string()));
        assert!(c.env.is_empty());
    }

    #[test]
    fn mariadb_falls_back_to_mysql_tools() {
        let only_mysql = |p: &str| p == "mysqldump";
        assert_eq!(
            tool_with(
                Flavor::MariaDb,
                None,
                "mysqldump",
                "mariadb-dump",
                only_mysql
            ),
            "mysqldump"
        );
        assert_eq!(
            tool_with(Flavor::MariaDb, None, "mysqldump", "mariadb-dump", |_| true),
            "mariadb-dump"
        );
        assert_eq!(
            tool_with(Flavor::MariaDb, None, "mysqldump", "mariadb-dump", |_| {
                false
            }),
            "mariadb-dump"
        );
        assert_eq!(
            tool_with(
                Flavor::MariaDb,
                Some("/x/dump"),
                "mysqldump",
                "mariadb-dump",
                only_mysql
            ),
            "/x/dump"
        );
        assert_eq!(
            tool_with(Flavor::MySql, None, "mysqldump", "mariadb-dump", |_| false),
            "mysqldump"
        );
    }
}
