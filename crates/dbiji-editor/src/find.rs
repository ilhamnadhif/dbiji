//! Find / replace engine (plain text or regular expressions, optional case
//! sensitivity and whole-word matching).

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::text::{char_at, char_before, next_char_boundary};

/// Search flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FindOptions {
    /// Match case exactly.
    pub case_sensitive: bool,
    /// Only match whole words.
    pub whole_word: bool,
    /// Interpret the query as a regular expression (`regex` crate syntax;
    /// `^`/`$` match at line boundaries). Replacements may use `$1`, `${name}`.
    pub regex: bool,
}

/// Maximum number of matches collected for highlighting and counting.
pub const MAX_MATCHES: usize = 100_000;

/// A compiled query.
#[derive(Clone, Debug)]
pub struct Searcher {
    regex: Regex,
    options: FindOptions,
}

impl Searcher {
    /// Compile `query`. Returns `Ok(None)` for an empty query and the regex
    /// error message for an invalid regular expression.
    pub fn new(query: &str, options: FindOptions) -> Result<Option<Self>, String> {
        if query.is_empty() {
            return Ok(None);
        }
        let pattern = if options.regex {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!options.case_sensitive)
            .multi_line(true)
            .size_limit(32 * 1024 * 1024)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Some(Self { regex, options }))
    }

    fn word_ok(&self, text: &str, m: &Range<usize>, is_word: &impl Fn(char) -> bool) -> bool {
        if !self.options.whole_word {
            return true;
        }
        let first = char_at(text, m.start);
        let last = char_before(text, m.end);
        let left_ok = match (char_before(text, m.start), first) {
            (Some(b), Some(f)) => !(is_word(b) && is_word(f)),
            _ => true,
        };
        let right_ok = match (last, char_at(text, m.end)) {
            (Some(l), Some(a)) => !(is_word(l) && is_word(a)),
            _ => true,
        };
        left_ok && right_ok && !m.is_empty()
    }

    /// All matches in `text` (at most `limit`), in order. Zero-length
    /// matches are included (useful for regex replace with `^`).
    pub fn find_all(
        &self,
        text: &str,
        limit: usize,
        is_word: impl Fn(char) -> bool,
    ) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos <= text.len() && out.len() < limit {
            let Some(m) = self.regex.find_at(text, pos) else {
                break;
            };
            let range = m.range();
            if self.word_ok(text, &range, &is_word) {
                out.push(range.clone());
            }
            pos = if range.is_empty() || !self.word_ok(text, &range, &is_word) {
                if range.start >= text.len() {
                    break;
                }
                next_char_boundary(text, range.start)
            } else {
                range.end
            };
        }
        out
    }

    /// Does `range` of `text` match the query exactly (used to decide whether
    /// "replace" applies to the current selection)?
    pub fn matches_exactly(
        &self,
        text: &str,
        range: Range<usize>,
        is_word: impl Fn(char) -> bool,
    ) -> bool {
        if range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            return false;
        }
        match self.regex.find_at(text, range.start) {
            Some(m) => m.range() == range && self.word_ok(text, &range, &is_word),
            None => false,
        }
    }

    /// Replacement text for the match at `range`: `$n` / `${name}` capture
    /// references are expanded for regex queries; plain queries insert
    /// `replacement` literally.
    pub fn replacement_for(&self, text: &str, range: Range<usize>, replacement: &str) -> String {
        if !self.options.regex {
            return replacement.to_owned();
        }
        let Some(caps) = self.regex.captures_at(text, range.start) else {
            return replacement.to_owned();
        };
        let mut dst = String::new();
        caps.expand(replacement, &mut dst);
        dst
    }
}

/// Index of the first match at or after `offset` (wrapping to the first).
pub fn next_match(matches: &[Range<usize>], offset: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let i = matches.partition_point(|m| m.start < offset);
    Some(if i < matches.len() { i } else { 0 })
}

/// Index of the last match starting before `offset` (wrapping to the last).
pub fn prev_match(matches: &[Range<usize>], offset: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let i = matches.partition_point(|m| m.start < offset);
    Some(if i == 0 { matches.len() - 1 } else { i - 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::is_word_char;

    fn find(text: &str, q: &str, o: FindOptions) -> Vec<Range<usize>> {
        Searcher::new(q, o)
            .expect("valid")
            .map(|s| s.find_all(text, MAX_MATCHES, is_word_char))
            .unwrap_or_default()
    }

    #[test]
    fn plain_case_and_word() {
        let t = "select Sel, sel_x, SEL";
        let o = FindOptions::default();
        assert_eq!(find(t, "sel", o), vec![0..3, 7..10, 12..15, 19..22]);
        let cs = FindOptions {
            case_sensitive: true,
            ..o
        };
        assert_eq!(find(t, "sel", cs), vec![0..3, 12..15]);
        let ww = FindOptions {
            whole_word: true,
            ..o
        };
        assert_eq!(find(t, "sel", ww), vec![7..10, 19..22]);
        // Special characters are literal in plain mode.
        assert_eq!(find("a.b a*b", ".", o), vec![1..2]);
        assert_eq!(find("x", "", o), Vec::<Range<usize>>::new());
        // Whole word with a query that starts with punctuation.
        assert_eq!(find("x.y .y", ".y", ww), vec![1..3, 4..6]);
    }

    #[test]
    fn overlapping_whole_word_candidates() {
        let ww = FindOptions {
            whole_word: true,
            ..Default::default()
        };
        assert_eq!(find("aaa aa", "aa", ww), vec![4..6]);
        assert_eq!(find("abab ab", "ab", ww), vec![5..7]);
    }

    #[test]
    fn regex_mode_and_replacement() {
        let o = FindOptions {
            regex: true,
            ..Default::default()
        };
        let t = "id = 10, name = 'x', age = 42";
        assert_eq!(find(t, r"\d+", o), vec![5..7, 27..29]);
        let s = Searcher::new(r"(\w+) = (\d+)", o)
            .expect("ok")
            .expect("some");
        let m = s.find_all(t, 10, is_word_char);
        assert_eq!(m, vec![0..7, 21..29]);
        assert_eq!(s.replacement_for(t, m[0].clone(), "$2 AS $1"), "10 AS id");
        assert!(Searcher::new("(", o).is_err());
        // ^ matches at every line start (multi-line).
        assert_eq!(find("a\nb\nc", "^", o), vec![0..0, 2..2, 4..4]);
        // Unicode-safe progress over zero-length matches.
        assert_eq!(find("éé", "x*", o), vec![0..0, 2..2, 4..4]);
        let plain = Searcher::new("a", FindOptions::default())
            .expect("ok")
            .expect("some");
        assert_eq!(plain.replacement_for("a", 0..1, "$1"), "$1");
    }

    #[test]
    fn exact_match_and_navigation() {
        let s = Searcher::new("ab", FindOptions::default())
            .expect("ok")
            .expect("some");
        assert!(s.matches_exactly("xAB", 1..3, is_word_char));
        assert!(!s.matches_exactly("xAB", 0..2, is_word_char));
        assert!(!s.matches_exactly("xAB", 1..9, is_word_char));
        let m = vec![2..4, 10..12, 20..22];
        assert_eq!(next_match(&m, 0), Some(0));
        assert_eq!(next_match(&m, 3), Some(1));
        assert_eq!(next_match(&m, 21), Some(0));
        assert_eq!(prev_match(&m, 10), Some(0));
        assert_eq!(prev_match(&m, 2), Some(2));
        assert_eq!(prev_match(&m, 30), Some(2));
        assert_eq!(next_match(&[], 0), None);
    }

    #[test]
    fn limit_is_respected() {
        let t = "a".repeat(1000);
        let s = Searcher::new("a", FindOptions::default())
            .expect("ok")
            .expect("some");
        assert_eq!(s.find_all(&t, 10, is_word_char).len(), 10);
    }
}
