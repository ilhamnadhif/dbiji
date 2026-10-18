//! Fuzzy matching for the command palette and autocomplete.
//!
//! The pattern must appear in the candidate as a case-insensitive
//! subsequence. Among all alignments the best one is chosen (dynamic
//! programming in the style of fzf): every matched character scores, and
//! bonuses reward matches at the start, after word boundaries (`_`, `.`,
//! `-`, `/`, space), on camelCase humps and in consecutive runs, while gaps
//! between matched characters are penalised.

const SCORE_MATCH: i32 = 16;
const GAP_START: i32 = -3;
const GAP_EXTENSION: i32 = -1;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_CAMEL: i32 = 7;
const BONUS_DELIMITER: i32 = 4;
const BONUS_CONSECUTIVE: i32 = 4;
const BONUS_FIRST_CHAR_MULTIPLIER: i32 = 2;
const NEG: i32 = i32::MIN / 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Delimiter,
    Other,
}

fn class_of(c: char) -> Class {
    if c.is_lowercase() {
        Class::Lower
    } else if c.is_uppercase() {
        Class::Upper
    } else if c.is_numeric() {
        Class::Digit
    } else if c.is_whitespace()
        || matches!(
            c,
            '_' | '.' | '-' | '/' | ':' | '\\' | ',' | '(' | ')' | '"' | '`' | '[' | ']'
        )
    {
        Class::Delimiter
    } else if c.is_alphabetic() {
        Class::Lower
    } else {
        Class::Other
    }
}

fn bonus(prev: Option<Class>, cur: Class) -> i32 {
    match (prev, cur) {
        (_, Class::Delimiter) => BONUS_DELIMITER,
        (None, _) => BONUS_BOUNDARY,
        (Some(Class::Delimiter) | Some(Class::Other), _) => BONUS_BOUNDARY,
        (Some(Class::Lower), Class::Upper) => BONUS_CAMEL,
        (Some(Class::Lower) | Some(Class::Upper), Class::Digit) => BONUS_CAMEL,
        _ => 0,
    }
}

#[inline]
fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

struct Prepared {
    chars: Vec<char>,
    bonus: Vec<i32>,
}

fn prepare(candidate: &str) -> Prepared {
    let mut chars = Vec::with_capacity(candidate.len());
    let mut bonuses = Vec::with_capacity(candidate.len());
    let mut prev = None;
    for c in candidate.chars() {
        let cls = class_of(c);
        bonuses.push(bonus(prev, cls));
        chars.push(fold(c));
        prev = Some(cls);
    }
    Prepared {
        chars,
        bonus: bonuses,
    }
}

fn is_subsequence(pat: &[char], cand: &[char]) -> bool {
    let mut it = cand.iter();
    pat.iter().all(|p| it.any(|c| c == p))
}

/// Score `candidate` against `pattern`; `None` when the pattern is not a
/// case-insensitive subsequence. Higher is better; an empty pattern matches
/// everything with score 0.
pub fn fuzzy_score(pattern: &str, candidate: &str) -> Option<i32> {
    align(pattern, candidate, false).map(|(s, _)| s)
}

/// Character indices (not byte offsets) of the best alignment of `pattern`
/// in `candidate`, for highlighting; `None` when it does not match.
pub fn fuzzy_indices(pattern: &str, candidate: &str) -> Option<Vec<usize>> {
    align(pattern, candidate, true).map(|(_, idx)| idx)
}

fn align(pattern: &str, candidate: &str, want_indices: bool) -> Option<(i32, Vec<usize>)> {
    let pat: Vec<char> = pattern
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold)
        .collect();
    if pat.is_empty() {
        return Some((0, Vec::new()));
    }
    let prep = prepare(candidate);
    let n = prep.chars.len();
    let m = pat.len();
    if m > n || !is_subsequence(&pat, &prep.chars) {
        return None;
    }
    // score[i * n + j]: best score with pat[i] matched at cand[j].
    // from[i * n + j]: index of cand matched by pat[i-1] in that alignment.
    let mut score = vec![NEG; m * n];
    let mut from = if want_indices {
        vec![usize::MAX; m * n]
    } else {
        Vec::new()
    };
    for ((s, &c), &b) in score.iter_mut().zip(&prep.chars).zip(&prep.bonus) {
        if c == pat[0] {
            *s = SCORE_MATCH + b * BONUS_FIRST_CHAR_MULTIPLIER;
        }
    }
    for i in 1..m {
        let (prev_row, cur_row) = score.split_at_mut(i * n);
        let prev_row = &prev_row[(i - 1) * n..];
        let cur_row = &mut cur_row[..n];
        // Best `prev_row[k] + gap(j - k - 1)` over k <= j - 2, maintained with
        // the affine gap recurrence.
        let mut gap_best = NEG;
        let mut gap_from = usize::MAX;
        for j in 0..n {
            if j >= 2 {
                let open = prev_row[j - 2].saturating_add(GAP_START);
                let extend = gap_best.saturating_add(GAP_EXTENSION);
                if open >= extend {
                    gap_best = open;
                    gap_from = j - 2;
                } else {
                    gap_best = extend;
                }
            }
            if prep.chars[j] != pat[i] || j < i {
                continue;
            }
            let consecutive = if j >= 1 && prev_row[j - 1] > NEG {
                prev_row[j - 1] + BONUS_CONSECUTIVE.max(prep.bonus[j])
            } else {
                NEG
            };
            let (best, src) = if consecutive >= gap_best {
                (consecutive, j.wrapping_sub(1))
            } else {
                (gap_best, gap_from)
            };
            if best <= NEG {
                continue;
            }
            cur_row[j] = best + SCORE_MATCH + prep.bonus[j];
            if want_indices {
                from[i * n + j] = src;
            }
        }
    }
    let last = &score[(m - 1) * n..];
    let (best_j, best) = last
        .iter()
        .copied()
        .enumerate()
        .max_by_key(|&(j, s)| (s, std::cmp::Reverse(j)))?;
    if best <= NEG {
        return None;
    }
    // Mild preference for shorter candidates.
    let total = best - ((n - m) as i32) / 4;
    let mut indices = Vec::new();
    if want_indices {
        indices = vec![0; m];
        let mut j = best_j;
        for i in (0..m).rev() {
            indices[i] = j;
            if i > 0 {
                j = from[i * n + j];
                if j == usize::MAX {
                    return None;
                }
            }
        }
    }
    Some((total, indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_and_rejects() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert!(fuzzy_score("usr", "users").is_some());
        assert!(fuzzy_score("USR", "users").is_some());
        assert!(fuzzy_score("xyz", "users").is_none());
        assert!(fuzzy_score("userss", "users").is_none());
        assert!(fuzzy_score("ü", "Über").is_some());
    }

    #[test]
    fn prefix_and_boundaries_rank_higher() {
        let prefix = fuzzy_score("ord", "orders").unwrap();
        let middle = fuzzy_score("ord", "user_orders").unwrap();
        let scattered = fuzzy_score("ord", "colour_rd_d").unwrap();
        assert!(prefix > middle, "{prefix} {middle}");
        assert!(middle > scattered, "{middle} {scattered}");
        let boundary = fuzzy_score("oi", "order_items").unwrap();
        let inner = fuzzy_score("oi", "notice").unwrap();
        assert!(boundary > inner, "{boundary} {inner}");
        let camel = fuzzy_score("oi", "orderItems").unwrap();
        assert!(camel > inner);
        let consecutive = fuzzy_score("use", "users").unwrap();
        let gapped = fuzzy_score("use", "ulster_e").unwrap();
        assert!(consecutive > gapped);
    }

    #[test]
    fn indices() {
        assert_eq!(fuzzy_indices("oi", "order_items"), Some(vec![0, 6]));
        assert_eq!(fuzzy_indices("usr", "users"), Some(vec![0, 1, 3]));
        assert_eq!(fuzzy_indices("ab", "xaxb"), Some(vec![1, 3]));
        assert_eq!(fuzzy_indices("", "x"), Some(vec![]));
        assert_eq!(fuzzy_indices("zz", "z"), None);
        // char indices, not bytes
        assert_eq!(fuzzy_indices("éb", "aébc"), Some(vec![1, 2]));
        // the best alignment is not the leftmost one
        assert_eq!(fuzzy_indices("ui", "fluid_ui"), Some(vec![6, 7]));
    }

    #[test]
    fn shorter_wins_ties() {
        assert!(
            fuzzy_score("user", "users").unwrap() > fuzzy_score("user", "users_archive").unwrap()
        );
    }
}
