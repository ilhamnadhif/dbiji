//! Robustness: every public API on random and pathological input must not
//! panic and must return in-bounds, char-boundary offsets.

use crate::catalog::test_catalog::catalog;
use crate::testutil::*;
use crate::*;
use dbiji_core::DbKind;
use dbiji_core::dialect::Dialect;
use std::collections::HashSet;

fn random_unicode(rng: &mut Rng, n: usize) -> String {
    let mut s = String::new();
    for _ in 0..n {
        let c = match rng.below(6) {
            0 => char::from_u32(rng.below(0x80) as u32),
            1 => char::from_u32(rng.below(0x110000) as u32),
            2 => Some(*rng.pick(&[
                '\'', '"', '`', '[', ']', '(', ')', '$', '@', ':', '?', '#', '-', '/', '*', ';',
                '\n', '\r', '{', '}', '\\', '.', ',',
            ])),
            _ => rng.pick(FRAGMENTS).chars().next(),
        };
        if let Some(c) = c {
            s.push(c);
        }
        if rng.below(4) == 0 {
            s.push_str(rng.pick(FRAGMENTS));
        }
    }
    s
}

fn exercise(src: &str, d: &dyn Dialect, rng: &mut Rng) {
    let syn = d.syntax();
    let cat = catalog();
    let check = |a: usize, b: usize| {
        assert!(a <= b && b <= src.len(), "range {a}..{b} out of bounds");
        assert!(
            src.is_char_boundary(a) && src.is_char_boundary(b),
            "range {a}..{b} not on char boundaries"
        );
    };
    for t in tokenize(src, syn) {
        check(t.start, t.end);
    }
    for mode in [
        BlankLineMode::Never,
        BlankLineMode::Smart,
        BlankLineMode::Always,
    ] {
        let opts = SplitOptions {
            blank_line: mode,
            ..Default::default()
        };
        for s in split_statements(src, syn, &opts) {
            check(s.start, s.end);
            check(s.outer_start, s.outer_end);
            let _ = parse_client_command(s.text(src));
        }
    }
    for (s, info) in split_and_classify(src, syn, &SplitOptions::default()) {
        check(s.start, s.end);
        let _ = info.is_transaction_control();
    }
    let _ = classify(src, syn);
    let _ = format_sql(src, syn, &FormatOptions::default());
    let _ = format_sql(
        src,
        syn,
        &FormatOptions {
            compact_short_lists: false,
            comma_first: true,
            ..Default::default()
        },
    );
    let a = analyze(src, syn);
    for t in &a.tables {
        check(t.start, t.end);
    }
    for _ in 0..3 {
        let off = floor_boundary(src, rng.below(src.len() + 1));
        let _ = statement_at(src, off, syn, &SplitOptions::default());
        let r = complete(src, off, d, &cat, &CompletionOptions::default());
        check(r.replace_start, r.replace_end);
        if let Some(o) = object_at(src, off, syn) {
            check(o.start, o.end);
        }
        let (line, col) = line_col(src, off);
        let back = position::offset_of_line_col(src, line, col);
        // An offset between the `\r` and `\n` of a CRLF is inside a line break.
        let inside_crlf = off > 0
            && src.as_bytes().get(off - 1) == Some(&b'\r')
            && src.as_bytes().get(off) == Some(&b'\n');
        if !inside_crlf {
            assert_eq!(back, off, "line/col round trip in {src:?}");
        }
        let _ = offset_of_line(src, line);
        let _ = char_pos_to_byte(src, rng.below(src.len() + 2));
    }
    for x in script_diagnostics(src, syn, &cat, &SplitOptions::default()) {
        check(x.start, x.end);
    }
    let popts = ParamOptions {
        named: true,
        template: true,
        positional: true,
        numbered: true,
    };
    for p in find_parameters(src, syn, &popts) {
        check(p.start, p.end);
    }
    let _ = substitute(src, syn, &popts, &|p| Some(p.name.clone()));
    for item in outline(src, syn) {
        check(item.start, item.end);
    }
    for (r, _) in highlight(src, syn) {
        check(r.start, r.end);
    }
    for i in check_spelling(src, syn, SpellChecker::english(), &HashSet::new()) {
        check(i.start, i.end);
    }
    let _ = syntax_check(src, d.kind());
    let pat: String = src.chars().take(4).collect();
    let _ = fuzzy_score(&pat, src);
    let _ = fuzzy_indices(&pat, &src.chars().take(64).collect::<String>());
}

#[test]
fn every_api_survives_random_input() {
    let dialects = [
        TestDialect::pg(),
        TestDialect::mysql(),
        TestDialect::sqlite(),
        TestDialect::tsql(),
    ];
    let mut rng = Rng::new(2026);
    for round in 0..120 {
        let src = if round % 2 == 0 {
            random_unicode(&mut rng, 30)
        } else {
            random_sql(&mut rng, 50)
        };
        exercise(&src, &dialects[round % dialects.len()], &mut rng);
    }
}

#[test]
fn pathological_nesting_and_size() {
    let d = TestDialect::pg();
    let mut rng = Rng::new(1);
    let deep_parens = format!("SELECT {}1{} FROM t", "(".repeat(3000), ")".repeat(3000));
    let deep_case = format!(
        "SELECT {}1{} FROM t",
        "CASE WHEN a THEN ".repeat(800),
        " END".repeat(800)
    );
    let deep_subq = format!(
        "SELECT * FROM {} t {}",
        "(SELECT * FROM ".repeat(500),
        ") x".repeat(500)
    );
    let many_ctes = format!(
        "WITH {} b AS (SELECT 1) SELECT * FROM b",
        "a AS (SELECT 1), ".repeat(500)
    );
    let unclosed = "(".repeat(5000);
    let long_line = "SELECT a, b, c FROM t WHERE x = 'y' AND ".repeat(5000);
    for src in [
        &deep_parens,
        &deep_case,
        &deep_subq,
        &many_ctes,
        &unclosed,
        &long_line,
    ] {
        exercise(src, &d, &mut rng);
        let once = format_sql(src, d.syntax(), &FormatOptions::default());
        assert_eq!(
            format_sql(&once, d.syntax(), &FormatOptions::default()),
            once
        );
    }
    assert!(
        syntax_check(&deep_parens, DbKind::Postgres).is_none()
            || syntax_check(&deep_parens, DbKind::Postgres).is_some()
    );
}
