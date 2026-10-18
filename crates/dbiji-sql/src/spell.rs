//! Spell checking of comments and string literals (FR-S6).
//!
//! The English dictionary is built from the public domain word list of
//! Webster's Second International (macOS `/usr/share/dict/words`, lower case
//! entries of 2–24 letters, 210 746 words). It is embedded front-coded and
//! zlib-compressed (`data/en_words.zz`, ~465 KB) and decompressed on first
//! use. The list has no inflections, so simple English morphology is applied:
//! plurals (-s, -es, -ies), -ed, -ing, -ly, -er/-est, possessive 's and
//! common contractions.

use crate::lexicon::{FxBuild, Lexicon};
use crate::token::{TokenKind, tokenize, unquote_ident};
use dbiji_core::SqlSyntax;
use std::collections::HashSet;
use std::hash::BuildHasher;
use std::io::Read;
use std::sync::LazyLock;

static EN_WORDS: &[u8] = include_bytes!("../data/en_words.zz");

/// Modern and technical words missing from the 1934 word list.
const TECH_WORDS: &[&str] = &[
    "database",
    "databases",
    "dataset",
    "datasets",
    "datatype",
    "datatypes",
    "datetime",
    "metadata",
    "timestamp",
    "timestamps",
    "timezone",
    "timezones",
    "login",
    "logins",
    "logout",
    "username",
    "usernames",
    "email",
    "emails",
    "online",
    "offline",
    "website",
    "websites",
    "internet",
    "intranet",
    "backend",
    "frontend",
    "config",
    "configs",
    "json",
    "jsonb",
    "xml",
    "csv",
    "tsv",
    "yaml",
    "html",
    "http",
    "https",
    "api",
    "apis",
    "url",
    "urls",
    "uri",
    "uuid",
    "uuids",
    "guid",
    "boolean",
    "booleans",
    "varchar",
    "nvarchar",
    "bigint",
    "smallint",
    "tinyint",
    "int",
    "enum",
    "enums",
    "nullable",
    "null",
    "nulls",
    "upsert",
    "upserts",
    "subquery",
    "subqueries",
    "cte",
    "ctes",
    "sql",
    "mysql",
    "postgres",
    "postgresql",
    "sqlite",
    "mariadb",
    "duckdb",
    "clickhouse",
    "oracle",
    "schema",
    "schemas",
    "namespace",
    "namespaces",
    "refactor",
    "refactoring",
    "refactored",
    "async",
    "sync",
    "runtime",
    "middleware",
    "workflow",
    "workflows",
    "dashboard",
    "dashboards",
    "app",
    "apps",
    "todo",
    "todos",
    "fixme",
    "hotfix",
    "debug",
    "debugging",
    "debugger",
    "cache",
    "cached",
    "caching",
    "callback",
    "callbacks",
    "checkbox",
    "dropdown",
    "unicode",
    "utf",
    "ascii",
    "admin",
    "admins",
    "auth",
    "oauth",
    "token",
    "tokens",
    "webhook",
    "webhooks",
    "endpoint",
    "endpoints",
    "params",
    "param",
    "args",
    "arg",
    "init",
    "initialize",
    "initialized",
    "deduplicate",
    "dedupe",
    "dedup",
    "lookup",
    "lookups",
    "rollup",
    "rollups",
    "backfill",
    "backfilled",
    "batch",
    "batches",
    "upload",
    "uploads",
    "uploaded",
    "download",
    "downloads",
    "downloaded",
    "username",
    "userid",
    "signup",
    "signups",
    "checkout",
    "checkouts",
    "inbox",
    "spam",
    "blog",
    "blogs",
    "repo",
    "repos",
    "github",
    "gitlab",
    "git",
    "docker",
    "kubernetes",
    "linux",
    "macos",
    "windows",
    "ios",
    "android",
    "smartphone",
    "app",
    "plugin",
    "plugins",
    "screenshot",
    "screenshots",
    "wifi",
    "bluetooth",
    "hashtag",
    "emoji",
    "emojis",
    "ecommerce",
    "sku",
    "skus",
    "vat",
    "kpi",
    "kpis",
    "analytics",
    "pipeline",
    "pipelines",
    "etl",
    "elt",
    "datawarehouse",
    "warehouse",
    "lakehouse",
    "parquet",
    "sharding",
    "shard",
    "shards",
    "replica",
    "replicas",
    "failover",
    "uptime",
    "downtime",
    "latency",
    "throughput",
    "timeout",
    "timeouts",
    "retry",
    "retries",
    "retried",
    "rerun",
    "reindex",
    "reindexing",
    "vacuum",
    "autovacuum",
    "trigger",
    "triggers",
    "cron",
    "crontab",
    "scheduler",
    "queue",
    "queues",
    "enqueue",
    "dequeue",
    "mutex",
    "multithreaded",
    "localhost",
    "hostname",
    "ip",
    "ipv",
    "dns",
    "ssl",
    "tls",
    "ssh",
    "vpn",
    "proxy",
    "firewall",
    "encrypt",
    "encrypted",
    "encryption",
    "decrypt",
    "decrypted",
    "hash",
    "hashed",
    "hashing",
    "salt",
    "salted",
    "pii",
    "gdpr",
    "anonymize",
    "anonymized",
    "pseudonymize",
    "timeline",
    "frontend",
    "fullname",
    "firstname",
    "lastname",
    "zipcode",
    "postcode",
    "geolocation",
    "lat",
    "lng",
    "lon",
    "geo",
    "gis",
    "spatial",
    "covid",
    "ok",
    "okay",
    "etc",
    "eg",
    "ie",
    "vs",
    "via",
    "misc",
    "info",
    "stats",
    "dev",
    "devs",
    "prod",
    "staging",
    "qa",
    "changelog",
    "readme",
    "dbiji",
    "dbeaver",
    "nosql",
    "rdbms",
    "olap",
    "oltp",
    "crud",
    "orm",
    "codebase",
    "hardcoded",
    "hardcode",
    "overridden",
    "override",
    "overrides",
    "deprecated",
    "deprecate",
    "inline",
    "inlined",
    "lowercase",
    "uppercase",
    "whitespace",
    "newline",
    "newlines",
    "tab",
    "tabs",
    "regex",
    "regexes",
    "wildcard",
    "wildcards",
];

static ENGLISH: LazyLock<SpellChecker> = LazyLock::new(|| {
    let mut c = match decode_word_list(EN_WORDS) {
        Some(words) => SpellChecker::from_text(words),
        None => {
            tracing::warn!("spell checker: embedded word list could not be decoded");
            SpellChecker::from_text(String::new())
        }
    };
    c.extend(TECH_WORDS.iter().copied());
    c
});

/// Decode the embedded format: zlib stream of `[shared prefix length: u8]
/// [suffix bytes] \n` entries.
fn decode_word_list(data: &[u8]) -> Option<String> {
    let mut raw = Vec::with_capacity(1_200_000);
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut raw)
        .ok()?;
    let mut text = String::with_capacity(raw.len() * 2);
    let mut prev: Vec<u8> = Vec::with_capacity(32);
    let mut i = 0;
    while i < raw.len() {
        let shared = raw[i] as usize;
        i += 1;
        let end = raw[i..].iter().position(|&b| b == b'\n').map(|p| i + p)?;
        prev.truncate(shared.min(prev.len()));
        prev.extend_from_slice(&raw[i..end]);
        i = end + 1;
        let word = std::str::from_utf8(&prev).ok()?;
        text.push_str(word);
        text.push('\n');
    }
    Some(text)
}

/// A misspelled word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpellIssue {
    /// Byte range in the source.
    pub start: usize,
    /// End of the word (exclusive).
    pub end: usize,
    /// The misspelled word as written.
    pub word: String,
}

/// Dictionary with fast lookup and suggestions.
pub struct SpellChecker {
    text: String,
    /// (offset, length) of every word in `text`.
    words: Vec<(u32, u8)>,
    /// Open addressing table of word index + 1 (0 = empty).
    table: Vec<u32>,
    /// Word indices by length.
    by_len: Vec<Vec<u32>>,
    /// Letter presence bit masks (a-z), for cheap candidate filtering.
    masks: Vec<u32>,
    hasher: FxBuild,
}

impl std::fmt::Debug for SpellChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpellChecker")
            .field("words", &self.words.len())
            .finish()
    }
}

fn letter_mask(w: &[u8]) -> u32 {
    w.iter().fold(0u32, |m, &c| {
        if c.is_ascii_lowercase() {
            m | (1 << (c - b'a'))
        } else {
            m | (1 << 26)
        }
    })
}

const MAX_WORD: usize = 32;

/// Damerau-Levenshtein (optimal string alignment) distance, or `None` when
/// above `max`. Words longer than 32 bytes are not compared.
fn distance(a: &[u8], b: &[u8], max: usize) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > max || n > MAX_WORD || m > MAX_WORD {
        return None;
    }
    let mut prev2 = [0usize; MAX_WORD + 1];
    let mut prev = [0usize; MAX_WORD + 1];
    let mut cur = [0usize; MAX_WORD + 1];
    for (j, p) in prev.iter_mut().enumerate().take(m + 1) {
        *p = j;
    }
    for i in 1..=n {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[m];
    (d <= max).then_some(d)
}

impl SpellChecker {
    /// The embedded English dictionary (decompressed on first use).
    pub fn english() -> &'static SpellChecker {
        &ENGLISH
    }

    /// Dictionary from arbitrary words (lower-cased; non-ASCII words are kept as is).
    pub fn from_words<I, S>(words: I) -> SpellChecker
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut list: Vec<String> = words
            .into_iter()
            .map(|w| w.as_ref().trim().to_lowercase())
            .filter(|w| !w.is_empty() && w.len() < 256 && !w.contains('\n'))
            .collect();
        list.sort();
        list.dedup();
        let mut text = String::new();
        for w in list {
            text.push_str(&w);
            text.push('\n');
        }
        SpellChecker::from_text(text)
    }

    /// From `\n` separated, de-duplicated words.
    fn from_text(text: String) -> SpellChecker {
        let mut words = Vec::with_capacity(text.len() / 9);
        let mut start = 0usize;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                let len = i - start;
                if len > 0 && len < 256 && start <= u32::MAX as usize {
                    words.push((start as u32, len as u8));
                }
                start = i + 1;
            }
        }
        let hasher = FxBuild::default();
        let cap = (words.len() * 2).next_power_of_two().max(16);
        let mut table = vec![0u32; cap];
        let mut by_len: Vec<Vec<u32>> = vec![Vec::new(); 26];
        let mut masks = Vec::with_capacity(words.len());
        for (idx, &(off, len)) in words.iter().enumerate() {
            let w = &text.as_bytes()[off as usize..off as usize + len as usize];
            let mut h = (hasher.hash_one(w) as usize) & (cap - 1);
            while table[h] != 0 {
                h = (h + 1) & (cap - 1);
            }
            table[h] = idx as u32 + 1;
            let l = (len as usize).min(25);
            by_len[l].push(idx as u32);
            masks.push(letter_mask(w));
        }
        SpellChecker {
            text,
            words,
            table,
            by_len,
            masks,
            hasher,
        }
    }

    /// Add words that are not in the dictionary yet.
    pub fn extend<'w>(&mut self, words: impl IntoIterator<Item = &'w str>) {
        let new: Vec<String> = words
            .into_iter()
            .map(|w| w.trim().to_lowercase())
            .filter(|w| !w.is_empty() && w.len() < 256 && !w.contains('\n'))
            .collect();
        let mut added = false;
        for w in new {
            if self.contains(&w) {
                continue;
            }
            let off = self.text.len();
            self.text.push_str(&w);
            self.text.push('\n');
            if off <= u32::MAX as usize {
                self.words.push((off as u32, w.len() as u8));
                added = true;
            }
            if self.words.len() * 2 > self.table.len() {
                // Keep the load factor low: rebuild everything.
                let text = std::mem::take(&mut self.text);
                *self = SpellChecker::from_text(text);
                added = false;
                continue;
            }
            if added {
                self.index_last();
            }
        }
    }

    fn index_last(&mut self) {
        let idx = self.words.len() - 1;
        let w = self.word(idx).to_vec();
        let cap = self.table.len();
        let mut h = (self.hasher.hash_one(&w[..]) as usize) & (cap - 1);
        while self.table[h] != 0 {
            h = (h + 1) & (cap - 1);
        }
        self.table[h] = idx as u32 + 1;
        self.by_len[w.len().min(25)].push(idx as u32);
        self.masks.push(letter_mask(&w));
    }

    fn word(&self, idx: usize) -> &[u8] {
        let (off, len) = self.words[idx];
        &self.text.as_bytes()[off as usize..off as usize + len as usize]
    }

    /// Number of dictionary words.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// The dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Exact membership of a lower case word.
    pub fn contains(&self, word: &str) -> bool {
        if self.words.is_empty() || word.is_empty() {
            return false;
        }
        let w = word.as_bytes();
        let mask = self.table.len() - 1;
        let mut h = (self.hasher.hash_one(w) as usize) & mask;
        loop {
            let slot = self.table[h];
            if slot == 0 {
                return false;
            }
            if self.word(slot as usize - 1) == w {
                return true;
            }
            h = (h + 1) & mask;
        }
    }

    /// The word is spelled correctly (case-insensitive, with inflections).
    pub fn is_correct(&self, word: &str) -> bool {
        if word.is_empty() {
            return true;
        }
        let lower = word.to_lowercase().replace('’', "'");
        self.known(&lower, 0)
    }

    fn known(&self, w: &str, depth: u32) -> bool {
        if self.contains(w) {
            return true;
        }
        if depth > 2 || w.len() < 3 {
            return false;
        }
        if let Some(i) = w.find('\'') {
            let (base, suffix) = (&w[..i], &w[i..]);
            return match suffix {
                "'s" | "'" | "'ll" | "'re" | "'ve" | "'d" | "'m" => self.known(base, depth + 1),
                "'t" => {
                    let stem = base.strip_suffix('n').unwrap_or(base);
                    matches!(stem, "ca" | "wo" | "sha" | "ai")
                        || self.known(stem, depth + 1)
                        || self.known(base, depth + 1)
                }
                _ => false,
            };
        }
        if let Some(s) = w.strip_suffix("s'") {
            return self.known(&format!("{s}s"), depth + 1);
        }
        let try_stem = |stem: &str, add: &str| -> bool {
            if stem.len() < 2 {
                return false;
            }
            let cand = format!("{stem}{add}");
            self.contains(&cand) || self.known(&cand, depth + 1)
        };
        let doubled = |stem: &str| -> bool {
            let b = stem.as_bytes();
            let n = b.len();
            n >= 3
                && b[n - 1] == b[n - 2]
                && !matches!(b[n - 1], b'a' | b'e' | b'i' | b'o' | b'u')
                && self.contains(&stem[..n - 1])
        };
        const RULES: &[(&str, &[&str])] = &[
            ("ies", &["y", "ie"]),
            ("es", &["", "e"]),
            ("s", &[""]),
            ("ied", &["y"]),
            ("ed", &["", "e"]),
            ("ing", &["", "e"]),
            ("ily", &["y"]),
            ("ally", &["al", ""]),
            ("ly", &["", "le"]),
            ("ier", &["y"]),
            ("iest", &["y"]),
            ("er", &["", "e"]),
            ("est", &["", "e"]),
        ];
        for (suffix, adds) in RULES {
            let Some(stem) = w.strip_suffix(suffix) else {
                continue;
            };
            if *suffix == "s" && (stem.ends_with('s') || stem.ends_with('\'')) {
                continue;
            }
            if adds.iter().any(|a| try_stem(stem, a)) {
                return true;
            }
            if matches!(*suffix, "ed" | "ing" | "er" | "est") && doubled(stem) {
                return true;
            }
        }
        false
    }

    /// Up to `max` dictionary words within Damerau-Levenshtein distance 2,
    /// closest first (case follows the input: `Teh` → `The`).
    pub fn suggestions(&self, word: &str, max: usize) -> Vec<String> {
        let lower = word.to_lowercase();
        if lower.is_empty() || !lower.is_ascii() || max == 0 || lower.len() > 30 {
            return Vec::new();
        }
        let w = lower.as_bytes();
        let wmask = letter_mask(w);
        let mut sorted_w = w.to_vec();
        sorted_w.sort_unstable();
        // (distance, not an anagram, first letter differs, length difference, index)
        let mut found: Vec<(usize, bool, bool, usize, u32)> = Vec::new();
        let lo = w.len().saturating_sub(2).max(1);
        let hi = (w.len() + 2).min(25);
        for len in lo..=hi {
            for &idx in &self.by_len[len] {
                if (wmask ^ self.masks[idx as usize]).count_ones() > 4 {
                    continue;
                }
                let cand = self.word(idx as usize);
                if let Some(d) = distance(w, cand, 2)
                    && d > 0
                {
                    let mut sc = cand.to_vec();
                    sc.sort_unstable();
                    found.push((
                        d,
                        sc != sorted_w,
                        cand.first() != w.first(),
                        len.abs_diff(w.len()),
                        idx,
                    ));
                }
            }
        }
        found.sort_by(|a, b| {
            (a.0, a.1, a.2, a.3)
                .cmp(&(b.0, b.1, b.2, b.3))
                .then_with(|| self.word(a.4 as usize).cmp(self.word(b.4 as usize)))
        });
        let capital = word.chars().next().is_some_and(char::is_uppercase);
        found
            .into_iter()
            .take(max)
            .map(|(_, _, _, _, idx)| {
                let s = String::from_utf8_lossy(self.word(idx as usize)).into_owned();
                if capital {
                    let mut c = s.chars();
                    c.next()
                        .map(|f| f.to_uppercase().chain(c).collect())
                        .unwrap_or(s)
                } else {
                    s
                }
            })
            .collect()
    }
}

/// Chunks that are code, URLs, e-mail addresses or paths are not prose.
fn is_code_like(chunk: &str) -> bool {
    if chunk.contains("://") || chunk.starts_with("www.") {
        return true;
    }
    if chunk.contains([
        '@', '/', '\\', '=', '{', '}', '<', '>', '_', '$', '`', '|', '#', '%', '*', '^', '~',
    ]) {
        return true;
    }
    // a.b with letters on both sides: file.sql, example.com, schema.table
    let b = chunk.as_bytes();
    (1..b.len().saturating_sub(1)).any(|i| {
        b[i] == b'.' && b[i - 1].is_ascii_alphanumeric() && b[i + 1].is_ascii_alphanumeric()
    })
}

fn is_camel_case(w: &str) -> bool {
    let mut prev_lower = false;
    for c in w.chars() {
        if c.is_uppercase() && prev_lower {
            return true;
        }
        prev_lower = c.is_lowercase();
    }
    false
}

struct Ctx<'a> {
    checker: &'a SpellChecker,
    lex: Lexicon,
    idents: HashSet<String>,
    /// User words, lower case.
    user: HashSet<String>,
    out: Vec<SpellIssue>,
}

impl Ctx<'_> {
    fn check_text(&mut self, text: &str, base: usize) {
        let mut chunk_start = None;
        for (i, c) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            if c.is_whitespace() {
                if let Some(s) = chunk_start.take() {
                    self.check_chunk(&text[s..i], base + s);
                }
            } else if chunk_start.is_none() {
                chunk_start = Some(i);
            }
        }
    }

    fn check_chunk(&mut self, chunk: &str, base: usize) {
        if is_code_like(chunk) {
            return;
        }
        let mut start = None;
        let word_char = |c: char| c.is_alphanumeric() || c == '\'' || c == '’' || c == '_';
        for (i, c) in chunk
            .char_indices()
            .chain(std::iter::once((chunk.len(), ' ')))
        {
            if word_char(c) {
                if start.is_none() {
                    start = Some(i);
                }
            } else if let Some(s) = start.take() {
                self.check_word(&chunk[s..i], base + s);
            }
        }
    }

    fn check_word(&mut self, raw: &str, pos: usize) {
        let lead = raw.len() - raw.trim_start_matches(['\'', '’']).len();
        let w = raw.trim_matches(['\'', '’']);
        if w.chars().count() < 3
            || w.chars().any(|c| {
                c.is_ascii_digit() || c == '_' || !c.is_alphabetic() && c != '\'' && c != '’'
            })
        {
            return;
        }
        let letters: Vec<char> = w.chars().filter(|c| c.is_alphabetic()).collect();
        if letters.iter().all(|c| c.is_uppercase()) || is_camel_case(w) {
            return;
        }
        let lower = w.to_lowercase();
        if self.lex.keywords.contains(w)
            || self.lex.functions.contains(w)
            || self.lex.data_types.contains(w)
        {
            return;
        }
        if self.idents.contains(&lower) || self.user.contains(&lower) {
            return;
        }
        if self.checker.is_correct(w) {
            return;
        }
        let start = pos + lead;
        self.out.push(SpellIssue {
            start,
            end: start + w.len(),
            word: w.to_string(),
        });
    }
}

/// Misspelled words in the comments and string literals of `src`.
///
/// Skipped: words shorter than 3 letters, words with digits or `_`,
/// camelCase and ALL-CAPS words, URLs, e-mail addresses and paths, SQL
/// keywords, functions and types, identifiers used elsewhere in the script,
/// words in `user_words`, dollar-quoted bodies, hex/bit strings and MySQL
/// executable comments.
pub fn check_spelling(
    src: &str,
    syntax: &SqlSyntax,
    checker: &SpellChecker,
    user_words: &HashSet<String>,
) -> Vec<SpellIssue> {
    let tokens = tokenize(src, syntax);
    let mut idents = HashSet::new();
    for t in &tokens {
        if matches!(t.kind, TokenKind::Identifier | TokenKind::QuotedIdentifier) {
            idents.insert(unquote_ident(t.text(src)).to_lowercase());
        }
    }
    let user = user_words.iter().map(|w| w.to_lowercase()).collect();
    let mut ctx = Ctx {
        checker,
        lex: Lexicon::new(syntax),
        idents,
        user,
        out: Vec::new(),
    };
    for t in &tokens {
        let text = t.text(src);
        let (body, skip) = match t.kind {
            TokenKind::LineComment => {
                let s = if text.starts_with("--") { 2 } else { 1 };
                (&text[s..], s)
            }
            TokenKind::BlockComment => {
                if text.starts_with("/*!") || text.starts_with("/*+") {
                    continue;
                }
                let inner = &text[2..];
                let inner = inner.strip_suffix("*/").unwrap_or(inner);
                (inner, 2)
            }
            TokenKind::String => {
                let first = text.chars().next().unwrap_or('\'');
                if first == '$' {
                    continue; // dollar-quoted bodies are code
                }
                let prefix = match first {
                    '\'' | '"' => 0,
                    'X' | 'x' | 'B' | 'b' => continue,
                    'U' | 'u' => 2,
                    _ => 1,
                };
                let open = prefix + 1;
                if text.len() <= open {
                    continue;
                }
                let inner = &text[open..];
                let inner = if t.unterminated {
                    inner
                } else {
                    &inner[..inner.len().saturating_sub(1)]
                };
                (inner, open)
            }
            _ => continue,
        };
        ctx.check_text(body, t.start + skip);
    }
    ctx.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn small() -> SpellChecker {
        SpellChecker::from_words([
            "the", "table", "update", "query", "stop", "run", "quick", "happy", "large", "big",
            "user", "be", "do", "is", "it", "this", "fetch", "rows", "all", "only", "read",
        ])
    }

    #[test]
    fn english_dictionary_loads_fast() {
        let t = std::time::Instant::now();
        let en = SpellChecker::english();
        let elapsed = t.elapsed();
        assert!(en.len() > 200_000, "{}", en.len());
        // Release builds load in well under 50 ms; debug builds are slower.
        assert!(elapsed.as_millis() < 3000, "{elapsed:?}");
        for w in ["table", "database", "query", "the", "column", "select"] {
            assert!(en.contains(w), "{w}");
        }
        assert!(!en.contains("qwzx"));
    }

    #[test]
    fn morphology() {
        let c = small();
        for w in [
            "tables", "Tables", "queries", "updated", "updating", "stopped", "running", "quickly",
            "happily", "larger", "biggest", "happier", "user's", "users'", "isn't", "don't",
            "can't", "it's", "Fetches",
        ] {
            assert!(c.is_correct(w), "{w}");
        }
        for w in ["tabel", "qeury", "xyzzy", "tabless"] {
            assert!(!c.is_correct(w), "{w}");
        }
        let en = SpellChecker::english();
        for w in [
            "rows",
            "columns",
            "indexes",
            "deleted",
            "inserting",
            "joined",
            "returns",
            "users",
            "orders",
        ] {
            assert!(en.is_correct(w), "{w}");
        }
    }

    #[test]
    fn suggestions() {
        let en = SpellChecker::english();
        let s = en.suggestions("tabel", 5);
        assert_eq!(s.first().map(String::as_str), Some("table"), "{s:?}");
        let s = en.suggestions("Teh", 5);
        assert_eq!(s.first().map(String::as_str), Some("The"), "{s:?}");
        let s = en.suggestions("qeury", 5);
        assert_eq!(s.first().map(String::as_str), Some("query"), "{s:?}");
        assert!(
            en.suggestions("databse", 5)
                .contains(&"database".to_string())
        );
        assert!(en.suggestions("", 5).is_empty());
        assert!(en.suggestions("ünïcode", 5).is_empty());
        assert_eq!(distance(b"abcd", b"acbd", 2), Some(1));
        assert_eq!(distance(b"kitten", b"sitting", 2), None);
        assert_eq!(distance(b"same", b"same", 2), Some(0));
        let t = std::time::Instant::now();
        for w in ["recieve", "seperate", "occured", "definately", "existance"] {
            assert!(!en.suggestions(w, 3).is_empty(), "{w}");
        }
        assert!(t.elapsed().as_millis() < 5000);
    }

    #[test]
    fn checks_comments_and_strings_only() {
        let c = small();
        let src = "-- the tabel is quick\nSELECT tabel_x, recieve FROM t WHERE a = 'the qeury' /* happy wrold */";
        let issues = check_spelling(src, &pg(), &c, &HashSet::new());
        let words: Vec<_> = issues.iter().map(|i| i.word.as_str()).collect();
        assert_eq!(words, vec!["tabel", "qeury", "wrold"]);
        for i in &issues {
            assert_eq!(&src[i.start..i.end], i.word);
        }
    }

    #[test]
    fn skips_code_like_words() {
        let c = SpellChecker::english();
        let src = "-- see https://exampel.com/pathh or mail usr@exampel.com, file reportt.sql, C:\\dirr, ABCD camelCase snake_case 123abc ab the\n-- recieve SELECT FROM\nSELECT recieve FROM t; -- recieve\nSELECT $$ wrongg $$, X'abcdef', E'the tabel'";
        let mut user = HashSet::new();
        user.insert("Tabel".to_string());
        let issues = check_spelling(src, &pg(), c, &user);
        let words: Vec<_> = issues.iter().map(|i| i.word.as_str()).collect();
        // `recieve` is an identifier elsewhere in the script; `Tabel` is a user word.
        assert!(words.is_empty(), "{words:?}");
        let issues = check_spelling("-- recieve the tabel", &pg(), c, &HashSet::new());
        assert_eq!(
            issues.iter().map(|i| i.word.as_str()).collect::<Vec<_>>(),
            vec!["recieve", "tabel"]
        );
    }

    #[test]
    fn random_inputs_never_panic() {
        let c = small();
        let mut rng = Rng::new(9);
        for _ in 0..300 {
            let src = random_sql(&mut rng, 30);
            for syn in all_syntaxes() {
                for i in check_spelling(&src, &syn, &c, &HashSet::new()) {
                    assert!(
                        src.is_char_boundary(i.start)
                            && src.is_char_boundary(i.end)
                            && i.start < i.end
                    );
                }
            }
        }
        let _ = SpellChecker::from_words(Vec::<String>::new()).suggestions("abc", 3);
        assert!(!SpellChecker::from_words(Vec::<String>::new()).contains("a"));
        let mut e = SpellChecker::from_words(["one"]);
        e.extend((0..100).map(|i| ["alpha", "beta", "gamma", "delta"][i % 4]));
        e.extend(["zeta", "one"]);
        assert_eq!(e.len(), 6);
        assert!(e.contains("zeta") && e.contains("gamma") && e.contains("one"));
        // Growing past the table capacity rebuilds the index.
        let many: Vec<String> = (0..40).map(|i| format!("word{i}")).collect();
        e.extend(many.iter().map(String::as_str));
        assert_eq!(e.len(), 46);
        assert!(many.iter().all(|w| e.contains(w)) && e.contains("alpha"));
    }

    #[test]
    fn embedded_list_is_sorted_unique_lowercase() {
        let text = decode_word_list(EN_WORDS).expect("decodes");
        let words: Vec<&str> = text.lines().collect();
        assert_eq!(words.len(), 210_746);
        assert!(words.windows(2).all(|w| w[0] < w[1]));
        assert!(
            words
                .iter()
                .all(|w| (2..=24).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()))
        );
        assert!(EN_WORDS.len() < 1_000_000);
        assert!(decode_word_list(b"not zlib").is_none());
    }

    /// Rebuilds `data/en_words.zz` from the macOS word list (public domain
    /// Webster's 2nd): `cargo test -p dbiji-sql -- --ignored regenerate_word_list`.
    #[test]
    #[ignore]
    fn regenerate_word_list() {
        use std::io::Write;
        let raw = std::fs::read_to_string("/usr/share/dict/words").expect("word list");
        let mut words: Vec<&str> = raw
            .lines()
            .filter(|w| (2..=24).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()))
            .collect();
        words.sort_unstable();
        words.dedup();
        let mut out = Vec::new();
        let mut prev: &[u8] = b"";
        for w in &words {
            let b = w.as_bytes();
            let shared = prev
                .iter()
                .zip(b)
                .take(255)
                .take_while(|(x, y)| x == y)
                .count();
            out.push(shared as u8);
            out.extend_from_slice(&b[shared..]);
            out.push(b'\n');
            prev = b;
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        z.write_all(&out).expect("compress");
        let data = z.finish().expect("compress");
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/data/en_words.zz");
        std::fs::write(path, &data).expect("write");
        assert_eq!(
            decode_word_list(&data).map(|t| t.lines().count()),
            Some(words.len())
        );
    }
}
