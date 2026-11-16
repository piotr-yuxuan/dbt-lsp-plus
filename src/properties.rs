// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What a YAML properties file declares: its named entries, where its text
//! writes them.  Offsets are bytes into the file's text.  Pure.

use crate::project;
use crate::text::{Encoding, LineIndex};
use crate::yaml::{self, Node};
use std::ops::Range;

/// Where named entries are: the label of what holds them (none for the
/// top of the file), the key of their sequence, and their label.
const HELD: [(&str, &str, &str); 22] = [
    ("", "models", "model"),
    ("", "seeds", "seed"),
    ("", "snapshots", "snapshot"),
    ("", "analyses", "analysis"),
    ("", "macros", "macro"),
    ("", "functions", "function"),
    ("", "data_tests", "data test"),
    ("", "tests", "data test"),
    ("", "sources", "source"),
    ("", "exposures", "exposure"),
    ("", "metrics", "metric"),
    ("", "semantic_models", "semantic model"),
    ("", "saved_queries", "saved query"),
    ("", "unit_tests", "unit test"),
    ("", "groups", "group"),
    ("model", "columns", "column"),
    ("model", "metrics", "metric"),
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

/// A generic test that a properties file names: the package its name
/// gives, if any, its name, and where the whole name is written.
#[derive(Clone, Debug, PartialEq)]
pub struct Test {
    pub package: Option<String>,
    pub name: String,
    pub range: Range<usize>,
}

/// The entries that NODE, of LABEL, holds, in the order of the text, each
/// with its label; an alias holds none, its anchor being elsewhere.
fn held<'a>(node: &'a Node, label: &str) -> Vec<(&'a Node, &'static str)> {
    let Node::Map { entries, .. } = node else { return Vec::new() };
    let lists = entries.iter().filter_map(|(key, value)| {
        // A model's version has no name: its columns count as the model's.
        if label == "model" && key.as_str() == Some("versions") {
            let columns = value.items().iter().filter_map(|version| version.get("columns")).flat_map(Node::items);
            return Some(columns.map(|item| (item, "column")).collect::<Vec<_>>());
        }
        let (.., child) = HELD.iter().find(|(parent, held, _)| *parent == label && key.as_str() == Some(held))?;
        Some(value.items().iter().map(|item| (item, *child)).collect())
    });
    lists.flatten().collect()
}

/// The named entries of the properties file TEXT, in the order of the
/// text, each before those it holds; none when TEXT is not valid YAML.
pub fn entries(text: &str) -> Vec<Entry> {
    yaml::parse(text).map_or_else(Vec::new, |root| entries_in(text, &root))
}

/// The named entries of the properties file TEXT, whose tree is ROOT, as
/// `entries` gives them.
pub fn entries_in(text: &str, root: &Node) -> Vec<Entry> {
    let index = LineIndex::new(text);
    // Popped from the end: what comes first in the text pops first.
    let mut pending = held(root, "");
    pending.reverse();
    let mut found = Vec::new();
    while let Some((item, label)) = pending.pop() {
        let Node::Map { entries, end } = item else { continue };
        // The first key that has a place: an alias or a complex key has none.
        let first = entries.iter().find_map(|(key, _)| match key {
            Node::Scalar { line, column, .. } => Some((*line, *column)),
            _ => None,
        });
        // A `name:` still being typed is empty, and placed at the next token;
        // a block scalar's name holds a line break, as written or as read.
        let named = item.get("name").and_then(|name| project::written(text, &index, name));
        let usable = |(name, range): &(String, Range<usize>)| {
            !name.trim().is_empty() && !name.contains(['\n', '\r']) && !text[range.clone()].contains(['\n', '\r'])
        };
        let (Some((line, column)), Some((name, name_range))) = (first, named.filter(usable)) else {
            continue;
        };
        let start = index.offset(line as u64, column as u64, Encoding::Utf32);
        let end = index.offset(end.0 as u64, end.1 as u64, Encoding::Utf32);
        let range = if text[end..].starts_with('}') {
            start..end + 1
        } else {
            // In block style, the parser ends a mapping at the next token:
            // the blank lines before it, and the comments less deep than the
            // entry's first key, belong to what follows; a key commented out
            // stays in its entry.
            let lines = text[start..end].split_inclusive(['\n', '\r']).scan(start, |at, line| {
                let line_start = std::mem::replace(at, *at + line.len());
                Some((line_start, line))
            });
            let content = |(_, line): &(usize, &str)| {
                let indent = line.len() - line.trim_start().len();
                !line.trim().is_empty() && !(line.trim_start().starts_with('#') && indent < column)
            };
            let last = lines.filter(content).last();
            start..last.map_or(end, |(line_start, line)| line_start + line.trim_end().len())
        };
        // LSP wants the selection inside the range; a name placed elsewhere
        // by the parser makes no symbol.
        if name_range.start < range.start || range.end < name_range.end {
            continue;
        }
        found.push(Entry { label, name, range, name_range });
        pending.extend(held(item, label).into_iter().rev());
    }
    found
}

/// The generic tests that the properties file TEXT names in its
/// `data_tests:` and `tests:` lists below the top level, at any depth, in
/// the order of the text: an item (`- unique`), the one key of an item
/// (`- relationships: {...}`), or an item's `test_name:`.  The top level's
/// lists describe singular tests; an alias names none.
pub fn tests(text: &str) -> Vec<Test> {
    let Some(Node::Map { entries: top, .. }) = yaml::parse(text) else { return Vec::new() };
    let index = LineIndex::new(text);
    let mut pending: Vec<&Node> = top.iter().map(|(_, value)| value).collect();
    let mut found = Vec::new();
    while let Some(node) = pending.pop() {
        match node {
            Node::Seq(items) => pending.extend(items),
            Node::Map { entries, .. } => {
                for (key, value) in entries {
                    if matches!(key.as_str(), Some("data_tests" | "tests")) {
                        let named = value.items().iter().filter_map(|item| match item {
                            Node::Map { entries, .. } if item.get("test_name").is_none() => match &entries[..] {
                                [(key, _)] => Some(key),
                                _ => None,
                            },
                            Node::Map { .. } => item.get("test_name"),
                            _ => Some(item),
                        });
                        let written = named.filter_map(|name| project::written(text, &index, name));
                        found.extend(written.map(|(written, range)| {
                            let (package, name) = match written.split_once('.') {
                                Some((package, name)) => (Some(package.to_owned()), name.to_owned()),
                                None => (None, written),
                            };
                            Test { package, name, range }
                        }));
                    }
                    pending.push(value);
                }
            }
            _ => {}
        }
    }
    found.sort_by_key(|test| test.range.start);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROPERTIES: &str = "\
version: 2

models:
  - name: orders
    metrics:
      - name: order_count
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
    columns:
      - name: dbt_valid_from
analyses:
  - name: order_counts
    columns:
      - name: n
macros:
  - name: cents_to_dollars
    arguments:
      - name: column_name
functions:
  - name: is_positive_int
data_tests:
  - name: assert_positive
tests:
  - name: old_style
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
            ("metric", "order_count"),
            ("column", "order_id"),
            ("column", "montant_€"),
            ("model", "customers"),
            ("seed", "raw_orders"),
            ("column", "id"),
            ("snapshot", "orders_snapshot"),
            ("column", "dbt_valid_from"),
            ("analysis", "order_counts"),
            ("column", "n"),
            ("macro", "cents_to_dollars"),
            ("function", "is_positive_int"),
            ("data test", "assert_positive"),
            ("data test", "old_style"),
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
    fn a_key_commented_out_stays_in_its_entry() {
        let text = "models:\n  - name: orders\n    columns:\n      - name: id\n    # tests:\n    #   - unique\n  \
                    # The next model.\n  - name: customers\n";
        let found = entries(text);
        assert!(text[found[0].range.clone()].ends_with("    #   - unique"));
        assert_eq!(&text[found[1].range.clone()], "name: id");
    }

    #[test]
    fn a_lone_carriage_return_ends_a_line_too() {
        let text = "models:\r  - name: a\r    description: x\r  # Next.\r\r  - name: b\r";
        let found = entries(text);
        assert_eq!(&text[found[0].range.clone()], "name: a\r    description: x");
        assert_eq!(&text[found[1].range.clone()], "name: b");
    }

    #[test]
    fn a_name_being_typed_or_out_of_its_entry_makes_no_entry() {
        let model = ("model", "kept".to_owned());
        let source = ("source", "kept".to_owned());
        let texts = [
            // Before a sibling, before a blank and a comment line then the
            // next section, at the end of the text: each placed by the parser
            // at the next token.
            ("models:\n  - name:\n    columns:\n      - name:\n  - name: kept\n", vec![model.clone()]),
            (
                "models:\n  - name: kept\n  - name:\n\n# Sources.\nsources:\n  - name: kept\n",
                vec![model.clone(), source],
            ),
            ("models:\n  - name: kept\n  - name:", vec![model.clone()]),
            (
                "models:\n  - name: \"\"\n  - name: '  '\n  - name: |\n      orders\n  - name: kept\n",
                vec![model.clone()],
            ),
            // A block scalar's name with another key after it, in each line end
            // and chomping that keeps a line break.
            ("models:\n  - name: |\n      orders\n    description: x\n  - name: kept\n", vec![model.clone()]),
            ("models:\r\n  - name: |\r\n      orders\r\n  - name: kept\r\n", vec![model.clone()]),
            ("models:\n  - name: |-\n      a\n      b\n    description: x\n  - name: kept\n", vec![model.clone()]),
        ];
        for (text, expected) in texts {
            assert_eq!(named(text), expected, "{text}");
            for entry in entries(text) {
                assert_eq!(&text[entry.range.clone()], "name: kept", "{text}");
            }
        }
    }

    #[test]
    fn a_block_scalar_name_on_one_line_is_a_name() {
        let text = "models:\n  - name: |-\n      orders\n    description: x\n";
        let found = entries(text);
        assert_eq!(named(text), [("model", "orders".to_owned())]);
        assert_eq!(&text[found[0].name_range.clone()], "orders");
    }

    #[test]
    fn an_entry_starts_at_its_first_placed_key_and_an_alias_holds_nothing() {
        let text = "anchors:\n  - &cols\n    - name: id\n  - &base\n    description: shared\nmodels:\n  \
                    - <<: *base\n    name: x\n    columns: *cols\n  - *base\n  - ? [complex]\n    : value\n    \
                    name: y\n";
        let found = entries(text);
        assert_eq!(named(text), [("model", "x".to_owned()), ("model", "y".to_owned())]);
        assert_eq!(&text[found[0].range.clone()], "<<: *base\n    name: x\n    columns: *cols");
        assert_eq!(&text[found[1].range.clone()], "name: y");
    }

    #[test]
    fn finds_the_generic_tests_named_below_the_top_level() {
        let text = "\
version: 2
data_tests:
  - name: singular_one
anchors:
  - &shared [hidden]
models:
  - name: orders
    data_tests:
      - shop_utils.not_negative:
          column_name: amount
    columns:
      - name: id
        data_tests:
          - unique
          - name: custom_name
            test_name: not_null
          - relationships:
              to: ref('customers')
              field: id
    versions:
      - v: 2
        columns:
          - name: id
            tests: [is_even]
seeds:
  - name: s
    tests:
      - \"accepted_values\":
          values: [a]
sources:
  - name: raw
    data_tests: [source_level]
    tables:
      - name: t
        tests:
          - table_level
        columns:
          - name: c
            data_tests: *shared
";
        let found = tests(text);
        let names: Vec<(Option<&str>, &str)> =
            found.iter().map(|test| (test.package.as_deref(), test.name.as_str())).collect();
        let expected = [
            (Some("shop_utils"), "not_negative"),
            (None, "unique"),
            (None, "not_null"),
            (None, "relationships"),
            (None, "is_even"),
            (None, "accepted_values"),
            (None, "source_level"),
            (None, "table_level"),
        ];
        assert_eq!(names, expected);
        let written: Vec<&str> = found.iter().map(|test| &text[test.range.clone()]).collect();
        assert_eq!(written[0], "shop_utils.not_negative");
        assert_eq!(written[2], "not_null");
        assert_eq!(written[5], "accepted_values");
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
