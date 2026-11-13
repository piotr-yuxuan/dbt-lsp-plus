// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a YAML properties file declares: its named entries, where its text
//! writes them.  Offsets are bytes into the file's text.  Pure.

use crate::project;
use crate::text::{Encoding, LineIndex};
use crate::yaml::{self, Node};
use std::ops::Range;

/// The top-level keys whose sequences hold named entries, with the label
/// of such an entry.
const SECTIONS: [(&str, &str); 12] = [
    ("models", "model"),
    ("seeds", "seed"),
    ("snapshots", "snapshot"),
    ("analyses", "analysis"),
    ("macros", "macro"),
    ("sources", "source"),
    ("exposures", "exposure"),
    ("metrics", "metric"),
    ("semantic_models", "semantic model"),
    ("saved_queries", "saved query"),
    ("unit_tests", "unit test"),
    ("groups", "group"),
];

/// The named entries that an entry holds: the entry's label, the key of
/// their sequence, and their label.
const CHILDREN: [(&str, &str, &str); 6] = [
    ("model", "columns", "column"),
    ("seed", "columns", "column"),
    ("snapshot", "columns", "column"),
    ("analysis", "columns", "column"),
    ("source", "tables", "table"),
    ("table", "columns", "column"),
];

/// A named entry of a properties file.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// What it declares, such as `model` or `column`.
    pub label: &'static str,
    pub name: String,
    /// From its first key to its last content.
    pub range: Range<usize>,
    /// Its name as written, inside its quotes.
    pub name_range: Range<usize>,
}

/// The named entries of the properties file TEXT, in the order of the
/// text, each before those it holds; none when TEXT is not valid YAML.
pub fn entries(text: &str) -> Vec<Entry> {
    let Some(Node::Map { entries: top, .. }) = yaml::parse(text) else { return Vec::new() };
    let index = LineIndex::new(text);
    let sections = top.iter().filter_map(|(key, value)| {
        let (_, label) = SECTIONS.iter().find(|(section, _)| key.as_str() == Some(section))?;
        Some(value.items().iter().map(|item| (item, *label)))
    });
    let mut pending: Vec<(&Node, &str)> = sections.flatten().collect();
    pending.reverse();
    let mut found = Vec::new();
    while let Some((item, label)) = pending.pop() {
        let Node::Map { entries, end } = item else { continue };
        let Some((Node::Scalar { line, column, .. }, _)) = entries.first() else { continue };
        let Some((name, name_range)) = item.get("name").and_then(|name| project::written(text, &index, name)) else {
            continue;
        };
        let start = index.offset(*line as u64, *column as u64, Encoding::Utf32);
        let end = index.offset(end.0 as u64, end.1 as u64, Encoding::Utf32);
        let range = if text[end..].starts_with('}') {
            start..end + 1
        } else {
            // In block style, the parser ends a mapping at the next token:
            // the blank lines before it, and the comments no deeper than the
            // entry's first key, belong to what follows.
            let lines = text[start..end].split_inclusive('\n').scan(start, |at, line| {
                let line_start = std::mem::replace(at, *at + line.len());
                Some((line_start, line))
            });
            let content = |(_, line): &(usize, &str)| {
                let indent = line.len() - line.trim_start().len();
                !line.trim().is_empty() && !(line.trim_start().starts_with('#') && indent <= *column)
            };
            let last = lines.filter(content).last();
            start..last.map_or(end, |(line_start, line)| line_start + line.trim_end().len())
        };
        found.push(Entry { label, name, range, name_range });
        if let Some((_, key, child)) = CHILDREN.iter().find(|(parent, ..)| *parent == label) {
            let held = item.get(key).map_or(&[][..], Node::items);
            pending.extend(held.iter().rev().map(|held| (held, *child)));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROPERTIES: &str = "\
version: 2

models:
  - name: orders
    columns:
      - name: order_id
        data_tests: [unique]
      # A comment of the next column.
      - name: \"montant_€\"
  # A comment of the next model.
  - name: customers
    columns: []
    description: |
      Customers.
      # A heading of the description, its last line.

seeds:
  - name: raw_orders
    columns:
      - name: id
snapshots:
  - name: orders_snapshot
analyses:
  - name: order_counts
    columns:
      - name: n
macros:
  - name: cents_to_dollars
    arguments:
      - name: column_name
sources:
  - name: raw
    tables:
      - name: orders
        columns:
          - name: id
exposures:
  - name: weekly
metrics:
  - name: revenue
semantic_models:
  - name: orders_sm
    entities:
      - name: order
saved_queries:
  - name: q
unit_tests:
  - name: t
    model: orders
groups:
  - name: finance
";

    fn named(text: &str) -> Vec<(&'static str, String)> {
        entries(text).into_iter().map(|entry| (entry.label, entry.name)).collect()
    }

    fn entry<'a>(found: &'a [Entry], label: &str, name: &str) -> &'a Entry {
        found.iter().find(|entry| entry.label == label && entry.name == name).unwrap()
    }

    #[test]
    fn finds_each_section_s_entries_in_order_and_what_they_hold() {
        let expected = [
            ("model", "orders"),
            ("column", "order_id"),
            ("column", "montant_€"),
            ("model", "customers"),
            ("seed", "raw_orders"),
            ("column", "id"),
            ("snapshot", "orders_snapshot"),
            ("analysis", "order_counts"),
            ("column", "n"),
            ("macro", "cents_to_dollars"),
            ("source", "raw"),
            ("table", "orders"),
            ("column", "id"),
            ("exposure", "weekly"),
            ("metric", "revenue"),
            ("semantic model", "orders_sm"),
            ("saved query", "q"),
            ("unit test", "t"),
            ("group", "finance"),
        ];
        let expected: Vec<_> = expected.iter().map(|(label, name)| (*label, name.to_string())).collect();
        assert_eq!(named(PROPERTIES), expected);
    }

    #[test]
    fn an_entry_runs_from_its_first_key_to_its_last_content() {
        let found = entries(PROPERTIES);
        let text = |entry: &Entry| &PROPERTIES[entry.range.clone()];
        // A comment no deeper than the first key belongs to what follows.
        let orders = entry(&found, "model", "orders");
        assert!(text(orders).starts_with("name: orders\n"));
        assert!(text(orders).ends_with("- name: \"montant_€\""));
        assert!(text(entry(&found, "column", "order_id")).ends_with("data_tests: [unique]"));
        // A deeper line of a block scalar is content, even when it starts with `#`.
        assert!(text(entry(&found, "model", "customers")).ends_with("# A heading of the description, its last line."));
        // At the end of a section, before the next key.
        assert_eq!(text(entry(&found, "column", "n")), "name: n");
        assert_eq!(text(entry(&found, "group", "finance")), "name: finance");
    }

    #[test]
    fn a_name_lies_in_its_entry_and_an_entry_in_its_parent() {
        let found = entries(PROPERTIES);
        for entry in &found {
            assert!(entry.range.start <= entry.name_range.start && entry.name_range.end <= entry.range.end);
            assert_eq!(&PROPERTIES[entry.name_range.clone()], entry.name);
        }
        // Each child lies inside its parent.
        let table = entry(&found, "table", "orders");
        let source = entry(&found, "source", "raw");
        let column = found.iter().rfind(|entry| entry.label == "column" && entry.name == "id").unwrap();
        assert!(source.range.start <= table.range.start && table.range.end <= source.range.end);
        assert!(table.range.start <= column.range.start && column.range.end <= table.range.end);
    }

    #[test]
    fn a_flow_entry_ends_at_its_brace() {
        let text = "{\"models\": [{\"name\": \"orders\", \"columns\": [{\"name\": \"id\"}]}]}";
        let found = entries(text);
        assert_eq!(&text[found[0].range.clone()], "\"name\": \"orders\", \"columns\": [{\"name\": \"id\"}]}");
        assert_eq!(&text[found[0].name_range.clone()], "orders");
        assert_eq!(&text[found[1].range.clone()], "\"name\": \"id\"}");
    }

    #[test]
    fn a_name_that_is_not_the_first_key_and_crlf_line_ends() {
        let text = "models:\r\n  - description: First.\r\n    name: late\r\n  # Next.\r\n  - name: next\r\n";
        let found = entries(text);
        assert_eq!(&text[found[0].range.clone()], "description: First.\r\n    name: late");
        assert_eq!(&text[found[0].name_range.clone()], "late");
        assert_eq!(&text[found[1].range.clone()], "name: next");
    }

    #[test]
    fn an_entry_without_a_scalar_name_is_skipped_with_what_it_holds() {
        let text = "models:\n  - name: [a, b]\n    columns:\n      - name: x\n  - description: none\n  - kept\n";
        assert_eq!(named(text), Vec::new());
    }

    #[test]
    fn other_yaml_and_invalid_yaml_declare_nothing() {
        let project = "name: shop\nmodels:\n  shop:\n    +materialized: view\n";
        let packages = "packages:\n  - package: dbt-labs/dbt_utils\n    version: 1.3.0\n";
        for text in [project, packages, "models: [unclosed", "", "- name: a\n"] {
            assert_eq!(named(text), Vec::new(), "{text}");
        }
    }
}
