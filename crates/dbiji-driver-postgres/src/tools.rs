//! Command lines for the PostgreSQL client tools: `pg_dump`, `pg_restore`
//! and `psql`. Secrets are passed only through `PGPASSWORD` in
//! [`CommandSpec::env`]; TLS settings through `PGSSL*` variables.

use dbiji_core::admin::{BackupFormat, BackupOptions, CommandSpec, RestoreOptions};
use dbiji_core::config::SslMode;
use dbiji_core::{ConnectParams, DbError, DbResult, Secret};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

fn exe_name(tool: &str) -> String {
    if cfg!(windows) { format!("{tool}.exe") } else { tool.to_string() }
}

/// Well-known installation directories searched after `PATH`.
fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push("/opt/homebrew/bin".into());
        dirs.push("/usr/local/bin".into());
        dirs.push("/Applications/Postgres.app/Contents/Versions/latest/bin".into());
        for v in (12..=19).rev() {
            dirs.push(format!("/opt/homebrew/opt/postgresql@{v}/bin").into());
            dirs.push(format!("/usr/local/opt/postgresql@{v}/bin").into());
        }
    }
    if cfg!(target_os = "linux") {
        for v in (12..=19).rev() {
            dirs.push(format!("/usr/lib/postgresql/{v}/bin").into());
            dirs.push(format!("/usr/pgsql-{v}/bin").into());
        }
    }
    if cfg!(windows) {
        for v in (12..=19).rev() {
            dirs.push(format!("C:\\Program Files\\PostgreSQL\\{v}\\bin").into());
        }
    }
    dirs
}

/// Resolve a tool: an explicit path (file, or directory containing the tool),
/// else `PATH`, else well-known locations, else the bare name.
pub(crate) fn find_tool(tool: &str, explicit: Option<&str>) -> DbResult<String> {
    let exe = exe_name(tool);
    if let Some(p) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(p);
        if path.is_dir() {
            let candidate = path.join(&exe);
            if candidate.is_file() {
                return Ok(candidate.to_string_lossy().into_owned());
            }
            return Err(DbError::config(format!("{exe} was not found in {}", path.display())));
        }
        // a directory of the client tools given as path to another tool
        if path.is_file() {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if name == exe {
                return Ok(path.to_string_lossy().into_owned());
            }
            if let Some(dir) = path.parent() {
                let sibling = dir.join(&exe);
                if sibling.is_file() {
                    return Ok(sibling.to_string_lossy().into_owned());
                }
            }
            return Ok(path.to_string_lossy().into_owned());
        }
        return Err(DbError::config(format!("The tool path {} does not exist", path.display())));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(&exe);
            if candidate.is_file() {
                return Ok(candidate.to_string_lossy().into_owned());
            }
        }
    }
    for dir in extra_dirs() {
        let candidate = dir.join(&exe);
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    Ok(exe)
}

/// Value for a libpq conninfo string.
fn conninfo_value(v: &str) -> String {
    if !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:@".contains(c)) {
        v.to_string()
    } else {
        format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// Connection arguments for the tools: `-h host -p port -U user -d dbname`.
///
/// When the session goes through a local forwarder (`tls_server_name` set
/// and `host` an IP address), `-d` carries a libpq connection string with
/// `host=<real name> hostaddr=<forwarder>`, so libpq connects to the
/// forwarder while verifying TLS (and sending SNI) for the real server name.
pub(crate) fn connection_args(params: &ConnectParams, database: Option<&str>) -> Vec<String> {
    let host = if params.host.trim().is_empty() { "localhost" } else { params.host.trim() };
    let db = database.or(params.database.as_deref()).filter(|d| !d.is_empty());
    let mut args = Vec::new();
    match (&params.tls_server_name, host.parse::<IpAddr>()) {
        (Some(name), Ok(_)) if !name.is_empty() && name != host => {
            let mut parts = vec![
                format!("host={}", conninfo_value(name)),
                format!("hostaddr={}", conninfo_value(host)),
                format!("port={}", params.port),
            ];
            if let Some(d) = db {
                parts.push(format!("dbname={}", conninfo_value(d)));
            }
            if let Some(u) = params.user.as_deref().filter(|u| !u.is_empty()) {
                args.push("-U".into());
                args.push(u.to_string());
            }
            args.push("-d".into());
            args.push(parts.join(" "));
        }
        _ => {
            args.push("-h".into());
            args.push(host.to_string());
            args.push("-p".into());
            args.push(params.port.to_string());
            if let Some(u) = params.user.as_deref().filter(|u| !u.is_empty()) {
                args.push("-U".into());
                args.push(u.to_string());
            }
            if let Some(d) = db {
                args.push("-d".into());
                // libpq would expand a name containing '=' or a URI prefix as a
                // connection string; pass such names as an explicit `dbname=`
                if d.contains('=') || d.starts_with("postgres://") || d.starts_with("postgresql://") {
                    args.push(format!("dbname={}", conninfo_value(d)));
                } else {
                    args.push(d.to_string());
                }
            }
        }
    }
    args
}

fn file_path_of(source: &str) -> Option<String> {
    // inline PEM cannot be passed to libpq; only file paths
    if source.contains("-----BEGIN") || source.trim().is_empty() {
        None
    } else {
        Some(source.trim().to_string())
    }
}

/// Environment for the tools: password and TLS settings.
pub(crate) fn tool_env(params: &ConnectParams) -> Vec<(String, Secret)> {
    let mut env = Vec::new();
    if let Some(pw) = &params.password
        && !pw.is_empty()
    {
        env.push(("PGPASSWORD".to_string(), pw.clone()));
    }
    env.push(("PGSSLMODE".to_string(), Secret::new(params.ssl.mode.label())));
    if params.ssl.mode != SslMode::Disable {
        if let Some(ca) = params.ssl.ca_cert.as_deref().and_then(file_path_of) {
            env.push(("PGSSLROOTCERT".to_string(), Secret::new(ca)));
        } else if params.ssl.mode.verifies_chain() {
            env.push(("PGSSLROOTCERT".to_string(), Secret::new("system")));
        }
        if let Some(c) = params.ssl.client_cert.as_deref().and_then(file_path_of) {
            env.push(("PGSSLCERT".to_string(), Secret::new(c)));
        }
        if let Some(k) = params.ssl.client_key.as_deref().and_then(file_path_of) {
            env.push(("PGSSLKEY".to_string(), Secret::new(k)));
        }
    }
    let app = if params.application_name.is_empty() { "DBiji" } else { params.application_name.as_str() };
    env.push(("PGAPPNAME".to_string(), Secret::new(app)));
    env.push(("PGCONNECT_TIMEOUT".to_string(), Secret::new(params.connect_timeout.as_secs().max(1).to_string())));
    env
}

/// `-t` pattern for a table name (quoted when needed so case is preserved).
fn table_pattern(t: &str) -> String {
    let t = t.trim();
    if t.contains('"') || t.contains('*') || t.contains('?') {
        return t.to_string();
    }
    match t.split_once('.') {
        Some((s, n)) => format!("{}.{}", crate::dialect::quote_ident(s), crate::dialect::quote_ident(n)),
        None => crate::dialect::quote_ident(t),
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `pg_dump` command for a backup.
pub(crate) fn backup_command(params: &ConnectParams, opts: &BackupOptions) -> DbResult<CommandSpec> {
    if opts.output.as_os_str().is_empty() {
        return Err(DbError::config("No output file given for the backup"));
    }
    if opts.schema_only && opts.data_only {
        return Err(DbError::config("Schema only and data only cannot be combined"));
    }
    let program = find_tool("pg_dump", opts.tool_path.as_deref())?;
    let mut args = vec!["--verbose".to_string(), "--no-password".to_string()];
    let fmt = match opts.format {
        BackupFormat::Plain => "p",
        BackupFormat::Custom => "c",
        BackupFormat::Tar => "t",
        BackupFormat::Directory => "d",
    };
    args.push(format!("--format={fmt}"));
    args.push(format!("--file={}", path_str(&opts.output)));
    if opts.schema_only {
        args.push("--schema-only".into());
    }
    if opts.data_only {
        args.push("--data-only".into());
    }
    for s in &opts.schemas {
        args.push(format!("--schema={}", table_pattern(s)));
    }
    for t in &opts.tables {
        args.push(format!("--table={}", table_pattern(t)));
    }
    if opts.clean {
        args.push("--clean".into());
        args.push("--if-exists".into());
    }
    if opts.create_database {
        args.push("--create".into());
    }
    if opts.no_owner {
        args.push("--no-owner".into());
    }
    if opts.no_privileges {
        args.push("--no-privileges".into());
    }
    if opts.inserts {
        args.push("--inserts".into());
    }
    if let Some(z) = opts.compression {
        if opts.format == BackupFormat::Tar {
            tracing::debug!("pg_dump does not compress tar archives; ignoring the compression level");
        } else {
            args.push(format!("--compress={}", z.min(9)));
        }
    }
    args.extend(opts.extra_args.iter().cloned());
    args.extend(connection_args(params, opts.database.as_deref()));
    Ok(CommandSpec { program, args, env: tool_env(params), stdin_file: None, stdout_file: None })
}

/// `pg_restore` (archives) or `psql -f` (plain SQL) command for a restore.
pub(crate) fn restore_command(params: &ConnectParams, opts: &RestoreOptions) -> DbResult<CommandSpec> {
    if opts.input.as_os_str().is_empty() {
        return Err(DbError::config("No input file given for the restore"));
    }
    let env = tool_env(params);
    match opts.format {
        BackupFormat::Plain => {
            let program = find_tool("psql", opts.tool_path.as_deref())?;
            let mut args = vec!["--no-psqlrc".to_string(), "--no-password".to_string()];
            if opts.exit_on_error {
                args.push("--set=ON_ERROR_STOP=1".into());
            }
            if opts.single_transaction {
                args.push("--single-transaction".into());
            }
            args.push(format!("--file={}", path_str(&opts.input)));
            args.extend(opts.extra_args.iter().cloned());
            // a dump made with --create connects to the new database itself
            let db = if opts.create_database { Some("postgres") } else { opts.database.as_deref() };
            args.extend(connection_args(params, db));
            Ok(CommandSpec { program, args, env, stdin_file: None, stdout_file: None })
        }
        fmt => {
            let program = find_tool("pg_restore", opts.tool_path.as_deref())?;
            let mut args = vec!["--verbose".to_string(), "--no-password".to_string()];
            args.push(format!(
                "--format={}",
                match fmt {
                    BackupFormat::Tar => "t",
                    BackupFormat::Directory => "d",
                    _ => "c",
                }
            ));
            if opts.clean {
                args.push("--clean".into());
                args.push("--if-exists".into());
            }
            if opts.create_database {
                args.push("--create".into());
            }
            if opts.no_owner {
                args.push("--no-owner".into());
            }
            if opts.single_transaction {
                args.push("--single-transaction".into());
            }
            if opts.exit_on_error {
                args.push("--exit-on-error".into());
            }
            args.extend(opts.extra_args.iter().cloned());
            // with --create, -d names the database used to issue CREATE DATABASE
            let db = if opts.create_database { Some("postgres") } else { opts.database.as_deref() };
            args.extend(connection_args(params, db));
            args.push(path_str(&opts.input));
            Ok(CommandSpec { program, args, env, stdin_file: None, stdout_file: None })
        }
    }
}

/// `psql` running a script file ("execute with local client").
pub(crate) fn client_command(params: &ConnectParams, script: &Path) -> DbResult<CommandSpec> {
    let program = find_tool("psql", params.property("psql_path"))?;
    let mut args = vec![
        "--no-psqlrc".to_string(),
        "--no-password".to_string(),
        "--set=ON_ERROR_STOP=1".to_string(),
        "--echo-errors".to_string(),
        format!("--file={}", path_str(script)),
    ];
    args.extend(connection_args(params, None));
    Ok(CommandSpec { program, args, env: tool_env(params), stdin_file: None, stdout_file: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::config::SslConfig;
    use std::time::Duration;

    fn params() -> ConnectParams {
        let mut cfg = dbiji_core::ConnectionConfig::new(dbiji_core::DbKind::Postgres, "t");
        cfg.host = "db.example.com".into();
        cfg.user = Some("app user".into());
        cfg.database = Some("shop".into());
        cfg.ssl = SslConfig { mode: SslMode::VerifyFull, ca_cert: Some("/etc/ca.pem".into()), ..Default::default() };
        let mut p = ConnectParams::from_config(&cfg, Some(Secret::new("hunter2")));
        p.connect_timeout = Duration::from_secs(7);
        p
    }

    #[test]
    fn dump_arguments() {
        let opts = BackupOptions {
            output: "/tmp/out.dump".into(),
            format: BackupFormat::Custom,
            schemas: vec!["app".into()],
            tables: vec!["app.Orders".into()],
            clean: true,
            no_owner: true,
            no_privileges: true,
            compression: Some(6),
            tool_path: Some("/nonexistent-dir-for-test/pg_dump".into()),
            ..Default::default()
        };
        // explicit missing path is an error
        assert!(backup_command(&params(), &opts).is_err());
        let opts = BackupOptions { tool_path: None, ..opts };
        let c = backup_command(&params(), &opts).unwrap();
        assert!(c.program.ends_with("pg_dump"));
        assert!(c.args.contains(&"--format=c".to_string()));
        assert!(c.args.contains(&"--file=/tmp/out.dump".to_string()));
        assert!(c.args.contains(&"--schema=app".to_string()));
        assert!(c.args.contains(&"--table=app.\"Orders\"".to_string()));
        assert!(c.args.contains(&"--clean".to_string()));
        assert!(c.args.contains(&"--compress=6".to_string()));
        let conn = &c.args[c.args.len() - 8..];
        assert_eq!(conn, ["-h", "db.example.com", "-p", "5432", "-U", "app user", "-d", "shop"]);
        assert!(c.env.iter().any(|(k, v)| k == "PGCONNECT_TIMEOUT" && v.expose() == "7"));
        // the password never appears in the arguments or the display text
        assert!(!c.display().contains("hunter2"));
        assert!(c.args.iter().all(|a| !a.contains("hunter2")));
        let pw = c.env.iter().find(|(k, _)| k == "PGPASSWORD").unwrap();
        assert_eq!(pw.1.expose(), "hunter2");
        assert_eq!(c.env.iter().find(|(k, _)| k == "PGSSLMODE").unwrap().1.expose(), "verify-full");
        assert_eq!(c.env.iter().find(|(k, _)| k == "PGSSLROOTCERT").unwrap().1.expose(), "/etc/ca.pem");
    }

    #[test]
    fn tunnel_uses_hostaddr() {
        let mut p = params();
        p.host = "127.0.0.1".into();
        p.port = 40001;
        p.tls_server_name = Some("db.example.com".into());
        let args = connection_args(&p, None);
        assert_eq!(args[0..2], ["-U".to_string(), "app user".to_string()]);
        assert_eq!(args[2], "-d");
        assert_eq!(args[3], "host=db.example.com hostaddr=127.0.0.1 port=40001 dbname=shop");
        // a database name that libpq would expand as a connection string
        let args = connection_args(&params(), Some("a=b c"));
        assert_eq!(args[args.len() - 2..], ["-d".to_string(), "dbname='a=b c'".to_string()]);
    }

    #[test]
    fn restore_arguments() {
        let plain = RestoreOptions {
            input: "/tmp/in.sql".into(),
            format: BackupFormat::Plain,
            single_transaction: true,
            exit_on_error: true,
            ..Default::default()
        };
        let c = restore_command(&params(), &plain).unwrap();
        assert!(c.program.ends_with("psql"));
        assert!(c.args.contains(&"--set=ON_ERROR_STOP=1".to_string()));
        assert!(c.args.contains(&"--single-transaction".to_string()));
        assert!(c.args.contains(&"--file=/tmp/in.sql".to_string()));
        let archive = RestoreOptions {
            input: "/tmp/in.dump".into(),
            format: BackupFormat::Custom,
            clean: true,
            create_database: true,
            exit_on_error: true,
            ..Default::default()
        };
        let c = restore_command(&params(), &archive).unwrap();
        assert!(c.program.ends_with("pg_restore"));
        assert!(c.args.contains(&"--create".to_string()));
        assert!(c.args.contains(&"--exit-on-error".to_string()));
        assert!(c.args.windows(2).any(|w| w[0] == "-d" && w[1] == "postgres"));
        assert_eq!(c.args.last().unwrap(), "/tmp/in.dump");
        let s = client_command(&params(), Path::new("/tmp/x.sql")).unwrap();
        assert!(s.args.contains(&"--file=/tmp/x.sql".to_string()));
    }
}
