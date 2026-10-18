//! Minimal localisation: English source strings are the keys, translations
//! live in `locales/<lang>.tsv` (`English<TAB>Translation` per line, `\n` and
//! `\t` escapes, `#` comments) embedded in the binary.
//!
//! Use [`tr`] for every user visible string and [`trf`] for strings with
//! placeholders (`{}` sequential or `{0}`, `{1}` positional).

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Locale {
    #[default]
    En,
    Id,
}

impl Locale {
    pub const ALL: [Locale; 2] = [Locale::En, Locale::Id];

    pub fn code(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::Id => "id",
        }
    }

    /// Name of the language in that language.
    pub fn native_name(self) -> &'static str {
        match self {
            Locale::En => "English",
            Locale::Id => "Bahasa Indonesia",
        }
    }

    pub fn from_code(code: &str) -> Option<Locale> {
        let c = code.to_ascii_lowercase();
        if c.starts_with("id") || c.starts_with("in") {
            Some(Locale::Id)
        } else if c.starts_with("en") {
            Some(Locale::En)
        } else {
            None
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

const ID_TSV: &str = include_str!("../locales/id.tsv");

fn unescape(s: &str) -> std::borrow::Cow<'static, str> {
    if !s.contains('\\') {
        // SAFETY of lifetime: callers only pass slices of the 'static TSV.
        return std::borrow::Cow::Owned(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

fn parse_table(tsv: &'static str) -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    for line in tsv.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('\t') else { continue };
        if v.is_empty() {
            continue;
        }
        let key: &'static str = if k.contains('\\') { Box::leak(unescape(k).into_owned().into_boxed_str()) } else { k };
        let val: &'static str = if v.contains('\\') { Box::leak(unescape(v).into_owned().into_boxed_str()) } else { v };
        map.insert(key, val);
    }
    map
}

static ID_TABLE: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| parse_table(ID_TSV));

pub fn set_locale(locale: Locale) {
    CURRENT.store(
        match locale {
            Locale::En => 0,
            Locale::Id => 1,
        },
        Ordering::Relaxed,
    );
    if locale == Locale::Id {
        LazyLock::force(&ID_TABLE);
    }
}

pub fn locale() -> Locale {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Locale::Id,
        _ => Locale::En,
    }
}

/// Locale of the operating system, falling back to English.
pub fn system_locale() -> Locale {
    sys_locale::get_locale().and_then(|l| Locale::from_code(&l)).unwrap_or(Locale::En)
}

/// Translate an English UI string into the current locale.
pub fn tr(s: &str) -> &str {
    match locale() {
        Locale::En => s,
        Locale::Id => ID_TABLE.get(s).copied().unwrap_or(s),
    }
}

/// Translate and substitute placeholders: `{}` takes arguments in order,
/// `{0}`, `{1}` ... select by position (translations may reorder them).
pub fn trf(s: &str, args: &[&dyn Display]) -> String {
    format_placeholders(tr(s), args)
}

pub fn format_placeholders(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0usize;
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(close) = template[i + 1..].find('}') {
                let inner = &template[i + 1..i + 1 + close];
                if inner.is_empty() {
                    if let Some(a) = args.get(next) {
                        out.push_str(&a.to_string());
                    }
                    next += 1;
                    i += close + 2;
                    continue;
                }
                if let Ok(n) = inner.parse::<usize>() {
                    if let Some(a) = args.get(n) {
                        out.push_str(&a.to_string());
                    }
                    i += close + 2;
                    continue;
                }
            }
        let ch = template[i..].chars().next().expect("char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// All keys of the Indonesian table (used by tests that check coverage).
pub fn translated_keys() -> Vec<&'static str> {
    ID_TABLE.keys().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders() {
        assert_eq!(format_placeholders("{} of {}", &[&1, &2]), "1 of 2");
        assert_eq!(format_placeholders("{1} before {0}", &[&"a", &"b"]), "b before a");
        assert_eq!(format_placeholders("no {x} args", &[]), "no {x} args");
    }

    #[test]
    fn table_parses() {
        let t = parse_table("# comment\nHello\tHalo\nLine\\nTwo\tBaris\\nDua\nEmpty\t\n");
        assert_eq!(t.get("Hello"), Some(&"Halo"));
        assert_eq!(t.get("Line\nTwo"), Some(&"Baris\nDua"));
        assert!(t.get("Empty").is_none());
    }
}
