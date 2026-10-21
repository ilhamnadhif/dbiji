//! Collect diagnostic info: a zip file with the application log, the
//! settings and a description of the system and of the connections, for a
//! bug report. Nothing identifying or secret is included: no passwords or
//! keys (they are never in these files), no connection names, host names,
//! user names, database names or connection properties; credentials
//! embedded in URLs are masked and the home directory is shown as `~`.

use dbiji_config::settings::AppSettings as Settings;
use dbiji_core::config::ConnectionConfig;
use dbiji_core::{DbError, DbResult};
use std::io::Write;
use std::path::{Path, PathBuf};

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// [`mask_url_credentials`] and the home directory as `~`.
fn scrub(text: &str) -> String {
    let masked = mask_url_credentials(text);
    match home().map(|h| h.to_string_lossy().trim_end_matches(['/', '\\']).to_string()).filter(|h| h.len() > 1) {
        Some(h) => masked.replace(&h, "~"),
        None => masked,
    }
}

/// Mask `user:password@` in URLs (`https://u:p@host` → `https://***@host`).
pub fn mask_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || "/\"'?#".contains(c)).unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// What is known about the connections, without anything identifying.
pub fn describe_connections(conns: &[ConnectionConfig]) -> serde_json::Value {
    let list: Vec<serde_json::Value> = conns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            serde_json::json!({
                "connection": i + 1,
                "kind": c.kind.display_name(),
                "type": c.connection_type,
                "auth": format!("{:?}", c.auth),
                "saved_password": c.save_password,
                "read_only": c.read_only,
                "auto_commit": c.auto_commit,
                "ssh_tunnel": c.ssh.is_some(),
                "proxy": c.proxy.is_some(),
                "network_profile": c.network_profile.is_some(),
                "ssl_mode": format!("{:?}", c.ssl.mode),
                "properties": c.properties.len(),
                "view": format!("{:?}", c.navigator_view),
            })
        })
        .collect();
    serde_json::Value::Array(list)
}

/// System and build information.
pub fn system_info(paths: &dbiji_config::AppPaths, settings: &Settings, conns: &[ConnectionConfig]) -> String {
    let mut kinds: Vec<String> = Vec::new();
    for k in dbiji_core::DbKind::ALL {
        let n = conns.iter().filter(|c| c.kind == k).count();
        if n > 0 {
            kinds.push(format!("{} × {n}", k.display_name()));
        }
    }
    format!(
        "DBiji {version}\nOS: {os} ({family}, {arch})\nLocale: {locale:?}\nTheme: {theme:?}\nPortable: {portable}\nConnections: {count} ({kinds})\nTime: {time}\n",
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        family = std::env::consts::FAMILY,
        arch = std::env::consts::ARCH,
        locale = settings.general.effective_locale(),
        theme = settings.general.theme,
        portable = paths.is_portable(),
        count = conns.len(),
        kinds = if kinds.is_empty() { "-".to_string() } else { kinds.join(", ") },
        time = chrono::Local::now().to_rfc3339(),
    )
}

/// Names, hosts, users, databases and files of the connections with their
/// stand-ins: the logs mention them ("Cannot connect to Prod: …").
fn identifying_words(conns: &[ConnectionConfig]) -> Vec<(String, String)> {
    let mut words: Vec<(String, String)> = Vec::new();
    for (i, c) in conns.iter().enumerate() {
        let n = i + 1;
        let mut push = |w: Option<&str>, what: &str| {
            if let Some(w) = w.map(str::trim).filter(|w| w.chars().count() >= 3) {
                words.push((w.to_string(), format!("<{what} {n}>")));
            }
        };
        push(Some(c.name.as_str()), "connection");
        push(Some(c.host.as_str()), "host");
        push(c.user.as_deref(), "user");
        push(c.database.as_deref(), "database");
        push(c.file_path.as_deref(), "file");
    }
    // Longer first: a name containing a host name is replaced whole.
    words.sort_by_key(|w| std::cmp::Reverse(w.0.len()));
    words.dedup_by(|a, b| a.0 == b.0);
    words
}

fn redact(text: &str, words: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (w, stand_in) in words {
        if out.contains(w.as_str()) {
            out = out.replace(w.as_str(), stand_in);
        }
    }
    out
}

/// Write the diagnostic zip to `out`.
pub fn collect(paths: &dbiji_config::AppPaths, settings: &Settings, conns: &[ConnectionConfig], out: &Path) -> DbResult<PathBuf> {
    let io = |e: std::io::Error| DbError::io(e.to_string());
    let zerr = |e: zip::result::ZipError| DbError::io(e.to_string());
    let file = std::fs::File::create(out).map_err(io)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let identifying = identifying_words(conns);
    let add = |zip: &mut zip::ZipWriter<std::fs::File>, name: &str, text: &str| -> DbResult<()> {
        zip.start_file(name, opts).map_err(zerr)?;
        zip.write_all(redact(&scrub(text), &identifying).as_bytes()).map_err(io)
    };
    add(&mut zip, "system.txt", &system_info(paths, settings, conns))?;
    let settings_json = serde_json::to_string_pretty(settings).map_err(|e| DbError::internal(e.to_string()))?;
    add(&mut zip, "settings.json", &settings_json)?;
    let conns_json = serde_json::to_string_pretty(&describe_connections(conns)).map_err(|e| DbError::internal(e.to_string()))?;
    add(&mut zip, "connections.json", &conns_json)?;
    let session: Vec<String> = crate::applog::entries().iter().map(|e| e.line()).collect();
    add(&mut zip, "log/session.log", &session.join("\n"))?;
    for name in ["dbiji.1.log", "dbiji.log"] {
        let p = paths.logs_dir().join(name);
        if let Ok(text) = std::fs::read_to_string(&p) {
            add(&mut zip, &format!("log/{name}"), &text)?;
        }
    }
    zip.finish().map_err(zerr)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o600));
    }
    Ok(out.to_path_buf())
}

/// Default file: `~/Downloads/dbiji-diagnostics-<date>.zip` (or the home directory).
pub fn default_output() -> PathBuf {
    let dir = home().map(|h| if h.join("Downloads").is_dir() { h.join("Downloads") } else { h }).unwrap_or_else(std::env::temp_dir);
    dir.join(format!("dbiji-diagnostics-{}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::DbKind;

    #[test]
    fn masks_and_collects_without_identifying_data() {
        assert_eq!(mask_url_credentials("see https://bob:s3cret@api.example.com/v1 and postgres://h/db"), "see https://***@api.example.com/v1 and postgres://h/db");
        assert_eq!(mask_url_credentials("no url here: a@b"), "no url here: a@b");

        let root = std::env::temp_dir().join(format!("dbiji-diag-{}", uuid::Uuid::new_v4().simple()));
        let paths = dbiji_config::AppPaths::with_root(&root);
        std::fs::create_dir_all(paths.logs_dir()).unwrap();
        std::fs::write(paths.logs_dir().join("dbiji.log"), "2026-01-01 INFO dbiji started\n").unwrap();
        let mut settings = Settings::default();
        settings.ai.base_url = Some("https://user:topsecret@llm.local/v1".into());
        let mut c = ConnectionConfig::new(DbKind::Postgres, "Payroll production");
        c.host = "payroll.internal.example".into();
        c.user = Some("hr_admin".into());
        c.database = Some("payroll".into());
        c.properties.insert("options".into(), "-c search_path=hr".into());
        let out = root.join("diag.zip");
        collect(&paths, &settings, &[c], &out).unwrap();

        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut all = String::new();
        let mut names = Vec::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            names.push(f.name().unwrap().to_string());
            std::io::Read::read_to_string(&mut f, &mut all).unwrap();
        }
        for n in ["system.txt", "settings.json", "connections.json", "log/session.log", "log/dbiji.log"] {
            assert!(names.iter().any(|x| x == n), "{n} in {names:?}");
        }
        assert!(all.contains("PostgreSQL") && all.contains("dbiji started"), "{all}");
        for secret in ["topsecret", "payroll", "Payroll", "hr_admin", "search_path"] {
            assert!(!all.contains(secret), "{secret} leaked");
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
