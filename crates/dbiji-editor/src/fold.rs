//! Code folding. The language reports foldable byte ranges (multi-line
//! statements, block comments); a collapsed region keeps its first line and
//! hides the others, which get no rows in the layout. Collapsed regions are
//! remembered by their first line and follow line insertions and removals.

use crate::language::EditorLanguage;
use crate::text::LineIndex;
use std::collections::BTreeSet;
use std::ops::Range;

#[derive(Debug, Clone, Default)]
pub(crate) struct Folds {
    /// Foldable regions as (first line, last line), sorted by first line.
    pub regions: Vec<(usize, usize)>,
    /// First lines of the collapsed regions.
    pub collapsed: BTreeSet<usize>,
    /// Text version the regions were computed for.
    pub version: Option<u64>,
    /// Hidden line ranges applied to the layout.
    pub applied: Vec<Range<usize>>,
}

impl Folds {
    /// The regions were computed for another text version.
    pub fn is_stale(&self, version: u64) -> bool {
        self.version != Some(version)
    }

    /// Recompute the regions after a text change. Returns true when they changed.
    pub fn update(
        &mut self,
        text: &str,
        index: &LineIndex,
        version: u64,
        lang: &dyn EditorLanguage,
    ) -> bool {
        if self.version == Some(version) {
            return false;
        }
        self.version = Some(version);
        let mut regions: Vec<(usize, usize)> = lang
            .fold_ranges(text)
            .into_iter()
            .filter(|r| r.start < r.end && r.end <= text.len())
            .map(|r| {
                (
                    index.line_of_offset(r.start),
                    index.line_of_offset(r.end.saturating_sub(1).max(r.start)),
                )
            })
            .filter(|(a, b)| b > a)
            .collect();
        regions.sort_unstable();
        // One region per first line: the largest.
        regions.dedup_by(|b, a| {
            if a.0 == b.0 {
                a.1 = a.1.max(b.1);
                true
            } else {
                false
            }
        });
        let changed = regions != self.regions;
        self.regions = regions;
        let starts: BTreeSet<usize> = self.regions.iter().map(|r| r.0).collect();
        self.collapsed.retain(|l| starts.contains(l));
        changed
    }

    /// Lines `first .. first + old` were replaced by `new` lines.
    pub fn on_splice(&mut self, first: usize, old: usize, new: usize) {
        let delta = new as isize - old as isize;
        let shift = |l: usize| (l as isize + delta) as usize;
        // Regions are shifted until they are recomputed (deferred for big documents).
        let before = std::mem::take(&mut self.regions);
        self.regions = before
            .iter()
            .filter_map(|&(a, b)| {
                if b < first {
                    Some((a, b))
                } else if a >= first + old {
                    Some((shift(a), shift(b)))
                } else {
                    None
                }
            })
            .collect();
        let regions = before;
        self.collapsed = std::mem::take(&mut self.collapsed)
            .into_iter()
            .filter_map(|l| {
                if l < first {
                    // Edits inside a collapsed region open it.
                    let end = regions.iter().find(|r| r.0 == l).map(|r| r.1).unwrap_or(l);
                    (end < first).then_some(l)
                } else if l >= first + old {
                    Some((l as isize + delta) as usize)
                } else {
                    None
                }
            })
            .collect();
        self.version = None;
    }

    /// The region starting at `line`.
    pub fn region_at(&self, line: usize) -> Option<(usize, usize)> {
        self.regions
            .binary_search_by_key(&line, |r| r.0)
            .ok()
            .map(|i| self.regions[i])
    }

    /// Hidden line ranges (lines after the first of each collapsed region).
    pub fn hidden(&self) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        for &start in &self.collapsed {
            let Some((a, b)) = self.region_at(start) else {
                continue;
            };
            let r = a + 1..b + 1;
            match out.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => out.push(r),
            }
        }
        out
    }

    /// Collapse or expand the region starting at `line`; false when none starts there.
    pub fn toggle(&mut self, line: usize) -> bool {
        if self.region_at(line).is_none() {
            return false;
        }
        if !self.collapsed.remove(&line) {
            self.collapsed.insert(line);
        }
        true
    }

    /// Expand every collapsed region that hides `line`.
    pub fn reveal(&mut self, line: usize) -> bool {
        let before = self.collapsed.len();
        let regions = &self.regions;
        self.collapsed.retain(|&s| {
            regions
                .iter()
                .find(|r| r.0 == s)
                .is_none_or(|r| !(s < line && line <= r.1))
        });
        self.collapsed.len() != before
    }

    /// The innermost region containing `line`.
    pub fn region_containing(&self, line: usize) -> Option<(usize, usize)> {
        self.regions
            .iter()
            .filter(|r| r.0 <= line && line <= r.1)
            .max_by_key(|r| r.0)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_ranges_and_edits() {
        let mut f = Folds {
            regions: vec![(0, 3), (1, 2), (5, 9)],
            ..Default::default()
        };
        f.collapsed.insert(1);
        f.collapsed.insert(5);
        assert_eq!(f.hidden(), vec![2..3, 6..10]);
        f.collapsed.insert(0);
        assert_eq!(f.hidden(), vec![1..4, 6..10], "nested ranges merge");
        // Two lines inserted at line 4: the region at 5 moves to 7.
        f.on_splice(4, 1, 3);
        assert!(f.collapsed.contains(&7) && f.collapsed.contains(&0) && f.collapsed.contains(&1));
        // An edit inside the collapsed region 0..=3 opens it.
        f.on_splice(2, 1, 1);
        assert!(!f.collapsed.contains(&0));
        assert!(!f.collapsed.contains(&1) && !f.collapsed.contains(&2));
        f.regions = vec![(7, 11)];
        assert!(f.reveal(9));
        assert!(f.collapsed.is_empty());
        assert!(f.toggle(7) && f.collapsed.contains(&7));
        assert!(!f.toggle(8));
        assert_eq!(f.region_containing(9), Some((7, 11)));
    }
}
