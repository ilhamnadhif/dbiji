//! Fast case-insensitive lookup in the static word lists of a
//! [`SqlSyntax`](dbiji_core::SqlSyntax) (keywords, functions, data types).
//!
//! `SqlSyntax::is_keyword` scans the whole list for every word, which is far
//! too slow for tokenizing large scripts on every keystroke. Word lists are
//! `&'static` slices, so their address identifies them: hash sets built from
//! them are cached per thread and reused by every call.

use std::cell::RefCell;
use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

/// Small, fast, non-cryptographic hasher (FxHash) for short ASCII words.
#[derive(Default, Clone, Copy)]
pub(crate) struct FxHasher(u64);

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        for &b in rest {
            self.add(b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(FX_SEED);
    }
}

/// `BuildHasher` for [`FxHasher`].
pub(crate) type FxBuild = BuildHasherDefault<FxHasher>;

/// Longest word that can be a member of a [`WordSet`]; longer words are
/// never keywords and are rejected without hashing.
const MAX_WORD: usize = 64;

/// Case-insensitive set of ASCII words.
#[derive(Debug, Default)]
pub(crate) struct WordSet {
    words: HashSet<Box<[u8]>, FxBuild>,
}

impl WordSet {
    /// Build from words; entries are stored upper case. With `split_words`,
    /// every space separated part of multi-word entries (`DOUBLE PRECISION`)
    /// is added as well.
    pub(crate) fn build(list: &[&str], split_words: bool) -> WordSet {
        let mut words: HashSet<Box<[u8]>, FxBuild> =
            HashSet::with_capacity_and_hasher(list.len() * 2, FxBuild::default());
        for w in list {
            let upper = w.trim().to_ascii_uppercase();
            if upper.is_empty() {
                continue;
            }
            if split_words && upper.contains(' ') {
                for part in upper.split_whitespace() {
                    words.insert(part.as_bytes().to_vec().into_boxed_slice());
                }
            }
            words.insert(upper.into_bytes().into_boxed_slice());
        }
        WordSet { words }
    }

    /// Case-insensitive membership test.
    #[inline]
    pub(crate) fn contains(&self, word: &str) -> bool {
        let b = word.as_bytes();
        if b.is_empty() || b.len() > MAX_WORD {
            return false;
        }
        let mut buf = [0u8; MAX_WORD];
        for (i, &c) in b.iter().enumerate() {
            if !c.is_ascii() {
                return false;
            }
            buf[i] = c.to_ascii_uppercase();
        }
        self.words.contains(&buf[..b.len()])
    }

    /// Number of distinct entries.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.words.len()
    }
}

type CacheEntry = (usize, usize, bool, Rc<WordSet>);

const CACHE_CAP: usize = 24;

thread_local! {
    static CACHE: RefCell<Vec<CacheEntry>> = const { RefCell::new(Vec::new()) };
}

/// Cached [`WordSet`] for a static word list.
pub(crate) fn word_set(list: &'static [&'static str], split_words: bool) -> Rc<WordSet> {
    let key = (list.as_ptr() as usize, list.len(), split_words);
    let cached = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let pos = c.iter().position(|e| (e.0, e.1, e.2) == key)?;
        // Move to the front so hot entries stay cached.
        let entry = c.remove(pos);
        let set = entry.3.clone();
        c.insert(0, entry);
        Some(set)
    });
    if let Some(set) = cached {
        return set;
    }
    let set = Rc::new(WordSet::build(list, split_words));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.insert(0, (key.0, key.1, key.2, set.clone()));
        c.truncate(CACHE_CAP);
    });
    set
}

/// Lookup tables for one syntax, built once per call site.
pub(crate) struct Lexicon {
    pub keywords: Rc<WordSet>,
    pub functions: Rc<WordSet>,
    pub data_types: Rc<WordSet>,
}

impl Lexicon {
    pub(crate) fn new(syntax: &dbiji_core::SqlSyntax) -> Lexicon {
        Lexicon {
            keywords: word_set(syntax.keywords, false),
            functions: word_set(syntax.functions, false),
            data_types: word_set(syntax.data_types, true),
        }
    }
}

/// Words reserved by SQL:2016 (from `dbiji_core::keywords::RESERVED`).
pub(crate) fn reserved() -> Rc<WordSet> {
    word_set(dbiji_core::keywords::RESERVED, false)
}

/// Case-insensitive equality for identifiers (ASCII fast path, Unicode aware).
#[inline]
pub(crate) fn ident_eq(a: &str, b: &str) -> bool {
    if a.len() == b.len() && a.eq_ignore_ascii_case(b) {
        return true;
    }
    if a.is_ascii() && b.is_ascii() {
        return false;
    }
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

#[cfg(test)]
mod tests {
    use super::*;

    static LIST: &[&str] = &["SELECT", "from", "Double Precision"];

    #[test]
    fn case_insensitive_lookup() {
        let s = word_set(LIST, true);
        assert!(s.contains("select"));
        assert!(s.contains("SeLeCt"));
        assert!(s.contains("FROM"));
        assert!(s.contains("double"));
        assert!(s.contains("precision"));
        assert!(s.contains("double precision"));
        assert!(!s.contains("selects"));
        assert!(!s.contains(""));
        assert!(!s.contains("sélect"));
        assert!(!s.contains(&"x".repeat(200)));
        let n = word_set(LIST, false);
        assert!(!n.contains("double"));
        assert_eq!(n.len(), 3);
    }

    #[test]
    fn cache_reuses_sets() {
        let a = word_set(LIST, false);
        let b = word_set(LIST, false);
        assert!(Rc::ptr_eq(&a, &b));
    }

    #[test]
    fn ident_equality() {
        assert!(ident_eq("Users", "users"));
        assert!(!ident_eq("users", "user"));
        assert!(ident_eq("ÄBC", "äbc"));
        assert!(!ident_eq("äbc", "abc"));
    }

    #[test]
    fn hasher_distinguishes() {
        use std::hash::BuildHasher;
        let b = FxBuild::default();
        assert_ne!(
            b.hash_one(b"SELECT".as_slice()),
            b.hash_one(b"SELECTS".as_slice())
        );
    }
}
