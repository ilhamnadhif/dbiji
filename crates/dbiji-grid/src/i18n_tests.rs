//! Keeps `locales/id.tsv` complete: every `tr()` / `trf()` key used in the
//! crate must have an Indonesian translation with the same placeholders.

use std::collections::HashMap;
use std::path::Path;

fn unescape_rust(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\n') => {
                while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                    chars.next();
                }
            }
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

fn unescape_tsv(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\t", "\t").replace("\\\\", "\\")
}

fn source_keys(dir: &Path, out: &mut Vec<(String, String)>) {
    let re = regex::Regex::new(r#"(?s)\btrf?\(\s*"((?:[^"\\]|\\.)*)""#).expect("valid regex");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            source_keys(&p, out);
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".rs") || name.contains("tests") || name == "test_support.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        let text = match text.find("#[cfg(test)]\nmod tests") {
            Some(i) => &text[..i],
            None => &text[..],
        };
        for cap in re.captures_iter(text) {
            out.push((unescape_rust(&cap[1]), p.display().to_string()));
        }
    }
}

fn placeholders(s: &str) -> usize {
    s.matches("{}").count()
}

#[test]
fn indonesian_translations_cover_all_keys() {
    let tsv = include_str!("../locales/id.tsv");
    let mut table: HashMap<String, String> = HashMap::new();
    for line in tsv.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('\t').expect("key<TAB>translation");
        assert!(table.insert(unescape_tsv(k), unescape_tsv(v)).is_none(), "duplicate key {k}");
    }
    let mut keys = Vec::new();
    source_keys(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut keys);
    assert!(keys.len() > 250, "found {} keys", keys.len());
    let mut missing = Vec::new();
    for (k, file) in &keys {
        match table.get(k) {
            None => missing.push(format!("{file}: {k:?}")),
            Some(t) => assert_eq!(placeholders(k), placeholders(t), "placeholders differ for {k:?} -> {t:?}"),
        }
    }
    missing.sort();
    missing.dedup();
    assert!(missing.is_empty(), "missing Indonesian translations:\n{}", missing.join("\n"));
}
