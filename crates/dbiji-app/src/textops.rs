//! Text transformations of the SQL editor (DBeaver's "Morph to delimited
//! list", "Trim spaces", "Search in web").

/// The non-empty lines of `text` as a comma separated list: numbers as they
/// are, everything else as quoted strings (`'` doubled).
pub fn morph_to_delimited(text: &str) -> String {
    let items: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| l.trim_end_matches(',')).collect();
    let numeric = !items.is_empty() && items.iter().all(|i| i.parse::<f64>().is_ok());
    items.iter().map(|i| if numeric { i.to_string() } else { format!("'{}'", i.trim_matches('\'').replace('\'', "''")) }).collect::<Vec<_>>().join(", ")
}

/// `text` without spaces and tabs at the end of its lines.
pub fn trim_trailing(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let (body, cr) = match line.strip_suffix('\r') {
            Some(b) => (b, true),
            None => (line, false),
        };
        out.push_str(body.trim_end_matches([' ', '\t']));
        if cr {
            out.push('\r');
        }
    }
    out
}

/// A web search URL for `query`.
pub fn search_url(query: &str) -> String {
    let mut q = String::new();
    for b in query.trim().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => q.push(b as char),
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    format!("https://www.google.com/search?q={q}")
}

/// Run an external formatter: `command` (through the shell) gets `sql` on
/// stdin and must print the formatted SQL; at most 10 seconds.
pub fn run_formatter(command: &str, sql: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::Command;
    let mut child =
        if cfg!(windows) { Command::new("cmd").args(["/C", command]).spawn_piped() } else { Command::new("sh").args(["-c", command]).spawn_piped() }.map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let input = sql.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    // Read while it runs: a formatter blocks once its output fills the pipe.
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                return Err("the formatter did not finish within 10 seconds".into());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    let _ = writer.join();
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err).trim().to_string();
        return Err(if err.is_empty() { format!("exit status {status}") } else { err });
    }
    String::from_utf8(out).map_err(|_| "the formatter printed invalid UTF-8".to_string())
}

trait SpawnPiped {
    fn spawn_piped(&mut self) -> std::io::Result<std::process::Child>;
}

impl SpawnPiped for std::process::Command {
    fn spawn_piped(&mut self) -> std::io::Result<std::process::Child> {
        use std::process::Stdio;
        self.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
    }
}

/// `stmt` wrapped to count its rows.
pub fn row_count_sql(stmt: &str) -> String {
    let body = stmt.trim().trim_end_matches(';').trim_end();
    format!("SELECT COUNT(*) AS row_count FROM (\n{body}\n) dbiji_count")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morph() {
        assert_eq!(morph_to_delimited("Ani\nBudi\n\n  O'Neil  \n"), "'Ani', 'Budi', 'O''Neil'");
        assert_eq!(morph_to_delimited("1\n2,\n 3.5"), "1, 2, 3.5");
        assert_eq!(morph_to_delimited("'a'\n'b'"), "'a', 'b'");
        assert_eq!(morph_to_delimited(""), "");
    }

    #[test]
    fn trim() {
        assert_eq!(trim_trailing("select 1;  \n\tfrom t\t\r\nx "), "select 1;\n\tfrom t\r\nx");
    }

    #[cfg(unix)]
    #[test]
    fn external_formatter() {
        assert_eq!(run_formatter("tr a-z A-Z", "select 1 from t").unwrap(), "SELECT 1 FROM T");
        assert!(run_formatter("exit 3", "x").is_err());
        assert!(run_formatter("echo bad >&2; exit 1", "x").unwrap_err().contains("bad"));
    }

    #[test]
    fn urls_and_count() {
        assert_eq!(search_url("pg_stat_activity wait_event"), "https://www.google.com/search?q=pg_stat_activity+wait_event");
        assert_eq!(search_url("a&b=c"), "https://www.google.com/search?q=a%26b%3Dc");
        assert_eq!(row_count_sql("select * from t;\n"), "SELECT COUNT(*) AS row_count FROM (\nselect * from t\n) dbiji_count");
    }
}
