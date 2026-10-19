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

    #[test]
    fn urls_and_count() {
        assert_eq!(search_url("pg_stat_activity wait_event"), "https://www.google.com/search?q=pg_stat_activity+wait_event");
        assert_eq!(search_url("a&b=c"), "https://www.google.com/search?q=a%26b%3Dc");
        assert_eq!(row_count_sql("select * from t;\n"), "SELECT COUNT(*) AS row_count FROM (\nselect * from t\n) dbiji_count");
    }
}
