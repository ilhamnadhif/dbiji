//! Small helpers: deterministic topological sort and escaping.

use std::collections::{BTreeMap, BTreeSet};

/// Kahn topological sort. `deps[n]` lists the nodes `n` depends on (which
/// must come first). Only edges between members of `nodes` are considered.
/// Returns the sorted nodes and, separately, the nodes left in cycles (in
/// name order).
pub(crate) fn topo_sort(nodes: &[String], deps: &BTreeMap<String, BTreeSet<String>>) -> (Vec<String>, Vec<String>) {
    let members: BTreeSet<&String> = nodes.iter().collect();
    let mut remaining: BTreeMap<&String, BTreeSet<&String>> = BTreeMap::new();
    for n in nodes {
        let d: BTreeSet<&String> = deps
            .get(n)
            .map(|s| s.iter().filter(|x| *x != n && members.contains(x)).collect())
            .unwrap_or_default();
        remaining.insert(n, d);
    }
    let mut out = Vec::with_capacity(nodes.len());
    loop {
        let ready: Vec<&String> = remaining.iter().filter(|(_, d)| d.is_empty()).map(|(n, _)| *n).collect();
        if ready.is_empty() {
            break;
        }
        // take the first ready node (name order) to keep the result stable
        let next = ready[0];
        remaining.remove(next);
        for d in remaining.values_mut() {
            d.remove(next);
        }
        out.push(next.clone());
    }
    let cyclic = remaining.keys().map(|k| (*k).clone()).collect();
    (out, cyclic)
}

/// Escape text for HTML element content and attribute values.
pub(crate) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Characters allowed in XML 1.0 documents.
fn xml_char_allowed(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

/// Escape text for XML content or attribute values; characters not allowed
/// in XML 1.0 are replaced by U+FFFD.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#13;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#9;"),
            c if xml_char_allowed(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

/// Text as a CDATA section (splitting `]]>`), invalid XML characters replaced.
pub(crate) fn xml_cdata(s: &str) -> String {
    let clean: String = s.chars().map(|c| if xml_char_allowed(c) { c } else { '\u{FFFD}' }).collect();
    format!("<![CDATA[{}]]>", clean.replace("]]>", "]]]]><![CDATA[>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topo_sort_orders_and_reports_cycles() {
        let nodes: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        let mut deps = BTreeMap::new();
        deps.insert("a".to_string(), BTreeSet::from(["b".to_string()]));
        deps.insert("b".to_string(), BTreeSet::from(["c".to_string(), "zz".to_string()]));
        deps.insert("d".to_string(), BTreeSet::from(["e".to_string()]));
        deps.insert("e".to_string(), BTreeSet::from(["d".to_string()]));
        let (order, cyclic) = topo_sort(&nodes, &deps);
        assert_eq!(order, vec!["c", "b", "a"]);
        assert_eq!(cyclic, vec!["d", "e"]);
    }

    #[test]
    fn escaping() {
        assert_eq!(html_escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(xml_escape("a\u{1}b<"), "a\u{FFFD}b&lt;");
        assert_eq!(xml_cdata("x]]>y"), "<![CDATA[x]]]]><![CDATA[>y]]>");
    }
}
