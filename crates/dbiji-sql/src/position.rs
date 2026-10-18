//! Conversions between byte offsets, line/column positions and character
//! positions. Lines are separated by `\n`, `\r\n` or a lone `\r`; lines and
//! columns are 0-based and columns count characters (not bytes).

/// Largest character boundary `<= offset`, clamped to `src.len()`.
pub fn floor_char_boundary(src: &str, offset: usize) -> usize {
    let mut i = offset.min(src.len());
    while i > 0 && !src.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest character boundary `>= offset`, clamped to `src.len()`.
pub fn ceil_char_boundary(src: &str, offset: usize) -> usize {
    let mut i = offset.min(src.len());
    while i < src.len() && !src.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 0-based `(line, column)` of byte offset `byte` (clamped into `src`).
/// The column is the number of characters between the start of the line and
/// the offset.
pub fn line_col(src: &str, byte: usize) -> (usize, usize) {
    let byte = floor_char_boundary(src, byte);
    let b = src.as_bytes();
    let mut line = 0;
    let mut line_start = 0;
    let mut i = 0;
    while i < byte {
        match b[i] {
            b'\n' => {
                line += 1;
                line_start = i + 1;
            }
            b'\r' if b.get(i + 1) != Some(&b'\n') => {
                line += 1;
                line_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    (line, src[line_start..byte].chars().count())
}

/// Byte offset where 0-based `line` starts; `src.len()` when the text has
/// fewer lines.
pub fn offset_of_line(src: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let b = src.as_bytes();
    let mut seen = 0;
    let mut i = 0;
    while i < b.len() {
        let brk = match b[i] {
            b'\n' => true,
            b'\r' => b.get(i + 1) != Some(&b'\n'),
            _ => false,
        };
        i += 1;
        if brk {
            seen += 1;
            if seen == line {
                return i;
            }
        }
    }
    src.len()
}

/// Byte offset of 0-based (`line`, `col`), with `col` counted in characters
/// and clamped to the end of the line.
pub fn offset_of_line_col(src: &str, line: usize, col: usize) -> usize {
    let start = offset_of_line(src, line);
    let rest = &src[start..];
    let line_len = rest.find(['\n', '\r']).unwrap_or(rest.len());
    let line_text = &rest[..line_len];
    start
        + line_text
            .char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(line_len)
}

/// Byte offset of a 1-based character position inside a statement, as
/// reported by PostgreSQL errors (`position`). Positions `<= 1` map to `0`,
/// positions past the end to `stmt.len()`.
pub fn char_pos_to_byte(stmt: &str, char_pos_1based: usize) -> usize {
    if char_pos_1based <= 1 {
        return 0;
    }
    stmt.char_indices()
        .nth(char_pos_1based - 1)
        .map(|(i, _)| i)
        .unwrap_or(stmt.len())
}

/// Inverse of [`char_pos_to_byte`]: 1-based character position of `byte`.
pub fn byte_to_char_pos(stmt: &str, byte: usize) -> usize {
    stmt[..floor_char_boundary(stmt, byte)].chars().count() + 1
}

/// Byte offset of 1-based (`line`, `column`) where lines are separated by
/// `\n` only and columns count characters, the convention of the `sqlparser`
/// crate. Clamped to the end of the line / text.
pub(crate) fn offset_of_line_col_lf(src: &str, line_1: usize, col_1: usize) -> usize {
    let mut start = 0;
    for _ in 1..line_1.max(1) {
        match src[start..].find('\n') {
            Some(p) => start += p + 1,
            None => return src.len(),
        }
    }
    let rest = &src[start..];
    let line_len = rest.find('\n').unwrap_or(rest.len());
    start
        + rest[..line_len]
            .char_indices()
            .nth(col_1.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(line_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column() {
        let s = "ab\ncdé\r\nf\rg";
        assert_eq!(line_col(s, 0), (0, 0));
        assert_eq!(line_col(s, 2), (0, 2));
        assert_eq!(line_col(s, 3), (1, 0));
        assert_eq!(line_col(s, 7), (1, 3)); // after é (2 bytes)
        assert_eq!(line_col(s, 9), (2, 0));
        assert_eq!(line_col(s, 11), (3, 0));
        assert_eq!(line_col(s, 999), (3, 1));
        // inside a multi-byte char: floor to boundary
        assert_eq!(line_col(s, 6), (1, 2));
    }

    #[test]
    fn line_offsets() {
        let s = "ab\ncdé\r\nf\rg";
        assert_eq!(offset_of_line(s, 0), 0);
        assert_eq!(offset_of_line(s, 1), 3);
        assert_eq!(offset_of_line(s, 2), 9);
        assert_eq!(offset_of_line(s, 3), 11);
        assert_eq!(offset_of_line(s, 4), s.len());
        assert_eq!(offset_of_line_col(s, 1, 2), 5);
        assert_eq!(offset_of_line_col(s, 1, 3), 7);
        assert_eq!(offset_of_line_col(s, 1, 99), 7);
        assert_eq!(offset_of_line(s, 99), s.len());
        assert_eq!(offset_of_line("", 1), 0);
    }

    #[test]
    fn char_positions() {
        let s = "SELECT 'é' FROM x";
        assert_eq!(char_pos_to_byte(s, 0), 0);
        assert_eq!(char_pos_to_byte(s, 1), 0);
        assert_eq!(char_pos_to_byte(s, 9), 8); // é
        assert_eq!(char_pos_to_byte(s, 10), 10); // closing quote after 2-byte é
        assert_eq!(char_pos_to_byte(s, 1000), s.len());
        assert_eq!(byte_to_char_pos(s, 10), 10);
        assert_eq!(byte_to_char_pos(s, 0), 1);
    }

    #[test]
    fn boundaries() {
        let s = "aé";
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(ceil_char_boundary(s, 2), 3);
        assert_eq!(floor_char_boundary(s, 99), 3);
    }

    #[test]
    fn sqlparser_positions() {
        let s = "SELECT\n  é x\r\nFROM";
        assert_eq!(offset_of_line_col_lf(s, 1, 1), 0);
        assert_eq!(offset_of_line_col_lf(s, 2, 3), 9);
        assert_eq!(offset_of_line_col_lf(s, 2, 4), 11);
        assert_eq!(offset_of_line_col_lf(s, 3, 1), 15);
        assert_eq!(offset_of_line_col_lf(s, 9, 1), s.len());
        assert_eq!(offset_of_line_col_lf(s, 0, 0), 0);
    }
}
