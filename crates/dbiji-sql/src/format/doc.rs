//! A small Wadler-style pretty printer: groups are printed on one line when
//! they fit in the remaining width, otherwise their line breaks are taken.
//! Consecutive line breaks collapse (no blank lines are ever produced) and
//! no trailing white space is emitted.

/// Layout document.
#[derive(Debug, Clone)]
pub(crate) enum Doc {
    /// Literal text. May contain line breaks only for multi-line comments.
    Text(String),
    /// A single space (dropped at the start of a line).
    Space,
    /// A space when the enclosing group is flat, a line break otherwise.
    Line,
    /// Nothing when flat, a line break otherwise.
    SoftLine,
    /// Always a line break; forces enclosing groups to break.
    HardLine,
    /// Increase the indentation of line breaks inside.
    Indent(Box<Doc>),
    /// Print flat if it fits (and `force` is false).
    Group(Box<Doc>, bool),
    /// `broken` when the enclosing group breaks, `flat` otherwise.
    IfBreak(Box<Doc>, Box<Doc>),
    Seq(Vec<Doc>),
}

impl Doc {
    pub(crate) fn text(s: impl Into<String>) -> Doc {
        Doc::Text(s.into())
    }
    pub(crate) fn indent(d: Doc) -> Doc {
        Doc::Indent(Box::new(d))
    }
    pub(crate) fn group(d: Doc) -> Doc {
        Doc::Group(Box::new(d), false)
    }
    pub(crate) fn group_forced(d: Doc, force: bool) -> Doc {
        Doc::Group(Box::new(d), force)
    }
    pub(crate) fn if_break(broken: Doc, flat: Doc) -> Doc {
        Doc::IfBreak(Box::new(broken), Box::new(flat))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

/// Width of `d` printed flat, or `None` when it cannot be flat or exceeds `limit`.
fn flat_width(d: &Doc, limit: usize) -> Option<usize> {
    let mut total = 0usize;
    let mut stack: Vec<&Doc> = vec![d];
    while let Some(d) = stack.pop() {
        match d {
            Doc::Text(s) => {
                if s.contains('\n') {
                    return None;
                }
                total += s.chars().count();
            }
            Doc::Space | Doc::Line => total += 1,
            Doc::SoftLine => {}
            Doc::HardLine => return None,
            Doc::Indent(x) => stack.push(x),
            Doc::Group(x, force) => {
                if *force {
                    return None;
                }
                stack.push(x);
            }
            Doc::IfBreak(_, flat) => stack.push(flat),
            Doc::Seq(v) => stack.extend(v.iter().rev()),
        }
        if total > limit {
            return None;
        }
    }
    Some(total)
}

/// Whether `group` fits flat in `limit` columns together with the text that
/// follows it up to the next possible line break (`rest` is the printer
/// stack; its last element is printed next).
fn fits(group: &Doc, rest: &[(usize, Mode, &Doc)], limit: usize) -> bool {
    let Some(mut total) = flat_width(group, limit) else {
        return false;
    };
    let mut work: Vec<(Mode, &Doc)> = Vec::new();
    for &(_, mode, d) in rest.iter().rev() {
        work.push((mode, d));
        while let Some((mode, d)) = work.pop() {
            match d {
                Doc::Text(s) => {
                    if let Some(p) = s.find('\n') {
                        return total + s[..p].chars().count() <= limit;
                    }
                    total += s.chars().count();
                }
                Doc::Space => total += 1,
                Doc::Line if mode == Mode::Break => return true,
                Doc::Line => total += 1,
                Doc::SoftLine if mode == Mode::Break => return true,
                Doc::SoftLine => {}
                Doc::HardLine => return true,
                Doc::Indent(x) => work.push((mode, x)),
                Doc::Group(x, force) => work.push((if *force { Mode::Break } else { mode }, x)),
                Doc::IfBreak(broken, flat) => {
                    work.push((mode, if mode == Mode::Break { broken } else { flat }))
                }
                Doc::Seq(v) => work.extend(v.iter().rev().map(|x| (mode, x))),
            }
            if total > limit {
                return false;
            }
        }
    }
    true
}

struct Printer<'a> {
    out: String,
    unit: &'a str,
    unit_width: usize,
    width: usize,
    col: usize,
    pending_nl: Option<usize>,
    pending_space: bool,
}

impl Printer<'_> {
    fn newline(&mut self, indent: usize) {
        self.pending_nl = Some(indent);
        self.pending_space = false;
    }

    fn at_line_start(&self) -> bool {
        self.pending_nl.is_some() || self.col == 0
    }

    fn emit(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(ind) = self.pending_nl.take() {
            if !self.out.is_empty() {
                self.out.push('\n');
            }
            for _ in 0..ind {
                self.out.push_str(self.unit);
            }
            self.col = ind * self.unit_width;
        } else if self.pending_space && self.col > 0 {
            self.out.push(' ');
            self.col += 1;
        }
        self.pending_space = false;
        self.out.push_str(s);
        match s.rfind('\n') {
            Some(p) => self.col = s[p + 1..].chars().count(),
            None => self.col += s.chars().count(),
        }
    }
}

/// Render `doc` with the given indentation unit and maximum line width.
pub(crate) fn render(doc: &Doc, unit: &str, unit_width: usize, width: usize) -> String {
    let mut p = Printer {
        out: String::new(),
        unit,
        unit_width,
        width: width.max(20),
        col: 0,
        pending_nl: None,
        pending_space: false,
    };
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];
    while let Some((ind, mode, d)) = stack.pop() {
        match d {
            Doc::Text(s) => p.emit(s),
            Doc::Space => {
                if !p.at_line_start() {
                    p.pending_space = true;
                }
            }
            Doc::Line => match mode {
                Mode::Flat => {
                    if !p.at_line_start() {
                        p.pending_space = true;
                    }
                }
                Mode::Break => p.newline(ind),
            },
            Doc::SoftLine => {
                if mode == Mode::Break {
                    p.newline(ind);
                }
            }
            Doc::HardLine => p.newline(ind),
            Doc::Indent(x) => stack.push((ind + 1, mode, x)),
            Doc::Group(x, force) => {
                let start_col = if p.pending_nl.is_some() {
                    ind * p.unit_width
                } else {
                    p.col + usize::from(p.pending_space)
                };
                let remaining = p.width.saturating_sub(start_col);
                let m = if !*force && fits(x, &stack, remaining) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                stack.push((ind, m, x));
            }
            Doc::IfBreak(broken, flat) => {
                stack.push((ind, mode, if mode == Mode::Break { broken } else { flat }))
            }
            Doc::Seq(v) => {
                for x in v.iter().rev() {
                    stack.push((ind, mode, x));
                }
            }
        }
    }
    p.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(v: Vec<Doc>) -> Doc {
        Doc::Seq(v)
    }

    #[test]
    fn groups_fit_or_break() {
        let list = |n: usize| {
            let mut v = vec![Doc::text("SELECT")];
            let mut inner = Vec::new();
            for i in 0..n {
                if i > 0 {
                    inner.push(Doc::text(","));
                }
                inner.push(Doc::Line);
                inner.push(Doc::text(format!("column_{i}")));
            }
            v.push(Doc::indent(seq(inner)));
            Doc::group(seq(v))
        };
        assert_eq!(render(&list(2), "    ", 4, 80), "SELECT column_0, column_1");
        assert_eq!(
            render(&list(8), "    ", 4, 40),
            "SELECT\n    column_0,\n    column_1,\n    column_2,\n    column_3,\n    column_4,\n    column_5,\n    column_6,\n    column_7"
        );
    }

    #[test]
    fn hard_lines_force_breaks_and_collapse() {
        let d = Doc::group(seq(vec![
            Doc::text("a"),
            Doc::Line,
            Doc::text("-- c"),
            Doc::HardLine,
            Doc::HardLine,
            Doc::Line,
            Doc::text("b"),
        ]));
        assert_eq!(render(&d, "  ", 2, 80), "a\n-- c\nb");
        let d = seq(vec![
            Doc::text("x"),
            Doc::Space,
            Doc::HardLine,
            Doc::Space,
            Doc::text("y"),
        ]);
        assert_eq!(render(&d, "  ", 2, 80), "x\ny");
    }

    #[test]
    fn if_break_and_indent() {
        let d = Doc::group(seq(vec![
            Doc::text("("),
            Doc::indent(seq(vec![
                Doc::SoftLine,
                Doc::text("aaaaaaaaaa"),
                Doc::if_break(seq(vec![Doc::Line, Doc::text(", ")]), Doc::text(", ")),
                Doc::text("bbbbbbbbbb"),
            ])),
            Doc::SoftLine,
            Doc::text(")"),
        ]));
        assert_eq!(render(&d, "\t", 4, 80), "(aaaaaaaaaa, bbbbbbbbbb)");
        assert_eq!(
            render(&d, "\t", 4, 20).replace('\t', "T"),
            "(\nTaaaaaaaaaa\nT, bbbbbbbbbb\n)"
        );
        let forced = Doc::group_forced(seq(vec![Doc::text("a"), Doc::Line, Doc::text("b")]), true);
        assert_eq!(render(&forced, " ", 1, 80), "a\nb");
    }

    #[test]
    fn multiline_text_does_not_fit() {
        let d = Doc::group(seq(vec![
            Doc::text("a"),
            Doc::Line,
            Doc::text("/* x\n y */"),
            Doc::Line,
            Doc::text("b"),
        ]));
        assert_eq!(render(&d, " ", 1, 80), "a\n/* x\n y */\nb");
    }
}
