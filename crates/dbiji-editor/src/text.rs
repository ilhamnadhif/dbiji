//! Text primitives shared by the editor: char-boundary safe offset helpers,
//! an incrementally maintained line index (byte and char offsets of every
//! line start), selections and word navigation.
//!
//! Everything in this module works with **byte offsets** into a `str`, the
//! convention used by the rest of DBiji. Conversions to the character
//! indices used by egui's text layout go through [`LineIndex`], which keeps
//! them `O(log lines + line length)` instead of `O(document length)`.

use std::ops::Range;

/// Largest char boundary that is `<= offset` (clamped to `text.len()`).
pub fn floor_char_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut i = offset;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary that is `>= offset` (clamped to `text.len()`).
pub fn ceil_char_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut i = offset;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Clamp `range` into `text` and snap both ends outwards onto char
/// boundaries. The result always satisfies `start <= end <= text.len()`.
pub fn clamp_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = floor_char_boundary(text, range.start.min(text.len()));
    let end = ceil_char_boundary(text, range.end.min(text.len()).max(start));
    start..end
}

/// Byte offset of the char boundary before `offset` (or 0).
pub fn prev_char_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    text[..offset]
        .char_indices()
        .next_back()
        .map_or(0, |(i, _)| i)
}

/// Byte offset of the char boundary after `offset` (or `text.len()`).
pub fn next_char_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    text[offset..]
        .chars()
        .next()
        .map_or(text.len(), |c| offset + c.len_utf8())
}

/// The char starting at byte `offset`, if any.
pub fn char_at(text: &str, offset: usize) -> Option<char> {
    if offset >= text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    text[offset..].chars().next()
}

/// The char ending at byte `offset`, if any.
pub fn char_before(text: &str, offset: usize) -> Option<char> {
    if offset == 0 || offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    text[..offset].chars().next_back()
}

/// Default identifier character class used for word navigation, whole-word
/// search and completion prefixes.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Normalise line endings to `\n` (`\r\n` and lone `\r` both become `\n`).
pub fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Line ending style detected when a document was loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LineEnding {
    /// `\n` (Unix, macOS).
    #[default]
    Lf,
    /// `\r\n` (Windows).
    CrLf,
}

impl LineEnding {
    /// Detect the dominant line ending of `text` (CRLF if any `\r\n` occurs).
    pub fn detect(text: &str) -> Self {
        if text.contains("\r\n") {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        }
    }

    /// Convert `\n`-normalised text to this line ending.
    pub fn apply<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        match self {
            LineEnding::Lf => std::borrow::Cow::Borrowed(text),
            LineEnding::CrLf => std::borrow::Cow::Owned(text.replace('\n', "\r\n")),
        }
    }
}

/// A selection in byte offsets. `anchor == cursor` is a caret.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Selection {
    /// Fixed end (where a mouse drag started / shift-selection began).
    pub anchor: usize,
    /// Moving end, where the caret is drawn.
    pub cursor: usize,
}

impl Selection {
    /// A caret (empty selection) at `offset`.
    pub fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            cursor: offset,
        }
    }

    /// A selection from `anchor` to `cursor`.
    pub fn new(anchor: usize, cursor: usize) -> Self {
        Self { anchor, cursor }
    }

    /// `true` if no text is selected.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// Sorted byte range of the selection.
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    /// Lower end of the selection.
    pub fn start(&self) -> usize {
        self.anchor.min(self.cursor)
    }

    /// Upper end of the selection.
    pub fn end(&self) -> usize {
        self.anchor.max(self.cursor)
    }

    /// Clamp both ends into `text` and onto char boundaries.
    pub fn clamped(self, text: &str) -> Self {
        Self {
            anchor: floor_char_boundary(text, self.anchor),
            cursor: floor_char_boundary(text, self.cursor),
        }
    }
}

/// Byte and char offsets of every line start, kept in sync with the text by
/// [`LineIndex::edit`].
///
/// A document always has at least one line; a trailing `\n` starts a new,
/// empty last line (like every text editor).
#[derive(Clone, Debug)]
pub struct LineIndex {
    starts: Vec<usize>,
    char_starts: Vec<usize>,
    len: usize,
    char_len: usize,
}

impl Default for LineIndex {
    fn default() -> Self {
        Self {
            starts: vec![0],
            char_starts: vec![0],
            len: 0,
            char_len: 0,
        }
    }
}

impl LineIndex {
    /// Build the index for `text` (`O(text.len())`).
    pub fn new(text: &str) -> Self {
        let mut starts = Vec::with_capacity(text.len() / 32 + 1);
        let mut char_starts = Vec::with_capacity(text.len() / 32 + 1);
        starts.push(0);
        char_starts.push(0);
        let mut chars = 0usize;
        let mut line_start = 0usize;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                chars += text[line_start..=i].chars().count();
                starts.push(i + 1);
                char_starts.push(chars);
                line_start = i + 1;
            }
        }
        chars += text[line_start..].chars().count();
        Self {
            starts,
            char_starts,
            len: text.len(),
            char_len: chars,
        }
    }

    /// Number of lines (at least 1).
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Length of the indexed text in chars.
    #[cfg(test)]
    pub fn len_chars(&self) -> usize {
        self.char_len
    }

    /// Byte offset where `line` starts (clamped to the last line).
    pub fn line_start(&self, line: usize) -> usize {
        self.starts[line.min(self.starts.len() - 1)]
    }

    /// Byte offset where `line` ends, excluding its `\n`.
    pub fn line_end(&self, line: usize) -> usize {
        let line = line.min(self.starts.len() - 1);
        match self.starts.get(line + 1) {
            Some(next) => next - 1,
            None => self.len,
        }
    }

    /// Byte range of `line`, excluding its `\n`.
    pub fn line_range(&self, line: usize) -> Range<usize> {
        self.line_start(line)..self.line_end(line)
    }

    /// Byte range of `line` including its trailing `\n` (if any).
    pub fn line_range_with_newline(&self, line: usize) -> Range<usize> {
        let line = line.min(self.starts.len() - 1);
        let end = self.starts.get(line + 1).copied().unwrap_or(self.len);
        self.starts[line]..end
    }

    /// Line containing byte `offset` (offsets past the end map to the last line).
    pub fn line_of_offset(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1)
    }

    /// Line containing char index `ch`.
    pub fn line_of_char(&self, ch: usize) -> usize {
        self.char_starts
            .partition_point(|&s| s <= ch)
            .saturating_sub(1)
    }

    /// Convert a byte offset to a char index (the offset is first snapped
    /// down onto a char boundary).
    pub fn byte_to_char(&self, text: &str, offset: usize) -> usize {
        let offset = floor_char_boundary(text, offset);
        let line = self.line_of_offset(offset);
        let start = self.starts[line].min(offset);
        self.char_starts[line] + text[start..offset].chars().count()
    }

    /// Convert a char index to a byte offset (clamped to the text length).
    pub fn char_to_byte(&self, text: &str, ch: usize) -> usize {
        if ch >= self.char_len {
            return text.len();
        }
        let line = self.line_of_char(ch);
        let start = self.starts[line];
        let rel = ch - self.char_starts[line];
        match text[start..].char_indices().nth(rel) {
            Some((i, _)) => start + i,
            None => text.len(),
        }
    }

    /// Char column of byte `offset` within its line.
    pub fn char_col(&self, text: &str, offset: usize) -> usize {
        let offset = floor_char_boundary(text, offset);
        let line = self.line_of_offset(offset);
        let start = self.starts[line].min(offset);
        text[start..offset].chars().count()
    }

    /// Byte offset of char column `col` on `line` (clamped to the line end).
    pub fn offset_of_char_col(&self, text: &str, line: usize, col: usize) -> usize {
        let range = self.line_range(line);
        let line_text = &text[range.clone()];
        match line_text.char_indices().nth(col) {
            Some((i, _)) => range.start + i,
            None => range.end,
        }
    }

    /// Update the index for replacing `range` of `old_text` with `inserted`.
    ///
    /// Must be called with the text **before** the edit is applied. `range`
    /// must lie on char boundaries of `old_text`.
    pub fn edit(&mut self, old_text: &str, range: Range<usize>, inserted: &str) {
        debug_assert!(range.start <= range.end && range.end <= old_text.len());
        let first = self.line_of_offset(range.start);
        let last = self.line_of_offset(range.end);
        let removed_chars = old_text[range.clone()].chars().count();
        let inserted_chars = inserted.chars().count();
        let byte_delta = inserted.len() as isize - (range.end - range.start) as isize;
        let char_delta = inserted_chars as isize - removed_chars as isize;

        // Char offset of `range.start`.
        let start_char =
            self.char_starts[first] + old_text[self.starts[first]..range.start].chars().count();

        let mut new_starts = Vec::new();
        let mut new_char_starts = Vec::new();
        let mut chars_seen = 0usize;
        let mut seg_start = 0usize;
        for (i, b) in inserted.bytes().enumerate() {
            if b == b'\n' {
                chars_seen += inserted[seg_start..=i].chars().count();
                seg_start = i + 1;
                new_starts.push(range.start + i + 1);
                new_char_starts.push(start_char + chars_seen);
            }
        }

        let shift = |v: usize, d: isize| -> usize { (v as isize + d).max(0) as usize };
        for s in &mut self.starts[last + 1..] {
            *s = shift(*s, byte_delta);
        }
        for s in &mut self.char_starts[last + 1..] {
            *s = shift(*s, char_delta);
        }
        self.starts.splice(first + 1..last + 1, new_starts);
        self.char_starts
            .splice(first + 1..last + 1, new_char_starts);
        self.len = shift(self.len, byte_delta);
        self.char_len = shift(self.char_len, char_delta);
    }
}

/// Range of the identifier-like word touching `offset` (the word containing
/// the char at `offset`, or ending at `offset`).
pub fn word_range_at(
    text: &str,
    offset: usize,
    is_word: impl Fn(char) -> bool,
) -> Option<Range<usize>> {
    let offset = floor_char_boundary(text, offset);
    let after = char_at(text, offset).is_some_and(&is_word);
    let before = char_before(text, offset).is_some_and(&is_word);
    if !after && !before {
        return None;
    }
    let mut start = offset;
    while let Some(c) = char_before(text, start) {
        if !is_word(c) {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = offset;
    while let Some(c) = char_at(text, end) {
        if !is_word(c) {
            break;
        }
        end += c.len_utf8();
    }
    (start < end).then_some(start..end)
}

/// Start of the run of word chars ending at `offset` (the completion prefix start).
pub fn word_start_before(text: &str, offset: usize, is_word: impl Fn(char) -> bool) -> usize {
    let mut start = floor_char_boundary(text, offset);
    while let Some(c) = char_before(text, start) {
        if !is_word(c) {
            break;
        }
        start -= c.len_utf8();
    }
    start
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Other,
}

fn class_of(c: char, is_word: &impl Fn(char) -> bool) -> CharClass {
    if c == ' ' || c == '\t' {
        CharClass::Space
    } else if is_word(c) {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

/// Target of "move one word left" (Alt+Left on macOS, Ctrl+Left elsewhere):
/// skips whitespace, then a run of word chars or a run of punctuation. At a
/// line start it moves to the end of the previous line.
pub fn prev_word_boundary(text: &str, offset: usize, is_word: impl Fn(char) -> bool) -> usize {
    let mut pos = floor_char_boundary(text, offset);
    match char_before(text, pos) {
        None => return 0,
        Some('\n') => return pos - 1,
        _ => {}
    }
    while let Some(c) = char_before(text, pos) {
        if c == '\n' || class_of(c, &is_word) != CharClass::Space {
            break;
        }
        pos -= c.len_utf8();
    }
    let Some(c) = char_before(text, pos) else {
        return pos;
    };
    if c == '\n' {
        return pos;
    }
    let class = class_of(c, &is_word);
    while let Some(c) = char_before(text, pos) {
        if c == '\n' || class_of(c, &is_word) != class {
            break;
        }
        pos -= c.len_utf8();
    }
    pos
}

/// Target of "move one word right": skips whitespace, then a run of word
/// chars or punctuation. At a line end it moves to the start of the next line.
pub fn next_word_boundary(text: &str, offset: usize, is_word: impl Fn(char) -> bool) -> usize {
    let mut pos = floor_char_boundary(text, offset);
    match char_at(text, pos) {
        None => return text.len(),
        Some('\n') => return pos + 1,
        _ => {}
    }
    while let Some(c) = char_at(text, pos) {
        if c == '\n' || class_of(c, &is_word) != CharClass::Space {
            break;
        }
        pos += c.len_utf8();
    }
    let Some(c) = char_at(text, pos) else {
        return pos;
    };
    if c == '\n' {
        return pos;
    }
    let class = class_of(c, &is_word);
    while let Some(c) = char_at(text, pos) {
        if c == '\n' || class_of(c, &is_word) != class {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// Leading whitespace (spaces and tabs) of `line`.
pub fn leading_whitespace(line: &str) -> &str {
    let end = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    &line[..end]
}

/// Visual column (tabs expanded to `tab_size` stops) of the end of `prefix`.
pub fn visual_width(prefix: &str, tab_size: usize) -> usize {
    let tab_size = tab_size.max(1);
    prefix.chars().fold(0, |col, c| {
        if c == '\t' {
            (col / tab_size + 1) * tab_size
        } else {
            col + 1
        }
    })
}

/// Common prefix length (in bytes, on a char boundary of both strings).
pub fn common_prefix_len(a: &str, b: &str) -> usize {
    let n = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| a.len().min(b.len()));
    let mut n = n;
    while n > 0 && !(a.is_char_boundary(n) && b.is_char_boundary(n)) {
        n -= 1;
    }
    n
}

/// Common suffix length (in bytes) not overlapping the first `prefix` bytes.
pub fn common_suffix_len(a: &str, b: &str, prefix: usize) -> usize {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let max = (ab.len() - prefix).min(bb.len() - prefix);
    let mut n = ab
        .iter()
        .rev()
        .zip(bb.iter().rev())
        .take(max)
        .position(|(x, y)| x != y)
        .unwrap_or(max);
    while n > 0 && !(a.is_char_boundary(a.len() - n) && b.is_char_boundary(b.len() - n)) {
        n -= 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_index(text: &str, index: &LineIndex) {
        let fresh = LineIndex::new(text);
        assert_eq!(index.starts, fresh.starts, "starts for {text:?}");
        assert_eq!(
            index.char_starts, fresh.char_starts,
            "char_starts for {text:?}"
        );
        assert_eq!(index.len, fresh.len);
        assert_eq!(index.char_len, fresh.char_len);
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)] // reversed ranges are valid (clamped) input
    fn boundaries() {
        let s = "aé👍b";
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(ceil_char_boundary(s, 2), 3);
        assert_eq!(floor_char_boundary(s, 5), 3);
        assert_eq!(ceil_char_boundary(s, 5), 7);
        assert_eq!(floor_char_boundary(s, 100), s.len());
        assert_eq!(prev_char_boundary(s, 7), 3);
        assert_eq!(next_char_boundary(s, 3), 7);
        assert_eq!(next_char_boundary(s, 8), 8);
        assert_eq!(prev_char_boundary(s, 0), 0);
        assert_eq!(clamp_range(s, 2..5), 1..7);
        assert_eq!(clamp_range(s, 9..2), 8..8);
        assert_eq!(char_at(s, 1), Some('é'));
        assert_eq!(char_at(s, 2), None);
        assert_eq!(char_before(s, 7), Some('👍'));
    }

    #[test]
    fn line_index_basics() {
        let text = "ab\ncdé\n\nx";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_count(), 4);
        assert_eq!(idx.line_range(0), 0..2);
        assert_eq!(idx.line_range(1), 3..7);
        assert_eq!(idx.line_range(2), 8..8);
        assert_eq!(idx.line_range(3), 9..10);
        assert_eq!(idx.line_of_offset(0), 0);
        assert_eq!(idx.line_of_offset(2), 0);
        assert_eq!(idx.line_of_offset(3), 1);
        assert_eq!(idx.line_of_offset(10), 3);
        assert_eq!(idx.line_of_offset(1000), 3);
        assert_eq!(idx.len_chars(), 9);
        for (b, c) in [(0, 0), (3, 3), (5, 5), (7, 6), (8, 7), (9, 8), (10, 9)] {
            assert_eq!(idx.byte_to_char(text, b), c, "byte {b}");
            assert_eq!(idx.char_to_byte(text, c), b, "char {c}");
        }
        // Not on a boundary: snapped down.
        assert_eq!(idx.byte_to_char(text, 6), 5);
        assert_eq!(idx.char_to_byte(text, 99), text.len());
        assert_eq!(idx.char_col(text, 7), 3);
        assert_eq!(idx.offset_of_char_col(text, 1, 2), 5);
        assert_eq!(idx.offset_of_char_col(text, 1, 99), 7);
        assert_eq!(LineIndex::new("").line_count(), 1);
        assert_eq!(LineIndex::new("a\n").line_count(), 2);
    }

    #[test]
    fn line_index_incremental_edits() {
        let cases: &[(&str, Range<usize>, &str)] = &[
            ("hello\nworld", 2..2, "XY"),
            ("hello\nworld", 5..6, ""),
            ("hello\nworld", 5..5, "\n\n"),
            ("hello\nworld\n", 0..12, ""),
            ("", 0..0, "a\nb\nc"),
            ("aé\n👍b\nc", 1..8, "zz\nq"),
            ("one\ntwo\nthree", 4..8, "2\n"),
            ("x\ny", 3..3, "\n"),
            ("ab", 1..2, "é\n🎉\n"),
        ];
        for (text, range, ins) in cases {
            let mut idx = LineIndex::new(text);
            idx.edit(text, range.clone(), ins);
            let mut new = text.to_string();
            new.replace_range(range.clone(), ins);
            check_index(&new, &idx);
        }
    }

    #[test]
    fn line_index_random_edits_match_rebuild() {
        let mut text = String::from("SELECT *\nFROM t\nWHERE ünï = 'ç';\n");
        let mut idx = LineIndex::new(&text);
        let mut seed = 12345u64;
        let mut rnd = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n.max(1)
        };
        let pieces = ["", "a", "\n", "é", "👍\n", "xy\nz", "\n\n"];
        for _ in 0..500 {
            let a = floor_char_boundary(&text, rnd(text.len() + 1));
            let b = floor_char_boundary(&text, rnd(text.len() + 1));
            let range = a.min(b)..a.max(b);
            let ins = pieces[rnd(pieces.len())];
            idx.edit(&text, range.clone(), ins);
            text.replace_range(range, ins);
            check_index(&text, &idx);
        }
    }

    #[test]
    fn words() {
        let t = "SELECT foo_bar, x.y FROM  t1;";
        assert_eq!(word_range_at(t, 9, is_word_char), Some(7..14));
        assert_eq!(word_range_at(t, 14, is_word_char), Some(7..14));
        assert_eq!(word_range_at(t, 15, is_word_char), None);
        assert_eq!(word_start_before(t, 10, is_word_char), 7);
        assert_eq!(prev_word_boundary(t, 14, is_word_char), 7);
        assert_eq!(prev_word_boundary(t, 7, is_word_char), 0);
        assert_eq!(next_word_boundary(t, 0, is_word_char), 6);
        assert_eq!(next_word_boundary(t, 6, is_word_char), 14);
        assert_eq!(next_word_boundary(t, 14, is_word_char), 15);
        assert_eq!(next_word_boundary(t, 20, is_word_char), 24);
        let t2 = "ab\n  cd";
        assert_eq!(prev_word_boundary(t2, 5, is_word_char), 3);
        assert_eq!(prev_word_boundary(t2, 3, is_word_char), 2);
        assert_eq!(next_word_boundary(t2, 2, is_word_char), 3);
        assert_eq!(next_word_boundary(t2, 3, is_word_char), 7);
        assert_eq!(prev_word_boundary("", 0, is_word_char), 0);
        assert_eq!(next_word_boundary("é", 0, is_word_char), 2);
    }

    #[test]
    fn misc_helpers() {
        assert_eq!(leading_whitespace("  \tab "), "  \t");
        assert_eq!(visual_width("  \t", 4), 4);
        assert_eq!(visual_width("\t\t", 4), 8);
        assert_eq!(visual_width("ab", 4), 2);
        assert_eq!(normalize_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(LineEnding::detect("a\r\nb"), LineEnding::CrLf);
        assert_eq!(LineEnding::CrLf.apply("a\nb"), "a\r\nb");
        assert_eq!(common_prefix_len("abcé", "abcè"), 3);
        assert_eq!(common_prefix_len("abc", "abc"), 3);
        let p = common_prefix_len("xaaay", "xaay");
        assert_eq!(p, 3);
        assert_eq!(common_suffix_len("xaaay", "xaay", p), 1);
        assert_eq!(common_suffix_len("éa", "èa", 0), 1);
        let s = Selection::new(5, 2);
        assert_eq!(s.range(), 2..5);
        assert!(!s.is_empty());
        assert_eq!(Selection::new(9, 1).clamped("aé"), Selection::new(3, 1));
    }
}
