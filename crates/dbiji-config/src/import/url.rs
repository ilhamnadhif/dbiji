//! Connection URLs: `postgres://`, `mysql://`, `sqlite:`, JDBC URLs, plain
//! database file paths ... ([`parse_connection_url`]) and the reverse
//! ([`to_url`], never including the password).

use dbiji_core::config::{AuthMethod, ConnectionConfig, DbKind, SslMode};
use dbiji_core::{DbError, DbResult, Secret};

const FILE_DB_EXTENSIONS: &[(&str, DbKind)] = &[
    (".sqlite3", DbKind::Sqlite),
    (".sqlite", DbKind::Sqlite),
    (".db3", DbKind::Sqlite),
    (".db", DbKind::Sqlite),
];

/// Percent-decodes `s` (`+` is kept literally). Invalid escapes are kept as written.
pub fn percent_decode(s: &str) -> DbResult<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // `get` returns None past the end or inside a multi-byte character.
            let hex = s
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out)
        .map_err(|_| DbError::config("The URL contains percent-encoded bytes that are not UTF-8"))
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn kind_for_scheme(scheme: &str) -> Option<DbKind> {
    Some(match scheme {
        "postgres" | "postgresql" | "pg" | "pgsql" | "psql" => DbKind::Postgres,
        "cockroachdb" | "cockroach" | "crdb" => DbKind::CockroachDb,
        "mysql" | "mysqlx" | "mysql2" => DbKind::MySql,
        "mariadb" => DbKind::MariaDb,
        "sqlite" | "sqlite3" => DbKind::Sqlite,
        "oracle" | "oracledb" | "oracle+thin" => DbKind::Oracle,
        "cassandra" | "scylla" | "scylladb" | "cql" => DbKind::Cassandra,
        "redis" | "rediss" | "valkey" | "valkeys" => DbKind::Redis,
        "mongodb" | "mongodb+srv" | "mongo" => DbKind::MongoDb,
        "elasticsearch"
        | "elasticsearch+https"
        | "opensearch"
        | "opensearch+https"
        | "es"
        | "es+https" => DbKind::Elasticsearch,
        "neo4j" | "neo4j+s" | "neo4j+ssc" | "bolt" | "bolt+s" | "bolt+ssc" | "memgraph" => {
            DbKind::Neo4j
        }
        "sqlserver" | "mssql" | "microsoft" | "jtds" => DbKind::SqlServer,
        "clickhouse" | "ch" | "clickhouse+http" | "clickhouse+https" | "clickhouses" => {
            DbKind::ClickHouse
        }
        _ => return None,
    })
}

fn file_kind_for_path(path: &str) -> Option<DbKind> {
    let lower = path.to_ascii_lowercase();
    FILE_DB_EXTENSIONS
        .iter()
        .find(|(ext, _)| lower.ends_with(ext))
        .map(|(_, k)| *k)
}

fn looks_like_windows_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn file_config(kind: DbKind, path: String) -> ConnectionConfig {
    let name = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path.clone());
    let mut cfg = ConnectionConfig::new(kind, name);
    cfg.file_path = Some(path);
    cfg.auth = AuthMethod::None;
    cfg
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "required" | "require" => Some(true),
        "0" | "false" | "no" | "off" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

/// Parses a connection URL into a configuration and the password it carried.
///
/// Supported: `postgres(ql)://`, `cockroachdb://`, `mysql://`, `mariadb://`,
/// `sqlserver://` / `mssql://` (also with `;key=value` properties),
/// `clickhouse://`, `oracle://`, `cassandra://`, `redis(s)://`,
/// `mongodb(+srv)://`, `elasticsearch(+https)://`, `neo4j(+s)://` / `bolt://`,
/// `sqlite:///abs/path`,
/// `sqlite:relative`, `jdbc:` variants of all of them (`jdbc:postgresql://`,
/// `jdbc:mysql://`, `jdbc:mariadb://`, `jdbc:sqlite:path`,
/// `jdbc:sqlserver://h:p;databaseName=x;user=..;password=..`, `jdbc:clickhouse://`,
/// `jdbc:oracle:thin:@//host:port/service`, `jdbc:oracle:thin:@host:port:SID`)
/// and plain file paths ending in `.db`, `.sqlite`, `.sqlite3`, `.db3`. Recognised query parameters: `sslmode` /
/// `ssl-mode` / `sslMode`, `ssl` / `useSSL`, `sslrootcert`, `sslcert`,
/// `sslkey`, `application_name`, `options`, `connect_timeout`, `user`,
/// `password`, `dbname`, `currentSchema` / `search_path`; others become driver
/// properties. Percent-encoding and IPv6 literals in brackets are handled.
pub fn parse_connection_url(url: &str) -> DbResult<(ConnectionConfig, Option<Secret>)> {
    let s = url.trim();
    if s.is_empty() {
        return Err(DbError::config("The connection URL is empty"));
    }
    let lower = s.to_ascii_lowercase();
    let (body, jdbc) = match lower.strip_prefix("jdbc:") {
        Some(_) => (&s[5..], true),
        None => (s, false),
    };
    let scheme_end = body.find(':');
    let scheme = scheme_end.map(|i| body[..i].to_ascii_lowercase());
    let kind = scheme.as_deref().and_then(kind_for_scheme);
    let kind = match kind {
        Some(k) => k,
        None => {
            // A plain file path (Unix or Windows).
            if !jdbc
                && (scheme_end.is_none()
                    || looks_like_windows_path(s)
                    || s.starts_with('/')
                    || s.starts_with('.'))
                && let Some(k) = file_kind_for_path(s)
            {
                return Ok((file_config(k, s.to_string()), None));
            }
            return Err(DbError::config(format!(
                "Unrecognised connection URL '{}'",
                redact_url(s)
            )));
        }
    };
    let rest = &body[scheme_end.unwrap_or(0) + 1..];
    if kind.is_file_based() {
        return parse_file_url(kind, rest);
    }
    if kind == DbKind::Oracle && !rest.starts_with("//") {
        return parse_oracle_thin(rest);
    }
    // jTDS: jdbc:jtds:sqlserver://host:port/db;props
    let rest = if scheme.as_deref() == Some("jtds") {
        rest.split_once(':').map(|(_, r)| r).unwrap_or(rest)
    } else {
        rest
    };
    parse_network_url(kind, rest, scheme.as_deref().unwrap_or(""))
}

/// Where a `#fragment` starts: not a `#` inside `{…}` (JDBC `password={a#b}`) or
/// before the `@` of the host (an unencoded `#` in a password).
fn fragment_start(s: &str) -> usize {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '#' if depth == 0 && !s[i..].contains('@') => return i,
            _ => {}
        }
    }
    s.len()
}

fn parse_file_url(kind: DbKind, rest: &str) -> DbResult<(ConnectionConfig, Option<Secret>)> {
    let (path_part, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let path = if let Some(after) = path_part.strip_prefix("//") {
        // sqlite:///abs/path -> /abs/path ; sqlite:///C:/x -> C:/x ; sqlite://rel -> rel
        if after.len() >= 3 && after.starts_with('/') && looks_like_windows_path(&after[1..]) {
            after[1..].to_string()
        } else {
            after.to_string()
        }
    } else {
        path_part.to_string()
    };
    let path = percent_decode(&path)?;
    if path.is_empty() {
        return Err(DbError::config("The URL does not contain a database file"));
    }
    let mut cfg = file_config(kind, path);
    if let Some(q) = query {
        for pair in q.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = percent_decode(k)?;
            let v = percent_decode(v)?;
            match k.to_ascii_lowercase().as_str() {
                "mode" if v.eq_ignore_ascii_case("ro") => cfg.read_only = true,
                "readonly" | "read_only" | "access_mode" => {
                    if parse_bool(&v) == Some(true) || v.eq_ignore_ascii_case("read_only") {
                        cfg.read_only = true;
                    }
                }
                _ => {
                    cfg.properties.insert(k, v);
                }
            }
        }
    }
    Ok((cfg, None))
}

/// Oracle JDBC thin URLs after `jdbc:oracle:`:
/// `thin:[user/password]@[//]host[:port][/service]`, `thin:@host:port:SID`
/// and `thin:@(DESCRIPTION=...)` (kept whole as the `connect_descriptor`
/// property).
fn parse_oracle_thin(rest: &str) -> DbResult<(ConnectionConfig, Option<Secret>)> {
    let r = rest
        .strip_prefix("thin:")
        .or_else(|| rest.strip_prefix("oci:"))
        .unwrap_or(rest);
    let (creds, target) = r.rsplit_once('@').unwrap_or(("", r));
    let mut cfg = ConnectionConfig::new(DbKind::Oracle, String::new());
    let mut password = None;
    if !creds.is_empty() {
        let (u, p) = creds.split_once('/').unwrap_or((creds, ""));
        cfg.user = Some(u.to_string()).filter(|u| !u.is_empty());
        password = Some(Secret::from(p.to_string())).filter(|p| !p.is_empty());
    }
    let target = target.trim();
    if target.starts_with('(') {
        let value = |key: &str| {
            let upper = target.to_ascii_uppercase();
            let i = upper.find(&format!("({key}="))? + key.len() + 2;
            let end = target[i..].find(')')? + i;
            Some(target[i..end].trim().to_string())
        };
        cfg.host = value("HOST").unwrap_or_default();
        cfg.port = value("PORT").and_then(|p| p.parse().ok());
        cfg.database = value("SERVICE_NAME");
        cfg.properties
            .insert("connect_descriptor".into(), target.to_string());
    } else {
        let target = target.strip_prefix("//").unwrap_or(target);
        let (hostport, service) = match target.split_once('/') {
            Some((h, s)) => (h, Some(s)),
            None => (target, None),
        };
        let parts: Vec<&str> = hostport.split(':').collect();
        cfg.host = parts.first().copied().unwrap_or_default().to_string();
        cfg.port = parts.get(1).and_then(|p| p.parse().ok());
        if let Some(sid) = parts.get(2).filter(|s| !s.is_empty()) {
            cfg.properties.insert("sid".into(), sid.to_string());
        }
        cfg.database = service
            .map(|s| s.split('?').next().unwrap_or(s).to_string())
            .filter(|s| !s.is_empty());
    }
    if cfg.host.is_empty() {
        return Err(DbError::config("The URL does not contain a host"));
    }
    cfg.auth = AuthMethod::Password;
    cfg.name = match cfg.database.as_ref().or(cfg.properties.get("sid")) {
        Some(db) => format!("{db}@{}", cfg.host),
        None => cfg.host.clone(),
    };
    Ok((cfg, password))
}

fn parse_network_url(
    kind: DbKind,
    rest: &str,
    scheme: &str,
) -> DbResult<(ConnectionConfig, Option<Secret>)> {
    let rest = rest.strip_prefix("//").ok_or_else(|| {
        DbError::config("The URL must have the form scheme://host[:port]/database")
    })?;
    // Fragment is ignored.
    let rest = &rest[..fragment_start(rest)];
    // SQL Server style properties after ';'.
    let (main, semi_props) = match (kind, rest.split_once(';')) {
        (DbKind::SqlServer, Some((m, p))) => (m, Some(p)),
        _ => (rest, None),
    };
    let (before_query, query) = match main.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (main, None),
    };
    // Authority vs path. The '@' search is limited to the authority part
    // ends with the first '/' that comes after the last '@'.
    let at = before_query.rfind('@');
    let path_start = before_query[at.map(|i| i + 1).unwrap_or(0)..]
        .find('/')
        .map(|i| i + at.map(|a| a + 1).unwrap_or(0));
    let (authority, path) = match path_start {
        Some(i) => (&before_query[..i], &before_query[i + 1..]),
        None => (before_query, ""),
    };
    let (userinfo, hostlist) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let mut user: Option<String> = None;
    let mut password: Option<Secret> = None;
    if let Some(ui) = userinfo {
        match ui.split_once(':') {
            Some((u, p)) => {
                user = Some(percent_decode(u)?);
                password = Some(Secret::from(percent_decode(p)?));
            }
            None => user = Some(percent_decode(ui)?),
        }
    }
    let first_host = hostlist.split(',').next().unwrap_or("");
    let more_hosts = hostlist.contains(',') && matches!(kind, DbKind::MongoDb | DbKind::Cassandra);
    let (mut host, mut port) = split_host_port(first_host)?;
    let mut instance = None;
    if kind == DbKind::SqlServer
        && let Some((h, inst)) = host.split_once('\\')
    {
        instance = Some(inst.to_string());
        host = h.to_string();
    }
    let mut database = if path.is_empty() {
        None
    } else {
        Some(percent_decode(path.trim_end_matches('/'))?)
    };

    let mut cfg = ConnectionConfig::new(kind, String::new());
    let mut ssl_explicit = false;
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(q) = query {
        for pair in q.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            params.push((percent_decode(k)?, percent_decode(v)?));
        }
    }
    if let Some(p) = semi_props {
        params.extend(semi_properties(p));
    }
    let mut ssl_bool: Option<bool> = None;
    let mut trust_server_cert = false;
    for (k, v) in params {
        match k.to_ascii_lowercase().as_str() {
            "user" | "username" | "uid" | "user id" => user = Some(v),
            "password" | "pwd" => password = Some(Secret::from(v)),
            "dbname" | "database" | "databasename" | "initial catalog" => database = Some(v),
            "host" | "server" | "servername" if host.is_empty() => host = v,
            "port" | "portnumber" => port = v.parse().ok().or(port),
            "instance" | "instancename" => instance = Some(v),
            "sslmode" | "ssl-mode" | "ssl_mode" => {
                if let Some(m) = SslMode::parse(&v) {
                    cfg.ssl.mode = m;
                    ssl_explicit = true;
                }
            }
            "ssl" | "usessl" | "tls" | "secure" | "requiressl" | "encrypt" => {
                ssl_bool = parse_bool(&v).or(if v.eq_ignore_ascii_case("strict") {
                    Some(true)
                } else {
                    None
                });
            }
            "trustservercertificate" => trust_server_cert = parse_bool(&v).unwrap_or(false),
            "verifyservercertificate" => {
                if parse_bool(&v) == Some(true) && !ssl_explicit {
                    cfg.ssl.mode = SslMode::VerifyCa;
                    ssl_explicit = true;
                }
            }
            "sslrootcert" | "ssl-ca" | "sslca" | "ssl_ca" | "sslrootcertificate" => {
                cfg.ssl.ca_cert = Some(v)
            }
            "sslcert" | "ssl-cert" | "ssl_cert" => cfg.ssl.client_cert = Some(v),
            "sslkey" | "ssl-key" | "ssl_key" => cfg.ssl.client_key = Some(v),
            "application_name" | "applicationname" | "application name" | "program_name" => {
                cfg.properties.insert("application_name".into(), v);
            }
            "connect_timeout" | "connecttimeout" | "connectiontimeout" | "logintimeout"
            | "connection timeout" => {
                if let Ok(n) = v.trim().parse::<u64>() {
                    // MySQL Connector/J and ClickHouse JDBC use milliseconds.
                    let secs = if k == "connectTimeout"
                        && (kind.is_mysql_family() || kind == DbKind::ClickHouse)
                    {
                        n.div_ceil(1000)
                    } else {
                        n
                    };
                    cfg.init.connect_timeout_secs = u32::try_from(secs.max(1)).unwrap_or(u32::MAX);
                }
            }
            "currentschema" | "search_path" | "schema" => cfg.init.default_schema = Some(v),
            "readonly" | "read_only" | "read-only" => {
                if parse_bool(&v) == Some(true) {
                    cfg.read_only = true;
                }
            }
            _ => {
                cfg.properties.insert(k, v);
            }
        }
    }
    if !ssl_explicit {
        if let Some(b) = ssl_bool {
            cfg.ssl.mode = match (b, trust_server_cert, kind) {
                (false, _, _) => SslMode::Disable,
                (true, true, _) => SslMode::Require,
                (true, false, DbKind::SqlServer) => SslMode::VerifyFull,
                (true, false, _) => SslMode::Require,
            };
        } else if matches!(
            scheme,
            "clickhouse+https" | "clickhouses" | "rediss" | "valkeys" | "neo4j+ssc" | "bolt+ssc"
        ) {
            cfg.ssl.mode = SslMode::Require;
        } else if matches!(
            scheme,
            "mongodb+srv"
                | "neo4j+s"
                | "bolt+s"
                | "elasticsearch+https"
                | "opensearch+https"
                | "es+https"
        ) {
            cfg.ssl.mode = SslMode::VerifyFull;
        }
    }
    if scheme == "mongodb+srv" {
        cfg.properties.insert("srv".into(), "true".into());
    }
    if more_hosts {
        cfg.properties.insert("hosts".into(), hostlist.to_string());
    }
    if host.is_empty() {
        return Err(DbError::config("The URL does not contain a host"));
    }
    if let Some(i) = instance {
        cfg.properties.insert("instance".into(), i);
    }
    cfg.host = host;
    if port.is_some() {
        cfg.port = port;
    }
    if let Some(u) = user.filter(|u| !u.is_empty()) {
        cfg.user = Some(u);
    }
    if let Some(db) = database.filter(|d| !d.is_empty()) {
        cfg.database = Some(db);
    }
    cfg.auth = AuthMethod::Password;
    cfg.name = match &cfg.database {
        Some(db) => format!("{db}@{}", cfg.host),
        None => cfg.host.clone(),
    };
    let password = password.filter(|p| !p.is_empty());
    Ok((cfg, password))
}

fn split_host_port(s: &str) -> DbResult<(String, Option<u16>)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest
            .split_once(']')
            .ok_or_else(|| DbError::config("Unterminated IPv6 address in the URL"))?;
        let port = match after.strip_prefix(':') {
            Some(p) if !p.is_empty() => Some(parse_port(p)?),
            _ => None,
        };
        return Ok((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => {
            let port = if p.is_empty() {
                None
            } else {
                Some(parse_port(p)?)
            };
            Ok((percent_decode(h)?, port))
        }
        // Bare IPv6 without brackets: no port.
        Some(_) => Ok((s.to_string(), None)),
        None => Ok((percent_decode(s)?, None)),
    }
}

fn parse_port(p: &str) -> DbResult<u16> {
    match p.trim().parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(DbError::config(format!("Invalid port '{p}' in the URL"))),
    }
}

/// The URL with any password replaced by `***` (for messages).
pub fn redact_url(url: &str) -> String {
    let mut out = url.to_string();
    // user:password@host
    if let Some(scheme_end) = out.find("://") {
        let auth_start = scheme_end + 3;
        let auth_end = out[auth_start..]
            .find('/')
            .map(|i| i + auth_start)
            .unwrap_or(out.len());
        if let Some(at) = out[auth_start..auth_end].rfind('@').map(|i| i + auth_start)
            && let Some(colon) = out[auth_start..at].find(':').map(|i| i + auth_start)
        {
            out.replace_range(colon + 1..at, "***");
        }
    }
    // password=... in query / properties
    let lower = out.to_ascii_lowercase();
    let mut result = String::with_capacity(out.len());
    let mut i = 0;
    while let Some(pos) = lower[i..].find("password=").map(|p| p + i) {
        let value_start = pos + "password=".len();
        result.push_str(&out[i..value_start]);
        result.push_str("***");
        let value_end = out[value_start..]
            .find(['&', ';'])
            .map(|e| e + value_start)
            .unwrap_or(out.len());
        i = value_end;
    }
    result.push_str(&out[i..]);
    result
}

fn scheme_for(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "postgresql",
        DbKind::CockroachDb => "cockroachdb",
        DbKind::MySql => "mysql",
        DbKind::MariaDb => "mariadb",
        DbKind::SqlServer => "sqlserver",
        DbKind::ClickHouse => "clickhouse",
        DbKind::Sqlite => "sqlite",
        DbKind::Oracle => "oracle",
        DbKind::Cassandra => "cassandra",
        DbKind::Redis => "redis",
        DbKind::MongoDb => "mongodb",
        DbKind::Elasticsearch => "elasticsearch",
        DbKind::Neo4j => "neo4j",
        DbKind::Generic => "sql",
    }
}

/// URL of a configuration without the password (e.g. for copying to the clipboard).
pub fn to_url(cfg: &ConnectionConfig) -> String {
    let scheme = scheme_for(cfg.kind);
    if cfg.kind.is_file_based() {
        let path = cfg.file_path.clone().unwrap_or_default();
        let mut url = if path.starts_with('/') {
            format!("{scheme}://{}", encode_path(&path))
        } else if looks_like_windows_path(&path) {
            format!("{scheme}:///{}", encode_path(&path.replace('\\', "/")))
        } else {
            format!("{scheme}:{}", encode_path(&path))
        };
        if cfg.read_only {
            url.push_str("?mode=ro");
        }
        return url;
    }
    let mut url = format!("{scheme}://");
    if let Some(u) = cfg.user.as_deref().filter(|u| !u.is_empty()) {
        url.push_str(&percent_encode(u));
        url.push('@');
    }
    if cfg.host.contains(':') {
        url.push('[');
        url.push_str(&cfg.host);
        url.push(']');
    } else {
        url.push_str(&percent_encode(&cfg.host));
    }
    if let Some(p) = cfg.port {
        url.push_str(&format!(":{p}"));
    }
    if let Some(db) = cfg.database.as_deref().filter(|d| !d.is_empty()) {
        url.push('/');
        url.push_str(&percent_encode(db));
    }
    let mut params: Vec<(String, String)> = Vec::new();
    if cfg.ssl.mode != SslMode::default() {
        params.push(("sslmode".into(), cfg.ssl.mode.label().into()));
    }
    if let Some(v) = &cfg.ssl.ca_cert {
        params.push(("sslrootcert".into(), v.clone()));
    }
    if let Some(v) = &cfg.ssl.client_cert {
        params.push(("sslcert".into(), v.clone()));
    }
    if let Some(v) = &cfg.ssl.client_key {
        params.push(("sslkey".into(), v.clone()));
    }
    if let Some(v) = cfg.properties.get("application_name") {
        params.push(("application_name".into(), v.clone()));
    }
    if let Some(v) = cfg.properties.get("options") {
        params.push(("options".into(), v.clone()));
    }
    if cfg.init.connect_timeout_secs
        != dbiji_core::config::InitConfig::default().connect_timeout_secs
    {
        params.push((
            "connect_timeout".into(),
            cfg.init.connect_timeout_secs.to_string(),
        ));
    }
    if let Some(s) = &cfg.init.default_schema {
        params.push(("currentSchema".into(), s.clone()));
    }
    if cfg.read_only {
        params.push(("readonly".into(), "true".into()));
    }
    if !params.is_empty() {
        url.push('?');
        let q: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
            .collect();
        url.push_str(&q.join("&"));
    }
    url
}

/// `;key=value` properties of a SQL Server JDBC URL. A value in braces may hold
/// `;` and `=` (`password={p;w}`), `}}` being a literal `}`.
fn semi_properties(p: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = p;
    while !rest.is_empty() {
        let (key, after) = match rest.find(['=', ';']) {
            Some(i) if rest.as_bytes()[i] == b'=' => (&rest[..i], &rest[i + 1..]),
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let mut value = String::new();
        let tail;
        if let Some(braced) = after.trim_start().strip_prefix('{') {
            let mut chars = braced.char_indices();
            let mut close = "";
            while let Some((i, c)) = chars.next() {
                if c == '}' {
                    if braced[i + 1..].starts_with('}') {
                        chars.next();
                        value.push('}');
                    } else {
                        close = &braced[i + 1..];
                        break;
                    }
                } else {
                    value.push(c);
                }
            }
            tail = close.split_once(';').map_or("", |(_, r)| r);
        } else if !after.starts_with(';') {
            let (v, r) = after.split_once(';').unwrap_or((after, ""));
            value = v.trim().to_string();
            tail = r;
        } else {
            tail = &after[1..];
        }
        if !key.trim().is_empty() {
            out.push((key.trim().to_string(), value));
        }
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(u: &str) -> (ConnectionConfig, Option<Secret>) {
        parse_connection_url(u).unwrap_or_else(|e| panic!("{u}: {e}"))
    }

    #[test]
    fn hash_in_passwords_is_no_fragment() {
        let (c, pw) = parse("postgres://bob:a#b@h/db#ignored");
        assert_eq!(pw.unwrap().expose(), "a#b");
        assert_eq!(c.host, "h");
        assert_eq!(c.database.as_deref(), Some("db"));
        assert_eq!(fragment_start("h;password={x#y}"), "h;password={x#y}".len());
    }

    #[test]
    fn postgres_urls() {
        let (c, pw) = parse(
            "postgres://alice:p%40ss%3Aword@db.example.com:6543/my%20db?sslmode=verify-full&application_name=x&connect_timeout=7&options=-c%20search_path%3Dapp",
        );
        assert_eq!(c.kind, DbKind::Postgres);
        assert_eq!(c.user.as_deref(), Some("alice"));
        assert_eq!(pw.unwrap().expose(), "p@ss:word");
        assert_eq!(c.host, "db.example.com");
        assert_eq!(c.port, Some(6543));
        assert_eq!(c.database.as_deref(), Some("my db"));
        assert_eq!(c.ssl.mode, SslMode::VerifyFull);
        assert_eq!(
            c.properties.get("application_name").map(String::as_str),
            Some("x")
        );
        assert_eq!(
            c.properties.get("options").map(String::as_str),
            Some("-c search_path=app")
        );
        assert_eq!(c.init.connect_timeout_secs, 7);
        assert_eq!(c.auth, AuthMethod::Password);

        let (c, pw) = parse("postgresql://localhost");
        assert_eq!(c.port, Some(5432));
        assert_eq!(c.user.as_deref(), Some("postgres"), "kind default user");
        assert!(pw.is_none());

        let (c, _) =
            parse("postgresql://[::1]:5433/db?sslrootcert=/ca.pem&sslcert=/c.pem&sslkey=/k.pem");
        assert_eq!(c.host, "::1");
        assert_eq!(c.port, Some(5433));
        assert_eq!(c.ssl.ca_cert.as_deref(), Some("/ca.pem"));
        assert_eq!(c.ssl.client_key.as_deref(), Some("/k.pem"));

        // Multiple hosts: the first is used.
        let (c, _) = parse("postgresql://h1:5432,h2:5433/app");
        assert_eq!((c.host.as_str(), c.port), ("h1", Some(5432)));

        let (c, pw) = parse(
            "jdbc:postgresql://pg:5432/shop?user=bob&password=secret&sslmode=require&currentSchema=sales",
        );
        assert_eq!(c.user.as_deref(), Some("bob"));
        assert_eq!(pw.unwrap().expose(), "secret");
        assert_eq!(c.ssl.mode, SslMode::Require);
        assert_eq!(c.init.default_schema.as_deref(), Some("sales"));

        let (c, _) = parse("cockroachdb://root@crdb:26257/defaultdb?sslmode=disable");
        assert_eq!(c.kind, DbKind::CockroachDb);
        assert_eq!(c.ssl.mode, SslMode::Disable);
    }

    #[test]
    fn mysql_and_mariadb() {
        let (c, pw) = parse("mysql://root:pw@127.0.0.1:3307/app?ssl-mode=REQUIRED&charset=utf8mb4");
        assert_eq!(c.kind, DbKind::MySql);
        assert_eq!(c.port, Some(3307));
        assert_eq!(pw.unwrap().expose(), "pw");
        assert_eq!(c.ssl.mode, SslMode::Require);
        assert_eq!(
            c.properties.get("charset").map(String::as_str),
            Some("utf8mb4")
        );
        let (c, _) = parse("mariadb://u@maria/db?ssl-mode=VERIFY_IDENTITY");
        assert_eq!(c.kind, DbKind::MariaDb);
        assert_eq!(c.port, Some(3306));
        assert_eq!(c.ssl.mode, SslMode::VerifyFull);
        let (c, _) = parse("jdbc:mysql://h:3306/x?useSSL=false&connectTimeout=5000");
        assert_eq!(c.ssl.mode, SslMode::Disable);
        assert_eq!(c.init.connect_timeout_secs, 5);
        let (c, _) = parse("jdbc:mariadb://h/x?useSSL=true");
        assert_eq!(c.kind, DbKind::MariaDb);
        assert_eq!(c.ssl.mode, SslMode::Require);
    }

    #[test]
    fn sqlserver_forms() {
        let (c, pw) = parse(
            "jdbc:sqlserver://sql.corp:1444;databaseName=Sales;user=sa;password=Pa;ss;encrypt=true;trustServerCertificate=true",
        );
        assert_eq!(c.kind, DbKind::SqlServer);
        assert_eq!(c.host, "sql.corp");
        assert_eq!(c.port, Some(1444));
        assert_eq!(c.database.as_deref(), Some("Sales"));
        assert_eq!(c.user.as_deref(), Some("sa"));
        // ';' ends the property value (as in the JDBC driver).
        assert_eq!(pw.unwrap().expose(), "Pa");
        let (_, braced) = parse("jdbc:sqlserver://h;user=sa;password={p;w}}x};encrypt=true");
        assert_eq!(braced.unwrap().expose(), "p;w}x");
        let (c2, braced) = parse("jdbc:sqlserver://h;password={x};databaseName=Db");
        assert_eq!(braced.unwrap().expose(), "x");
        assert_eq!(c2.database.as_deref(), Some("Db"));
        assert_eq!(c.ssl.mode, SslMode::Require);
        let (c, _) = parse("sqlserver://sa:x@srv\\SQLEXPRESS/db?encrypt=true");
        assert_eq!(c.host, "srv");
        assert_eq!(
            c.properties.get("instance").map(String::as_str),
            Some("SQLEXPRESS")
        );
        assert_eq!(c.ssl.mode, SslMode::VerifyFull);
        let (c, _) = parse("mssql://sa@host:1433/master");
        assert_eq!(c.kind, DbKind::SqlServer);
        let (c, _) = parse("jdbc:jtds:sqlserver://legacy:1433/old");
        assert_eq!((c.kind, c.host.as_str()), (DbKind::SqlServer, "legacy"));
    }

    #[test]
    fn clickhouse_urls() {
        let (c, _) = parse("clickhouse://default:@ch.local:8123/logs");
        assert_eq!(c.kind, DbKind::ClickHouse);
        assert_eq!(c.database.as_deref(), Some("logs"));
        let (c, _) = parse("clickhouses://ch.local/logs");
        assert_eq!(c.ssl.mode, SslMode::Require);
        let (c, _) = parse("jdbc:clickhouse://ch:8443/db?ssl=true");
        assert_eq!(c.ssl.mode, SslMode::Require);
        assert!(parse_connection_url("duckdb:///data/analytics.duckdb").is_err());
    }

    #[test]
    fn oracle_and_nosql_urls() {
        let (c, pw) = parse("oracle://scott:tiger@ora.local:1521/FREEPDB1");
        assert_eq!(
            (c.kind, c.host.as_str(), c.port, c.database.as_deref()),
            (DbKind::Oracle, "ora.local", Some(1521), Some("FREEPDB1"))
        );
        assert_eq!(
            (c.user.as_deref(), pw.as_ref().map(|p| p.expose())),
            (Some("scott"), Some("tiger"))
        );
        let (c, _) = parse("jdbc:oracle:thin:@//db.example.com:1522/sales.example.com");
        assert_eq!(
            (c.host.as_str(), c.port, c.database.as_deref()),
            ("db.example.com", Some(1522), Some("sales.example.com"))
        );
        let (c, pw) = parse("jdbc:oracle:thin:hr/secret@db:1521:ORCL");
        assert_eq!(
            (c.host.as_str(), c.database.as_deref(), c.user.as_deref()),
            ("db", None, Some("hr"))
        );
        assert_eq!(c.properties.get("sid").map(String::as_str), Some("ORCL"));
        assert_eq!(pw.as_ref().map(|p| p.expose()), Some("secret"));
        let (c, _) = parse(
            "jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h1)(PORT=1530))(CONNECT_DATA=(SERVICE_NAME=svc)))",
        );
        assert_eq!(
            (c.host.as_str(), c.port, c.database.as_deref()),
            ("h1", Some(1530), Some("svc"))
        );
        assert!(c.properties.contains_key("connect_descriptor"));

        let (c, _) = parse("cassandra://cass:pw@n1:9042,n2:9042/shop");
        assert_eq!(
            (c.kind, c.host.as_str(), c.database.as_deref()),
            (DbKind::Cassandra, "n1", Some("shop"))
        );
        assert_eq!(
            c.properties.get("hosts").map(String::as_str),
            Some("n1:9042,n2:9042")
        );
        let (c, pw) = parse("rediss://:s3cret@cache.example.com:6380/2");
        assert_eq!(
            (c.kind, c.port, c.database.as_deref(), c.user.as_deref()),
            (DbKind::Redis, Some(6380), Some("2"), None)
        );
        assert_eq!(c.ssl.mode, SslMode::Require);
        assert_eq!(pw.as_ref().map(|p| p.expose()), Some("s3cret"));
        let (c, _) = parse(
            "mongodb+srv://app:pw@cluster0.abcde.mongodb.net/shop?authSource=admin&retryWrites=true",
        );
        assert_eq!(
            (c.kind, c.host.as_str(), c.database.as_deref()),
            (DbKind::MongoDb, "cluster0.abcde.mongodb.net", Some("shop"))
        );
        assert_eq!(c.properties.get("srv").map(String::as_str), Some("true"));
        assert_eq!(
            c.properties.get("authSource").map(String::as_str),
            Some("admin")
        );
        assert_eq!(c.ssl.mode, SslMode::VerifyFull);
        let (c, _) = parse("neo4j+s://abc.databases.neo4j.io");
        assert_eq!((c.kind, c.ssl.mode), (DbKind::Neo4j, SslMode::VerifyFull));
        let (c, _) = parse("opensearch+https://admin@search.local:9200");
        assert_eq!(
            (c.kind, c.ssl.mode, c.user.as_deref()),
            (DbKind::Elasticsearch, SslMode::VerifyFull, Some("admin"))
        );
    }

    #[test]
    fn sqlite_and_files() {
        let (c, _) = parse("sqlite:///var/data/app.db");
        assert_eq!(c.kind, DbKind::Sqlite);
        assert_eq!(c.file_path.as_deref(), Some("/var/data/app.db"));
        assert_eq!(c.name, "app.db");
        let (c, _) = parse("sqlite:relative/dir/x.sqlite?mode=ro");
        assert_eq!(c.file_path.as_deref(), Some("relative/dir/x.sqlite"));
        assert!(c.read_only);
        let (c, _) = parse("sqlite://rel.db");
        assert_eq!(c.file_path.as_deref(), Some("rel.db"));
        let (c, _) = parse("sqlite:///C:/data/x.db");
        assert_eq!(c.file_path.as_deref(), Some("C:/data/x.db"));
        let (c, _) = parse("jdbc:sqlite:/home/me/my%20db.sqlite3");
        assert_eq!(c.file_path.as_deref(), Some("/home/me/my db.sqlite3"));
        let (c, _) = parse("jdbc:sqlite::memory:");
        assert_eq!(c.file_path.as_deref(), Some(":memory:"));
        let (c, _) = parse("/Users/me/chinook.db");
        assert_eq!(c.kind, DbKind::Sqlite);
        assert!(parse_connection_url("C:\\data\\warehouse.duckdb").is_err());
        let (c, _) = parse("./local.sqlite3");
        assert_eq!(c.file_path.as_deref(), Some("./local.sqlite3"));
    }

    #[test]
    fn errors_do_not_leak_passwords() {
        let e = parse_connection_url("db2://scott:tiger@db2:50000/sample").unwrap_err();
        assert!(e.message.contains("scott:***@db2"), "{}", e.message);
        assert!(!e.message.contains("tiger"));
        assert!(parse_connection_url("").is_err());
        assert!(parse_connection_url("postgres://u@:5432/db").is_err());
        assert!(parse_connection_url("postgres://h:99999/db").is_err());
        assert!(parse_connection_url("postgres:nohost").is_err());
        assert!(parse_connection_url("mysql://[::1/db").is_err());
        assert_eq!(
            redact_url("jdbc:sqlserver://h;user=sa;password=abc;x=1"),
            "jdbc:sqlserver://h;user=sa;password=***;x=1"
        );
    }

    #[test]
    fn to_url_roundtrip_without_password() {
        let (c, pw) = parse(
            "postgresql://alice:hunter2@db.example.com:6543/my%20db?sslmode=verify-full&application_name=x",
        );
        assert!(pw.is_some());
        let url = to_url(&c);
        assert!(!url.contains("hunter2"));
        assert!(
            url.starts_with("postgresql://alice@db.example.com:6543/my%20db?"),
            "{url}"
        );
        let (back, pw2) = parse(&url);
        assert!(pw2.is_none());
        assert_eq!(
            (
                back.kind,
                back.host,
                back.port,
                back.database,
                back.user,
                back.ssl.mode
            ),
            (
                c.kind,
                c.host.clone(),
                c.port,
                c.database.clone(),
                c.user.clone(),
                c.ssl.mode
            )
        );
        assert_eq!(
            back.properties.get("application_name"),
            c.properties.get("application_name")
        );

        let mut ipv6 = ConnectionConfig::new(DbKind::MySql, "x");
        ipv6.host = "fe80::1".into();
        let u = to_url(&ipv6);
        assert!(u.starts_with("mysql://root@[fe80::1]:3306"), "{u}");
        assert_eq!(parse(&u).0.host, "fe80::1");

        let (file, _) = parse("sqlite:///var/data/my app.db");
        let u = to_url(&file);
        assert_eq!(u, "sqlite:///var/data/my%20app.db");
        assert_eq!(parse(&u).0.file_path, file.file_path);
        let mut rel = ConnectionConfig::new(DbKind::Sqlite, "d");
        rel.file_path = Some("data/x.db".into());
        rel.read_only = true;
        assert_eq!(to_url(&rel), "sqlite:data/x.db?mode=ro");
    }

    #[test]
    fn percent_coding() {
        assert_eq!(percent_decode("a%20b%2Fc").unwrap(), "a b/c");
        assert_eq!(percent_decode("100%").unwrap(), "100%");
        assert_eq!(percent_decode("%zz").unwrap(), "%zz");
        assert_eq!(percent_decode("caf%C3%A9").unwrap(), "café");
        assert!(percent_decode("%FF").is_err());
        assert_eq!(percent_encode("a b@c"), "a%20b%40c");
    }
}
