//! HTML report and ER diagram status of a schema comparison.

use crate::model::{DiffItem, DiffStatus, PropertyChange, SchemaDiff};
use crate::util::html_escape;
use dbiji_core::{ObjectKind, tr};
use std::collections::BTreeMap;
use std::fmt::Write as _;

const CSS: &str = r#"
:root {
  --bg: #ffffff; --fg: #1f2328; --muted: #656d76; --border: #d0d7de; --panel: #f6f8fa;
  --added: #1a7f37; --added-bg: #dafbe1; --removed: #cf222e; --removed-bg: #ffebe9;
  --modified: #9a6700; --modified-bg: #fff8c5; --unchanged: #57606a; --unchanged-bg: #eaeef2;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117; --fg: #e6edf3; --muted: #8d96a0; --border: #30363d; --panel: #161b22;
    --added: #3fb950; --added-bg: #12261e; --removed: #f85149; --removed-bg: #2d1214;
    --modified: #d29922; --modified-bg: #2b2111; --unchanged: #8d96a0; --unchanged-bg: #21262d;
  }
}
* { box-sizing: border-box; }
body { margin: 0; padding: 24px; background: var(--bg); color: var(--fg);
  font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif; }
h1 { font-size: 22px; margin: 0 0 4px; }
h2 { font-size: 17px; margin: 28px 0 10px; border-bottom: 1px solid var(--border); padding-bottom: 6px; }
.sides { margin: 0; font-size: 15px; }
.sides b { font-weight: 600; }
.meta { color: var(--muted); margin: 2px 0 0; font-size: 12px; }
.cards { display: flex; flex-wrap: wrap; gap: 12px; margin: 12px 0; }
.card { border: 1px solid var(--border); border-radius: 8px; padding: 10px 16px; min-width: 120px; background: var(--panel); }
.card .n { font-size: 24px; font-weight: 600; display: block; }
.card.added .n { color: var(--added); } .card.removed .n { color: var(--removed); }
.card.modified .n { color: var(--modified); } .card.unchanged .n { color: var(--unchanged); }
table { border-collapse: collapse; width: 100%; margin: 6px 0; }
th, td { border: 1px solid var(--border); padding: 4px 8px; text-align: left; vertical-align: top; }
th { background: var(--panel); font-weight: 600; }
td.num { text-align: right; font-variant-numeric: tabular-nums; }
details.item { border: 1px solid var(--border); border-radius: 8px; margin: 8px 0; padding: 6px 12px; }
details.item > summary { cursor: pointer; font-weight: 600; }
details.item.added { border-left: 4px solid var(--added); }
details.item.removed { border-left: 4px solid var(--removed); }
details.item.modified { border-left: 4px solid var(--modified); }
.badge { display: inline-block; border-radius: 10px; padding: 0 8px; font-size: 12px; font-weight: 600; margin-right: 6px; }
.badge.added { color: var(--added); background: var(--added-bg); }
.badge.removed { color: var(--removed); background: var(--removed-bg); }
.badge.modified { color: var(--modified); background: var(--modified-bg); }
.badge.unchanged { color: var(--unchanged); background: var(--unchanged-bg); }
.kind { color: var(--muted); font-weight: 400; margin-right: 6px; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 12.5px; }
pre { margin: 0; white-space: pre-wrap; word-break: break-word; }
.none { color: var(--muted); font-style: italic; }
ul.children { list-style: none; padding-left: 0; margin: 6px 0; }
ul.children > li { border-top: 1px dashed var(--border); padding: 6px 0; }
ul.unchanged { columns: 3 260px; color: var(--muted); }
.empty { color: var(--added); font-weight: 600; }
"#;

fn status_class(s: DiffStatus) -> &'static str {
    match s {
        DiffStatus::Added => "added",
        DiffStatus::Removed => "removed",
        DiffStatus::Modified => "modified",
        DiffStatus::Unchanged => "unchanged",
    }
}

fn badge(s: DiffStatus) -> String {
    format!("<span class=\"badge {}\">{}</span>", status_class(s), html_escape(tr(s.label())))
}

fn value_cell(v: &Option<String>) -> String {
    match v {
        None => format!("<td class=\"none\">{}</td>", html_escape(tr("(none)"))),
        Some(s) if s.contains('\n') || s.len() > 80 => format!("<td><pre>{}</pre></td>", html_escape(s)),
        Some(s) => format!("<td><code>{}</code></td>", html_escape(s)),
    }
}

fn changes_table(out: &mut String, changes: &[PropertyChange]) {
    if changes.is_empty() {
        return;
    }
    let _ = write!(
        out,
        "<table class=\"props\"><thead><tr><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
        html_escape(tr("Property")),
        html_escape(tr("Source")),
        html_escape(tr("Target"))
    );
    for c in changes {
        let _ = write!(out, "<tr><td>{}</td>{}{}</tr>", html_escape(&c.property), value_cell(&c.source), value_cell(&c.target));
    }
    out.push_str("</tbody></table>");
}

fn item_html(out: &mut String, item: &DiffItem) {
    let status = if item.status == DiffStatus::Unchanged && item.has_changes() { DiffStatus::Modified } else { item.status };
    let _ = write!(
        out,
        "<details class=\"item {}\" open><summary>{}<span class=\"kind\">{}</span><code>{}</code></summary>",
        status_class(status),
        badge(status),
        html_escape(tr(item.kind.label())),
        html_escape(&match &item.schema {
            Some(s) if !s.is_empty() => format!("{s}.{}", item.display_name()),
            _ => item.display_name(),
        })
    );
    changes_table(out, &item.changes);
    if !item.children.is_empty() {
        out.push_str("<ul class=\"children\">");
        for c in &item.children {
            let _ = write!(out, "<li>{}<span class=\"kind\">{}</span><code>{}</code>", badge(c.status), html_escape(tr(c.kind.label())), html_escape(&c.display_name()));
            changes_table(out, &c.changes);
            out.push_str("</li>");
        }
        out.push_str("</ul>");
    }
    out.push_str("</details>\n");
}

/// Self-contained HTML report (embedded CSS, no external resources) with
/// summary counts, per-object details and colour-coded statuses. All
/// database supplied text is HTML-escaped.
pub fn html_report(diff: &SchemaDiff, source_label: &str, target_label: &str) -> String {
    let mut out = String::with_capacity(16 * 1024);
    let title = format!("{}: {} \u{2192} {}", tr("Schema comparison"), source_label, target_label);
    let _ = write!(
        out,
        "<!DOCTYPE html>\n<html lang=\"{}\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>{}</style>\n</head>\n<body>\n",
        dbiji_core::i18n::locale().code(),
        html_escape(&title),
        CSS
    );
    let _ = writeln!(
        out,
        "<header><h1>{}</h1><p class=\"sides\"><b>{}:</b> {} &#8594; <b>{}:</b> {}</p><p class=\"meta\">{} &middot; {}</p></header>",
        html_escape(tr("Schema comparison")),
        html_escape(tr("Source")),
        html_escape(source_label),
        html_escape(tr("Target")),
        html_escape(target_label),
        html_escape(tr("Generated by DBiji")),
        html_escape(&chrono::Local::now().format("%Y-%m-%d %H:%M").to_string())
    );

    // summary
    let t = diff.summary.total;
    let _ = write!(out, "<section class=\"summary\"><h2>{}</h2><div class=\"cards\">", html_escape(tr("Summary")));
    for (s, n) in [(DiffStatus::Added, t.added), (DiffStatus::Removed, t.removed), (DiffStatus::Modified, t.modified), (DiffStatus::Unchanged, t.unchanged)] {
        let _ = write!(out, "<div class=\"card {}\"><span class=\"n\">{n}</span>{}</div>", status_class(s), html_escape(tr(s.label())));
    }
    out.push_str("</div>");
    let _ = write!(
        out,
        "<table class=\"counts\"><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th></tr></thead><tbody>",
        html_escape(tr("Object type")),
        html_escape(tr("Added")),
        html_escape(tr("Removed")),
        html_escape(tr("Modified")),
        html_escape(tr("Unchanged"))
    );
    for (kind, c) in &diff.summary.by_kind {
        let _ = write!(
            out,
            "<tr><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            html_escape(tr(kind.plural_label())),
            c.added,
            c.removed,
            c.modified,
            c.unchanged
        );
    }
    out.push_str("</tbody></table></section>\n");

    // details
    let _ = writeln!(out, "<section class=\"details\"><h2>{}</h2>", html_escape(tr("Differences")));
    let changed: Vec<&DiffItem> = diff.items.iter().filter(|i| i.has_changes()).collect();
    if changed.is_empty() {
        let _ = write!(out, "<p class=\"empty\">{}</p>", html_escape(tr("The schemas are identical.")));
    }
    for item in changed {
        item_html(&mut out, item);
    }
    out.push_str("</section>\n");

    let unchanged: Vec<&DiffItem> = diff.items.iter().filter(|i| !i.has_changes()).collect();
    if !unchanged.is_empty() {
        let _ = write!(out, "<section><h2>{} ({})</h2><ul class=\"unchanged\">", html_escape(tr("Unchanged objects")), unchanged.len());
        for i in unchanged {
            let _ = write!(out, "<li><span class=\"kind\">{}</span>{}</li>", html_escape(tr(i.kind.label())), html_escape(&i.qualified_name()));
        }
        out.push_str("</ul></section>\n");
    }
    out.push_str("</body>\n</html>\n");
    out
}

fn qualify(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{s}.{name}"),
        _ => name.to_string(),
    }
}

/// Status of every table and view, keyed by `schema.table` (or `table` when
/// the snapshot has no schema), for colouring ER diagrams ("diagram diff").
/// Objects whose target name or schema differs are listed under both names.
pub fn diagram_status(diff: &SchemaDiff) -> BTreeMap<String, DiffStatus> {
    let mut out = BTreeMap::new();
    for item in &diff.items {
        if !matches!(item.kind, ObjectKind::Table | ObjectKind::ForeignTable | ObjectKind::View | ObjectKind::MaterializedView) {
            continue;
        }
        let status = if item.status == DiffStatus::Unchanged && item.has_changes() { DiffStatus::Modified } else { item.status };
        out.insert(qualify(item.schema.as_deref(), &item.name), status);
        if let Some(t) = &item.target_name {
            out.insert(qualify(diff.target_schema.as_deref(), t), status);
        }
    }
    out
}
