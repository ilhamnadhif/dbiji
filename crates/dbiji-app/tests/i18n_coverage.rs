//! Every string passed to `tr("...")` / `trf("...")` anywhere in the workspace
//! must have an Indonesian translation in `crates/dbiji-core/locales/id.tsv`
//! (PRD §8: "all text goes through the translation system").

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("workspace root")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "target" || name.starts_with('.') || name == "tests" || name == "examples" || name == "benches" {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// Extract the literal argument of tr("...")/trf("...") calls, handling escapes.
fn extract(src: &str, out: &mut BTreeSet<String>) {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        let rest = &src[i..];
        let (skip, ok) = if rest.starts_with("tr(\"") {
            (4, true)
        } else if rest.starts_with("trf(\"") {
            (5, true)
        } else {
            (1, false)
        };
        if !ok {
            i += 1;
            continue;
        }
        // must not be part of a longer identifier (e.g. `attr("`)
        if i > 0 {
            let prev = bytes[i - 1] as char;
            if prev.is_alphanumeric() || prev == '_' {
                i += skip;
                continue;
            }
        }
        let mut j = i + skip;
        let mut s = String::new();
        let mut closed = false;
        while j < bytes.len() {
            let c = src[j..].chars().next().unwrap_or('\0');
            if c == '\\' {
                let n = src[j + 1..].chars().next().unwrap_or('\0');
                match n {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    '"' => s.push('"'),
                    '\\' => s.push('\\'),
                    '\'' => s.push('\''),
                    other => {
                        s.push('\\');
                        s.push(other);
                    }
                }
                j += 1 + n.len_utf8();
                continue;
            }
            if c == '"' {
                closed = true;
                break;
            }
            s.push(c);
            j += c.len_utf8();
        }
        if closed && !s.is_empty() {
            out.insert(s);
        }
        i = j.max(i + 1);
    }
}

#[test]
fn every_ui_string_has_an_indonesian_translation() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    let mut keys = BTreeSet::new();
    for f in &files {
        if let Ok(src) = std::fs::read_to_string(f) {
            extract(&src, &mut keys);
        }
    }
    let translated: BTreeSet<&str> = dbiji_core::i18n::translated_keys().into_iter().collect();
    let missing: Vec<&String> = keys.iter().filter(|k| !translated.contains(k.as_str())).collect();
    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|m| m.replace('\n', "\\n").replace('\t', "\\t")).collect();
        // Writing the list helps the translator; the test still fails.
        let out = root.join("target").join("i18n-missing.txt");
        let _ = std::fs::create_dir_all(root.join("target"));
        let _ = std::fs::write(&out, list.join("\n"));
        panic!("{} UI strings without Indonesian translation (see {}):\n{}", missing.len(), out.display(), list.iter().take(50).cloned().collect::<Vec<_>>().join("\n"));
    }
}

#[test]
fn extractor_handles_escapes() {
    let mut keys = BTreeSet::new();
    extract(r#"let a = tr("Hello"); let b = trf("Rows: {}\nDone", &[&1]); attr("x");"#, &mut keys);
    assert!(keys.contains("Hello"));
    assert!(keys.contains("Rows: {}\nDone"));
    assert!(!keys.contains("x"));
}
