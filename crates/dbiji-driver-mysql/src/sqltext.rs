//! Small lexical helpers over MySQL statement text: comment / literal aware
//! scanning used for parameter checks, statement classification, LIMIT
//! insertion and view definition normalisation.

/// Does `sql` contain `:name` style named parameters according to the exact
/// rules of `mysql_common::named_params::ParsedNamedParams::parse`?
///
/// `mysql_async` rewrites such parameters for prepared statements and fails
/// *inside* its execute routine (closing the connection) when positional
/// values are supplied, so the driver rejects these statements up front.
/// This is a faithful port of that state machine (including its quirks).
pub fn has_named_params(sql: &str) -> bool {
    #[derive(Clone, Copy)]
    enum S {
        Top,
        Str(u8, bool),
        MaybeNamed,
        Named,
        Sharp,
        MaybeDash1,
        MaybeDash2,
        Dash,
        MaybeC1,
        MaybeC2,
        C,
        MaybeExitC,
        Quoted,
    }
    let mut state = S::Top;
    let mut found = false;
    for &c in sql.as_bytes() {
        let mut rematch = false;
        match state {
            S::Top => match c {
                b':' => state = S::MaybeNamed,
                b'/' => state = S::MaybeC1,
                b'-' => state = S::MaybeDash1,
                b'#' => state = S::Sharp,
                b'\'' => state = S::Str(b'\'', false),
                b'"' => state = S::Str(b'"', false),
                b'`' => state = S::Quoted,
                _ => {}
            },
            S::Str(sep, esc) => {
                state = if esc {
                    S::Str(sep, false)
                } else if c == sep {
                    S::Top
                } else if c == b'\\' {
                    S::Str(sep, true)
                } else {
                    S::Str(sep, false)
                }
            }
            S::MaybeNamed => match c {
                b'a'..=b'z' | b'_' => {
                    found = true;
                    state = S::Named;
                }
                _ => rematch = true,
            },
            S::Named => {
                if !matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_') {
                    rematch = true;
                }
            }
            S::Sharp => {
                if c == b'\n' {
                    state = S::Top
                }
            }
            S::MaybeDash1 => state = if c == b'-' { S::MaybeDash2 } else { S::Top },
            S::MaybeDash2 => {
                state = if c.is_ascii_whitespace() && c != b'\n' {
                    S::Dash
                } else {
                    S::Top
                }
            }
            S::Dash => {
                if c == b'\n' {
                    state = S::Top
                }
            }
            S::MaybeC1 => state = if c == b'*' { S::MaybeC2 } else { S::Top },
            S::MaybeC2 => state = if c == b'!' || c == b'+' { S::Top } else { S::C },
            S::C => {
                if c == b'*' {
                    state = S::MaybeExitC
                }
            }
            S::MaybeExitC => state = if c == b'/' { S::Top } else { S::C },
            S::Quoted => {
                if c == b'`' {
                    state = S::Top
                }
            }
        }
        if rematch {
            state = match c {
                b':' => S::MaybeNamed,
                b'\'' => S::Str(b'\'', false),
                b'"' => S::Str(b'"', false),
                _ => S::Top,
            };
        }
    }
    found
}

/// Token produced by [`tokens`]: words (upper-cased keywords/identifiers),
/// punctuation, and a marker for everything else (literals, quoted names).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// Unquoted word, upper-cased.
    Word(String),
    /// Quoted identifier (backticks), unquoted content.
    QuotedIdent(String),
    /// String / number / other literal.
    Literal,
    /// Single punctuation character.
    Punct(char),
}

/// Tokens of `sql` with their nesting depth (parentheses). Comments are skipped.
/// Good enough for statement classification; not a full parser.
pub fn tokens(sql: &str) -> Vec<(Tok, usize)> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut depth = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'#'
            || (c == b'-'
                && b.get(i + 1) == Some(&b'-')
                && b.get(i + 2).is_none_or(|x| x.is_ascii_whitespace()))
        {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            // `/*! ... */` executable comments contain real SQL: scan their content.
            if b.get(i + 2) == Some(&b'!') {
                i += 3;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                continue;
            }
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c == b'*' && b.get(i + 1) == Some(&b'/') {
            // end of an executable comment
            i += 2;
        } else if c == b'\'' || c == b'"' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push((Tok::Literal, depth));
        } else if c == b'`' {
            let start = i + 1;
            let mut s = String::new();
            i += 1;
            let mut seg = start;
            while i < b.len() {
                if b[i] == b'`' {
                    if b.get(i + 1) == Some(&b'`') {
                        s.push_str(&sql[seg..=i]);
                        i += 2;
                        seg = i;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            s.push_str(&sql[seg.min(b.len())..i.min(b.len())]);
            i += 1;
            out.push((Tok::QuotedIdent(s), depth));
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            out.push((Tok::Word(sql[start..i].to_ascii_uppercase()), depth));
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            out.push((Tok::Literal, depth));
        } else {
            if c == b'(' {
                out.push((Tok::Punct('('), depth));
                depth += 1;
            } else if c == b')' {
                depth = depth.saturating_sub(1);
                out.push((Tok::Punct(')'), depth));
            } else {
                out.push((Tok::Punct(c as char), depth));
            }
            i += 1;
        }
    }
    out
}

/// First keyword of the statement (upper case), skipping comments and
/// leading parentheses.
pub fn leading_keyword(sql: &str) -> Option<String> {
    tokens(sql).into_iter().find_map(|(t, _)| match t {
        Tok::Word(w) => Some(w),
        _ => None,
    })
}

/// The statement only reads data (SELECT / WITH / TABLE / VALUES / SHOW ...).
pub fn is_read_only_statement(sql: &str) -> bool {
    match leading_keyword(sql).as_deref() {
        Some("SELECT" | "TABLE" | "VALUES" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "HELP") => {
            // SELECT ... INTO OUTFILE / FOR UPDATE still only reads table data.
            true
        }
        Some("WITH") => {
            // WITH ... UPDATE/DELETE is possible in MySQL 8.
            !tokens(sql).iter().any(|(t, d)| *d == 0 && matches!(t, Tok::Word(w) if w == "UPDATE" || w == "DELETE" || w == "INSERT" || w == "REPLACE"))
        }
        _ => false,
    }
}

/// The statement is DDL that causes an implicit commit in MySQL / MariaDB.
pub fn causes_implicit_commit(sql: &str) -> bool {
    let toks = tokens(sql);
    let mut words = toks.iter().filter_map(|(t, _)| match t {
        Tok::Word(w) => Some(w.as_str()),
        _ => None,
    });
    match words.next() {
        Some(
            "ALTER" | "DROP" | "RENAME" | "TRUNCATE" | "GRANT" | "REVOKE" | "ANALYZE" | "OPTIMIZE"
            | "REPAIR" | "CHECK" | "CACHE" | "FLUSH" | "LOCK" | "UNLOCK" | "LOAD" | "INSTALL"
            | "UNINSTALL",
        ) => true,
        Some("CREATE") => {
            // CREATE TEMPORARY TABLE does not commit.
            !matches!(words.next(), Some("TEMPORARY"))
        }
        Some("BEGIN" | "START" | "COMMIT" | "ROLLBACK") => true,
        _ => false,
    }
}

/// Is there a keyword `kw` (upper case) at parenthesis depth 0?
pub fn has_top_level_word(sql: &str, kw: &str) -> bool {
    tokens(sql)
        .iter()
        .any(|(t, d)| *d == 0 && matches!(t, Tok::Word(w) if w == kw))
}

/// Remove one pair of parentheses enclosing the whole expression:
/// `(a + b)` → `a + b`; `(a) + (b)` stays unchanged.
pub fn strip_outer_parens(expr: &str) -> &str {
    let t = expr.trim();
    if !(t.starts_with('(') && t.ends_with(')')) {
        return t;
    }
    let b = t.as_bytes();
    let mut depth = 0i32;
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' && q != b'`' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 && i != b.len() - 1 {
                        return t;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    if depth == 0 {
        t[1..t.len() - 1].trim()
    } else {
        t
    }
}

/// Remove `` `schema`. `` qualifiers (outside string literals) from a view
/// definition, the way `SHOW CREATE VIEW` prints objects of the view's own
/// database.
pub fn strip_schema_qualifier(sql: &str, schema: &str) -> String {
    let quoted = format!("`{}`.", schema.replace('`', "``"));
    let b = sql.as_bytes();
    let q = quoted.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    let mut last = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if c == b'`' {
            if b[i..].starts_with(q) && (i == 0 || b[i - 1] != b'.') {
                out.push_str(&sql[last..i]);
                i += q.len();
                last = i;
                continue;
            }
            // skip a quoted identifier
            i += 1;
            while i < b.len() {
                if b[i] == b'`' {
                    if b.get(i + 1) == Some(&b'`') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    out.push_str(&sql[last.min(sql.len())..]);
    out
}

/// Command tag of a single statement (`CREATE TABLE`, `INSERT`, `SET` ...),
/// used when the server sends no info text. `None` for scripts with several
/// statements.
pub fn command_tag(sql: &str) -> Option<String> {
    if split_statements(sql).len() != 1 {
        return None;
    }
    const OBJECTS: &[&str] = &[
        "TABLE",
        "VIEW",
        "INDEX",
        "PROCEDURE",
        "FUNCTION",
        "TRIGGER",
        "EVENT",
        "DATABASE",
        "SCHEMA",
        "USER",
        "ROLE",
        "SEQUENCE",
        "TABLESPACE",
        "SERVER",
    ];
    let words: Vec<String> = tokens(sql)
        .into_iter()
        .take(12)
        .filter_map(|(t, d)| match t {
            Tok::Word(w) if d == 0 => Some(w),
            _ => None,
        })
        .collect();
    let first = words.first()?.clone();
    match first.as_str() {
        "CREATE" | "DROP" | "ALTER" | "RENAME" | "TRUNCATE" => {
            match words.iter().skip(1).find(|w| OBJECTS.contains(&w.as_str())) {
                Some(obj) => Some(format!("{first} {obj}")),
                None => Some(first),
            }
        }
        _ => Some(first),
    }
}

/// Split a script into statements on `;` outside literals and comments
/// (used for connection init SQL). Empty statements are dropped.
pub fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && c != b'`' {
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if c == b'#' || (c == b'-' && b.get(i + 1) == Some(&b'-')) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c == b';' {
            let s = sql[start..i].trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    let s = sql[start.min(sql.len())..].trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_params_like_mysql_common() {
        assert!(has_named_params("SELECT :a"));
        assert!(has_named_params("SELECT * FROM t WHERE a = :foo_1"));
        assert!(!has_named_params("SELECT ':a'"));
        assert!(!has_named_params("SELECT \":a\""));
        assert!(!has_named_params("SELECT `:a`"));
        assert!(!has_named_params("SELECT 1 -- :a\n"));
        assert!(!has_named_params("SELECT 1 # :a"));
        assert!(!has_named_params("SELECT /* :a */ 1"));
        assert!(!has_named_params("SET @x := 1"));
        assert!(!has_named_params("SELECT ?"));
        assert!(!has_named_params("SELECT :A")); // upper case does not start a name
        assert!(has_named_params("SELECT /*+ hint */ :a")); // optimizer hints are not comments for the parser
        assert!(!has_named_params("SELECT 'it\\'s :a'"));
    }

    #[test]
    fn keywords() {
        assert_eq!(
            leading_keyword("  /* c */ (select 1)").as_deref(),
            Some("SELECT")
        );
        assert_eq!(
            leading_keyword("-- x\nupdate t set a=1").as_deref(),
            Some("UPDATE")
        );
        assert!(is_read_only_statement(
            "WITH x AS (SELECT 1) SELECT * FROM x"
        ));
        assert!(!is_read_only_statement(
            "WITH x AS (SELECT 1) UPDATE t JOIN x SET a=1"
        ));
        assert!(!is_read_only_statement("DELETE FROM t"));
        assert!(causes_implicit_commit("CREATE TABLE t (a int)"));
        assert!(!causes_implicit_commit("CREATE TEMPORARY TABLE t (a int)"));
        assert!(!causes_implicit_commit("INSERT INTO t VALUES (1)"));
        assert!(has_top_level_word("SELECT * FROM t LIMIT 5", "LIMIT"));
        assert!(!has_top_level_word(
            "SELECT * FROM (SELECT * FROM t LIMIT 5) x",
            "LIMIT"
        ));
        assert!(!has_top_level_word("SELECT 'LIMIT' FROM t", "LIMIT"));
        assert!(has_top_level_word("SELECT * FROM t FOR UPDATE", "FOR"));
    }

    #[test]
    fn parens() {
        assert_eq!(strip_outer_parens("(`a` * 2)"), "`a` * 2");
        assert_eq!(strip_outer_parens("(a) + (b)"), "(a) + (b)");
        assert_eq!(strip_outer_parens("((a > 0))"), "(a > 0)");
        assert_eq!(strip_outer_parens("a > 0"), "a > 0");
        assert_eq!(strip_outer_parens("(a = ')')"), "a = ')'");
    }

    #[test]
    fn schema_qualifiers() {
        let def =
            "select `db`.`a`.`id` AS `id`,'`db`.x' AS `s` from `db`.`a` join `other`.`db` on 1";
        assert_eq!(
            strip_schema_qualifier(def, "db"),
            "select `a`.`id` AS `id`,'`db`.x' AS `s` from `a` join `other`.`db` on 1"
        );
    }

    #[test]
    fn tags() {
        assert_eq!(
            command_tag("create table t (a int)").as_deref(),
            Some("CREATE TABLE")
        );
        assert_eq!(
            command_tag("CREATE OR REPLACE ALGORITHM=MERGE VIEW v AS SELECT 1").as_deref(),
            Some("CREATE VIEW")
        );
        assert_eq!(
            command_tag("CREATE UNIQUE INDEX i ON t (a)").as_deref(),
            Some("CREATE INDEX")
        );
        assert_eq!(
            command_tag("drop table if exists t;").as_deref(),
            Some("DROP TABLE")
        );
        assert_eq!(command_tag("TRUNCATE t").as_deref(), Some("TRUNCATE"));
        assert_eq!(command_tag("SET @a = 1").as_deref(), Some("SET"));
        assert_eq!(
            command_tag("insert into t values (1)").as_deref(),
            Some("INSERT")
        );
        assert_eq!(command_tag("SELECT 1; SELECT 2"), None);
    }

    #[test]
    fn split() {
        assert_eq!(
            split_statements("SET a=1; SET b=';'; -- c;\n SELECT 2"),
            vec![
                "SET a=1".to_string(),
                "SET b=';'".to_string(),
                "-- c;\n SELECT 2".to_string()
            ]
        );
        assert!(split_statements(" ; ;").is_empty());
    }
}
