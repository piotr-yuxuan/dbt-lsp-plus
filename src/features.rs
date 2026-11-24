// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The answers of the features, as LSP JSON.  Pure: the texts, the project
//! and the graph are inputs.

use crate::graph::Graph;
use crate::navigation::Lookup;
use crate::node::{self, TYPES, Type};
use crate::project::{Entity, Project};
use crate::properties;
use crate::reference;
use crate::scan::{self, Kind, Symbol};
use crate::text::LineStarts;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use url::Url;

pub const MAX_WORKSPACE_SYMBOLS: usize = 20_000;

/// LSP's `SymbolKind` and a label of each kind of block or CTE of the
/// outline: a block that defines a node has its resource type's kind, a
/// generic test's or a materialization's that of a macro, as dbt holds
/// them.
const OUTLINE_KINDS: [(Kind, u32, &str); 10] = [
    (Kind::Macro, node::MACRO.symbol, "macro"),
    (Kind::Test, node::MACRO.symbol, "test"),
    (Kind::Materialization, node::MACRO.symbol, "materialization"),
    (Kind::Snapshot, node::SNAPSHOT.symbol, "snapshot"),
    (Kind::Docs, node::DOC.symbol, "docs"),
    (Kind::Set, 13, "set"),  // Variable
    (Kind::Cte, 23, "CTE"),  // Struct
    (Kind::If, 3, "if"),     // Namespace, as agreed with dbt-mode
    (Kind::For, 3, "for"),   // Namespace
    (Kind::Call, 3, "call"), // Namespace
];

/// LSP's `SymbolKind` of an entry of a properties file that declares no
/// node, by its label.  An entry that declares one has its resource
/// type's kind, found by its label in `node::TYPES`.
const ENTRY_KINDS: [(&str, u32); 2] = [
    ("source", 2), // Module: a source, the group of its tables, which `table` entries declare
    ("column", 8), // Field
];

/// A symbol of an outline: a block or CTE, or an entry of a properties file.
struct Outlined {
    name: String,
    kind: u32,
    detail: &'static str,
    range: Range<usize>,
    name_range: Range<usize>,
}

pub fn uri(path: &Path) -> String {
    Url::from_file_path(path).map(String::from).unwrap_or_default()
}

/// Closes the innermost open symbol of STACK into its parent, or into ROOTS.
fn close(stack: &mut Vec<(Outlined, Vec<Value>)>, roots: &mut Vec<Value>, starts: &LineStarts) {
    let Some((symbol, children)) = stack.pop() else { return };
    let value = json!({
        "name": symbol.name,
        "detail": symbol.detail,
        "kind": symbol.kind,
        "range": starts.range(symbol.range.start, symbol.range.end),
        "selectionRange": starts.range(symbol.name_range.start, symbol.name_range.end),
        "children": children,
    });
    match stack.last_mut() {
        Some((_, siblings)) => siblings.push(value),
        None => roots.push(value),
    }
}

/// The outline of TEXT: the entries of a properties file for YAML, else
/// its blocks and CTEs; `DocumentSymbol`s nested by extent, or, for a
/// client without hierarchical support, `SymbolInformation`s at URI.
pub fn document_symbols(text: &str, hierarchical: bool, uri: &str) -> Value {
    let starts = LineStarts::new(text);
    let contains = |outer: &Outlined, inner: &Outlined| {
        outer.range.start <= inner.range.start && inner.range.end <= outer.range.end
    };
    let symbols: Vec<Outlined> = if uri.ends_with(".yml") || uri.ends_with(".yaml") {
        let outlined = |entry: properties::Entry| Outlined {
            kind: TYPES
                .iter()
                .find(|row| row.label == Some(entry.label))
                .map(|row| row.symbol)
                .or_else(|| ENTRY_KINDS.iter().find(|(label, _)| *label == entry.label).map(|(_, kind)| *kind))
                .unwrap_or(node::OTHER.symbol),
            detail: entry.label,
            name: entry.name,
            range: entry.range,
            name_range: entry.name_range,
        };
        properties::entries(text).into_iter().map(outlined).collect()
    } else {
        let outlined = |symbol: Symbol| {
            let (_, kind, detail) = OUTLINE_KINDS.into_iter().find(|(kind, ..)| *kind == symbol.kind)?;
            Some(Outlined { name: symbol.name, kind, detail, range: symbol.range, name_range: symbol.name_range })
        };
        scan::outline(text, !uri.ends_with(".md"), true).into_iter().filter_map(outlined).collect()
    };
    let mut stack: Vec<(Outlined, Vec<Value>)> = Vec::new();
    let mut roots = Vec::new();
    let mut flat = Vec::new();
    for symbol in symbols {
        while stack.last().is_some_and(|(open, _)| !contains(open, &symbol)) {
            close(&mut stack, &mut roots, &starts);
        }
        let container = stack.last().map(|(open, _)| open.name.clone());
        let location = json!({"uri": uri, "range": starts.range(symbol.range.start, symbol.range.end)});
        let mut information = json!({"name": symbol.name, "kind": symbol.kind, "location": location});
        if let Some(container) = container {
            information["containerName"] = json!(container);
        }
        flat.push(information);
        stack.push((symbol, Vec::new()));
    }
    while !stack.is_empty() {
        close(&mut stack, &mut roots, &starts);
    }
    Value::Array(if hierarchical { roots } else { flat })
}

/// Every resource of PROJECT whose name holds the characters of QUERY in
/// order, ignoring case, as LSP 3.17 `WorkspaceSymbol`s that are also valid
/// `SymbolInformation`s: dbt's unique id as container, which begins with
/// the resource type and the package and which clients show, and the id
/// and resource type as data.
pub fn workspace_symbols(project: &Project, query: &str) -> Value {
    let query = query.to_lowercase();
    let matches = |name: &str| {
        let mut chars = name.chars().flat_map(char::to_lowercase);
        query.chars().all(|q| chars.any(|c| c == q))
    };
    let mut by_file: HashMap<&Path, LineStarts> = HashMap::new();
    let mut symbols = Vec::new();
    for entity in project.entities.iter().filter(|entity| matches(&entity.shown())).take(MAX_WORKSPACE_SYMBOLS) {
        let text = project.text(&entity.path).unwrap_or_default();
        let starts = by_file.entry(&entity.path).or_insert_with(|| LineStarts::new(text));
        symbols.push(json!({
            "name": entity.shown(),
            "kind": Type::named(entity.resource_type).symbol,
            "location": {"uri": uri(&entity.path), "range": starts.range(entity.name_range.start, entity.name_range.end)},
            "containerName": entity.id,
            "data": {"uniqueId": entity.id, "resourceType": entity.resource_type},
        }));
    }
    Value::Array(symbols)
}

/// The `CallHierarchyItem` of ITEM.
fn hierarchy_item(item: &Entity, project: &Project) -> Value {
    let starts = LineStarts::new(project.text(&item.path).unwrap_or_default());
    json!({
        "name": item.shown(),
        "kind": Type::named(item.resource_type).symbol,
        "detail": item.id,
        "uri": uri(&item.path),
        "range": starts.range(item.range.start, item.range.end),
        "selectionRange": starts.range(item.name_range.start, item.name_range.end),
        "data": {"uniqueId": item.id, "resourceType": item.resource_type},
    })
}

/// The item at OFFSET of the file at PATH: the resource that a call there
/// names, else the innermost resource defined there; none where the
/// position names neither.
pub fn prepare_call_hierarchy(project: &Project, graph: &Graph, path: &Path, offset: usize) -> Option<Value> {
    let in_file = |id: &str| graph.items.get(id).is_some_and(|item| item.path == path);
    let at = |site: &Range<usize>| site.start <= offset && offset <= site.end;
    let called = graph.edges.iter().find(|((from, _), sites)| in_file(from) && sites.iter().any(at));
    let defined = || {
        let around = graph
            .items
            .values()
            .filter(|item| item.path == path && item.range.start <= offset && offset <= item.range.end);
        around.min_by_key(|item| (item.range.len(), item.id.clone()))
    };
    let item = called.map(|((_, to), _)| &graph.items[to]).or_else(defined)?;
    Some(json!([hierarchy_item(item, project)]))
}

/// The calls of an item: those that call ID when INCOMING, else those ID makes.
pub fn calls(project: &Project, graph: &Graph, id: &str, incoming: bool) -> Value {
    let edges = graph.edges.iter().filter(|((from, to), _)| if incoming { to == id } else { from == id });
    let answer = edges.map(|((from, to), sites)| {
        let (other, key) = if incoming { (from, "from") } else { (to, "to") };
        let starts = LineStarts::new(project.text(&graph.items[from].path).unwrap_or_default());
        let ranges: Vec<Value> = sites.iter().map(|site| starts.range(site.start, site.end)).collect();
        json!({key: hierarchy_item(&graph.items[other], project), "fromRanges": ranges})
    });
    Value::Array(answer.collect())
}

/// A hover's result of TEXT: markdown when MARKDOWN, its line breaks hard
/// when HARD, as markdown needs two spaces before each; else plain text, as
/// given.
pub fn hover(text: &str, hard: bool, markdown: bool) -> Value {
    let value = if markdown && hard { text.replace('\n', "  \n") } else { text.to_owned() };
    json!({"contents": {"kind": if markdown { "markdown" } else { "plaintext" }, "value": value}})
}

/// The relation hints of TEXT, the file at PATH, whose calls are CALLS:
/// after each `ref` or `source`, right after its closing parenthesis, the
/// relation dbt builds for the node its definition leads to
/// (`Lookup::node`) by LOOKUP, or `ephemeral`; none for a disabled node,
/// or a `ref` that a keyword argument follows, whose closing parenthesis is
/// not right after its name.  Only those within RANGE, a range of byte
/// offsets.
pub fn inlay_hints(
    text: &str,
    calls: Vec<scan::Call>,
    lookup: &Lookup,
    path: &Path,
    range: Range<usize>,
) -> Vec<Value> {
    let lines = LineStarts::new(text);
    let hint = |call: scan::Call| {
        // A source's match ends after its parenthesis, a ref's before it.
        let after = match call.name.kind {
            reference::Kind::Table { .. } => call.range.end,
            reference::Kind::Ref => {
                let rest = &text[call.range.end..];
                let skipped = rest.len() - rest.trim_start().len();
                rest[skipped..].starts_with(')').then_some(call.range.end + skipped + 1)?
            }
            _ => return None,
        };
        // The range first: a node's lookup costs more.
        if !(range.start..=range.end).contains(&after) {
            return None;
        }
        let node = lookup.node(path, &call.name).filter(|node| node["disabled"] != true)?;
        let label = match node["config"]["materialized"].as_str() {
            Some("ephemeral") => "ephemeral",
            _ => node["relation"].as_str()?,
        };
        Some(json!({"position": lines.position(after), "label": label, "kind": 1,
                    "paddingLeft": true, "tooltip": node["unique_id"]}))
    };
    calls.into_iter().filter_map(hint).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ParseIndex;
    use crate::project;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop")
    }

    const SNAPSHOT: &str =
        "{% snapshot s %}\nwith a as (select 1),\nb as (select 2)\nselect * from b\n{% endsnapshot %}\n";

    #[test]
    fn nests_the_outline_by_extent() {
        let symbols = document_symbols(SNAPSHOT, true, "file:///s.sql");
        assert_eq!(symbols[0]["name"], "s");
        assert_eq!(
            symbols[0]["range"],
            json!({"start": {"line": 0, "character": 0}, "end": {"line": 4, "character": 17}})
        );
        let children: Vec<&Value> = symbols[0]["children"].as_array().unwrap().iter().map(|c| &c["name"]).collect();
        assert_eq!(children, ["a", "b"]);
        assert_eq!(
            symbols[0]["children"][1]["selectionRange"],
            json!({"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}})
        );
    }

    #[test]
    fn nests_the_control_blocks_with_ctes_and_definitions() {
        let text = "{% macro m() %}\n{%- if is_incremental() -%}\nwith a as (select 1)\nselect * from a\n\
                    {%- else %}\nselect 2\n{%- endif %}\n{% for c in cs %}{{ c }}{% endfor %}\n\
                    {% call statement('main') %}select 1{% endcall %}\n{% endmacro %}\n";
        let symbols = document_symbols(text, true, "file:///m.sql");
        let macro_ = &symbols[0];
        assert_eq!((&macro_["name"], &macro_["children"][0]["name"]), (&json!("m"), &json!("if is_incremental()")));
        let branch = &macro_["children"][0];
        assert_eq!((&branch["kind"], &branch["detail"]), (&json!(3), &json!("if")));
        assert_eq!(branch["children"][0]["name"], "a", "the CTE inside the if");
        assert_eq!(
            branch["selectionRange"],
            json!({"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 27}})
        );
        let others: Vec<(&Value, &Value, &Value)> = macro_["children"].as_array().unwrap()[1..]
            .iter()
            .map(|child| (&child["name"], &child["kind"], &child["detail"]))
            .collect();
        let expected = [
            (json!("for c in cs"), json!(3), json!("for")),
            (json!("call statement('main')"), json!(3), json!("call")),
        ];
        assert_eq!(others, expected.iter().map(|(n, k, d)| (n, k, d)).collect::<Vec<_>>(), "no else");
    }

    #[test]
    fn a_flat_outline_names_the_container() {
        let symbols = document_symbols(SNAPSHOT, false, "file:///s.sql");
        let flat: Vec<(&Value, Option<&Value>)> =
            symbols.as_array().unwrap().iter().map(|s| (&s["name"], s.get("containerName"))).collect();
        assert_eq!(flat, [(&json!("s"), None), (&json!("a"), Some(&json!("s"))), (&json!("b"), Some(&json!("s")))]);
        assert_eq!(symbols[1]["location"]["uri"], "file:///s.sql");
    }

    const PROPERTIES: &str = "models:\n  - name: orders\n    description: 'with x as (select 1) {% macro hidden() %}'\n    \
        columns:\n      - name: id\n    metrics:\n      - name: n\nseeds:\n  - name: s\nsnapshots:\n  - name: sn\n\
        analyses:\n  - name: a\nmacros:\n  - name: m\nfunctions:\n  - name: f\ndata_tests:\n  - name: dt\nsources:\n  - name: raw\n    tables:\n      - name: t\n        columns:\n          - name: c\nexposures:\n  - name: e\n\
        metrics:\n  - name: me\nsemantic_models:\n  - name: sm\nsaved_queries:\n  - name: q\nunit_tests:\n  \
        - name: u\ngroups:\n  - name: g\n";

    #[test]
    fn outlines_a_properties_file_with_the_agreed_kinds_and_no_sql() {
        let symbols = document_symbols(PROPERTIES, false, "file:///p.yml");
        let flat: Vec<(&str, u64, Option<&str>)> = symbols
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap(), s["kind"].as_u64().unwrap(), s["containerName"].as_str()))
            .collect();
        let expected = [
            ("orders", 5, None),
            ("id", 8, Some("orders")),
            ("n", 16, Some("orders")),
            ("s", 23, None),
            ("sn", 5, None),
            ("a", 5, None),
            ("m", 12, None),
            ("f", 12, None),
            ("dt", 24, None),
            ("raw", 2, None),
            ("t", 11, Some("raw")),
            ("c", 8, Some("t")),
            ("e", 19, None),
            ("me", 16, None),
            ("sm", 5, None),
            ("q", 6, None),
            ("u", 24, None),
            ("g", 3, None),
        ];
        assert_eq!(flat, expected);
    }

    #[test]
    fn nests_the_entries_of_a_properties_file_with_their_labels() {
        let symbols = document_symbols(PROPERTIES, true, "file:///p.yaml");
        let top: Vec<(&str, &str)> = symbols
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap(), s["detail"].as_str().unwrap()))
            .collect();
        let expected = [
            ("orders", "model"),
            ("s", "seed"),
            ("sn", "snapshot"),
            ("a", "analysis"),
            ("m", "macro"),
            ("f", "function"),
            ("dt", "data test"),
            ("raw", "source"),
            ("e", "exposure"),
            ("me", "metric"),
            ("sm", "semantic model"),
            ("q", "saved query"),
            ("u", "unit test"),
            ("g", "group"),
        ];
        assert_eq!(top, expected);
        let column = &symbols[0]["children"][0];
        assert_eq!((&column["name"], &column["detail"], &column["kind"]), (&json!("id"), &json!("column"), &json!(8)));
        assert_eq!(symbols[7]["children"][0]["detail"], "table");
        assert_eq!(
            symbols[0]["range"],
            json!({"start": {"line": 1, "character": 4}, "end": {"line": 6, "character": 15}})
        );
        assert_eq!(
            symbols[0]["selectionRange"],
            json!({"start": {"line": 1, "character": 10}, "end": {"line": 1, "character": 16}})
        );
    }

    #[test]
    fn yaml_that_does_not_parse_is_never_read_as_sql() {
        let text =
            "models:\n  - name: o\n    description: with x as (select 1) {% macro m() %}{% endmacro %}\n  - name: [";
        for hierarchical in [true, false] {
            assert_eq!(document_symbols(text, hierarchical, "file:///p.yml"), json!([]));
        }
        assert_ne!(document_symbols(text, true, "file:///p.sql"), json!([]));
    }

    #[test]
    fn workspace_symbols_match_a_subsequence_and_name_the_type() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        let symbols = workspace_symbols(&project, "EVPGVW");
        assert_eq!(symbols.as_array().unwrap().len(), 1);
        assert_eq!(symbols[0]["name"], "events.page_views");
        assert_eq!(symbols[0]["containerName"], "source.shop.events.page_views");
        assert_eq!(symbols[0]["data"], json!({"uniqueId": "source.shop.events.page_views", "resourceType": "source"}));
        assert_eq!(symbols[0]["location"]["range"]["start"], json!({"line": 13, "character": 15}));
        let all = workspace_symbols(&project, "");
        assert_eq!(all.as_array().unwrap().len(), project.entities.len());
    }

    #[test]
    fn symbols_and_the_hierarchy_show_what_dbt_added() {
        let files = [
            ("dbt_project.yml", "name: r\n"),
            ("models/_v.yml", "models:\n  - name: m\n    versions:\n      - v: 1\n      - v: 2\n"),
            ("models/m_v1.sql", "select 1"),
            ("models/m_v2.sql", "select {{ function('f') }}(1)"),
            ("models/py.py", "def model(dbt, session):\n    return dbt.ref('m')\n"),
            ("functions/f.sql", "x * 2"),
            ("snapshots/_s.yml", "snapshots:\n  - name: snap\n    relation: ref('m')\n"),
        ];
        let root = crate::project::scratch_project("features-what-dbt-added", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let symbols = |query: &str| -> Vec<(String, u64, String)> {
            let found = workspace_symbols(&project, query);
            let mut found: Vec<(String, u64, String)> = found
                .as_array()
                .unwrap()
                .iter()
                .map(|s| {
                    (
                        s["name"].as_str().unwrap().to_owned(),
                        s["kind"].as_u64().unwrap(),
                        s["data"]["uniqueId"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            found.sort();
            found
        };
        let symbol = |name: &str, kind: u64, id: &str| (name.to_owned(), kind, id.to_owned());
        // A version as dbt's selectors write it, which a query matches.
        assert_eq!(symbols("m.v2"), [symbol("m.v2", 5, "model.r.m.v2")]);
        assert_eq!(symbols("m"), [symbol("m.v1", 5, "model.r.m.v1"), symbol("m.v2", 5, "model.r.m.v2")]);
        assert_eq!(symbols("f"), [symbol("f", 12, "function.r.f")]);
        assert_eq!(symbols("py"), [symbol("py", 5, "model.r.py")]);
        let snap = workspace_symbols(&project, "snap");
        assert_eq!(snap[0]["location"]["range"]["start"], json!({"line": 1, "character": 10}), "at its name");
        let graph = Graph::scanned(&project, None);
        assert_eq!(graph.items["model.r.m.v1"].shown(), "m.v1");
        // At column 0 of a YAML snapshot's `name:` line, where dbt-mode's lineage asks.
        let yml = root.join("snapshots/_s.yml");
        let line_start = project.text(&yml).unwrap().find("  - name: snap").unwrap();
        let prepared = prepare_call_hierarchy(&project, &graph, &yml, line_start).unwrap();
        assert_eq!(prepared[0]["data"]["uniqueId"], "snapshot.r.snap");
        let incoming = calls(&project, &graph, "function.r.f", true);
        assert_eq!(incoming[0]["from"]["data"]["uniqueId"], "model.r.m.v2");
        assert_eq!(incoming.as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn each_scanned_type_has_its_kind_in_workspace_symbols() {
        let files = [
            ("dbt_project.yml", "name: r\n"),
            ("models/m.sql", "select 1"),
            ("models/_sources.yml", "sources:\n  - name: raw\n    tables:\n      - name: t\n"),
            ("models/d.md", "{% docs d %}text{% enddocs %}\n"),
            ("seeds/s.csv", "a\n1\n"),
            ("snapshots/snap.sql", "{% snapshot snap %}select 1{% endsnapshot %}\n"),
            ("analyses/a.sql", "select 1"),
            ("tests/t.sql", "select 1"),
            ("macros/mac.sql", "{% macro mac() %}1{% endmacro %}\n"),
            ("macros/positive.sql", "{% test positive(model, column_name) %}select 1{% endtest %}\n"),
            ("macros/mat.sql", "{% materialization mat, default %}select 1{% endmaterialization %}\n"),
            ("functions/f.sql", "x * 2"),
        ];
        let root = crate::project::scratch_project("features-each-type-s-kind", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let symbols = workspace_symbols(&project, "");
        let mut kinds: Vec<(&str, u64)> = symbols
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["data"]["resourceType"].as_str().unwrap(), s["kind"].as_u64().unwrap()))
            .collect();
        kinds.sort();
        // LSP's Class 5, Struct 23, Event 24, Function 12, String 15, Interface 11; a generic test's block is a
        // macro, dbt's resource type `source` a source table.
        let expected = [
            ("analysis", 5),
            ("doc", 15),
            ("function", 12),
            ("macro", 12),
            ("macro", 12),
            ("model", 5),
            ("seed", 23),
            ("snapshot", 5),
            ("source", 11),
            ("test", 24),
        ];
        assert_eq!(kinds, expected, "each resource's type and kind, a generic test's block a macro");
        // The same kinds in the outline of each block that defines a node: a materialization, which the scan
        // holds as no resource, is a macro to dbt too.
        let blocks = ["snapshots/snap.sql", "macros/mac.sql", "macros/positive.sql", "macros/mat.sql", "models/d.md"];
        let outlined: Vec<(String, u64)> = blocks
            .into_iter()
            .flat_map(|path| {
                let (_, text) = files.iter().find(|(file, _)| *file == path).unwrap();
                document_symbols(text, true, &uri(&root.join(path))).as_array().unwrap().clone()
            })
            .map(|s| (s["detail"].as_str().unwrap().to_owned(), s["kind"].as_u64().unwrap()))
            .collect();
        let expected = [("snapshot", 5), ("macro", 12), ("test", 12), ("materialization", 12), ("docs", 15)];
        assert_eq!(outlined, expected.map(|(detail, kind)| (detail.to_owned(), kind)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn each_resource_type_has_the_outline_s_kind_in_the_call_hierarchy() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        // The kinds of the outline agreed with dbt-mode, which dbt lsp's listing may bring to the hierarchy; a
        // type that dbt adds, or a hook, stays a Class.
        let expected = [
            ("model", 5),
            ("seed", 23),
            ("snapshot", 5),
            ("analysis", 5),
            ("test", 24),
            ("unit_test", 24),
            ("macro", 12),
            ("function", 12),
            ("doc", 15),
            ("source", 11),
            ("exposure", 19),
            ("metric", 16),
            ("semantic_model", 5),
            ("saved_query", 6),
            ("group", 3),
            ("operation", 5),
        ];
        for (resource_type, kind) in expected {
            let item = Entity {
                id: format!("{resource_type}.shop.x"),
                name: "x".to_owned(),
                resource_type,
                path: fixture().join("models/x.sql"),
                ..Entity::default()
            };
            assert_eq!(hierarchy_item(&item, &project)["kind"], kind, "{resource_type}");
        }
    }

    #[test]
    fn call_hierarchy_goes_both_ways_from_a_call_site_or_a_definition() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        let graph = Graph::scanned(&project, None);
        let orders = fixture().join("models/marts/orders.sql");
        let text = project.text(&orders).unwrap();
        let on_ref = text.find("stg_orders").unwrap();
        let prepared = prepare_call_hierarchy(&project, &graph, &orders, on_ref).unwrap();
        assert_eq!(prepared[0]["data"]["uniqueId"], "model.shop.stg_orders");
        let on_select = text.rfind("select").unwrap();
        let prepared = prepare_call_hierarchy(&project, &graph, &orders, on_select).unwrap();
        assert_eq!(prepared[0]["data"]["uniqueId"], "model.shop.orders");

        let incoming = calls(&project, &graph, "model.shop.orders", true);
        let mut callers: Vec<&str> =
            incoming.as_array().unwrap().iter().map(|c| c["from"]["detail"].as_str().unwrap()).collect();
        callers.sort();
        assert_eq!(callers, ["analysis.shop.order_counts", "model.shop.customers", "test.shop.assert_positive_amount"]);
        let outgoing = calls(&project, &graph, "model.shop.orders", false);
        let mut callees: Vec<&str> =
            outgoing.as_array().unwrap().iter().map(|c| c["to"]["detail"].as_str().unwrap()).collect();
        callees.sort();
        assert_eq!(callees, ["macro.shop_utils.is_positive", "model.shop.stg_customers", "model.shop.stg_orders"]);
        let stg_orders = outgoing.as_array().unwrap().iter().find(|c| c["to"]["detail"] == "model.shop.stg_orders");
        assert_eq!(stg_orders.unwrap()["fromRanges"][0]["start"], json!({"line": 4, "character": 21}));
    }

    #[test]
    fn nothing_to_prepare_outside_any_resource() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        let graph = Graph::scanned(&project, None);
        let readme = fixture().join("packages.yml");
        assert_eq!(prepare_call_hierarchy(&project, &graph, &readme, 0), None);
    }

    fn index() -> ParseIndex {
        let model = |name: &str, materialized: &str, relation: Value| {
            json!({"unique_id": format!("model.shop.{name}"), "name": name, "resource_type": "model",
                   "package_name": "shop", "original_file_path": format!("models/{name}.sql"),
                   "relation_name": relation, "config": {"materialized": materialized}})
        };
        let raw = json!({"unique_id": "source.shop.raw.orders", "name": "orders", "source_name": "raw",
                         "resource_type": "source", "package_name": "shop", "relation_name": "\"raw_orders\"",
                         "config": {}});
        // Ephemeral, so that their hint would show were they reached: only the lookup keeps the analysis's away,
        // since a `ref` names no analysis; the lookup and the disabled filter both keep `off`'s, which the last
        // manifest disables, each pinned by a test of its own (`Lookup::node`'s, the proxy's hints).
        let analysis = json!({"unique_id": "analysis.shop.counts", "name": "counts", "resource_type": "analysis",
                              "package_name": "shop", "original_file_path": "analyses/counts.sql",
                              "config": {"materialized": "ephemeral"}});
        let off = json!({"unique_id": "model.shop.off", "name": "off", "resource_type": "model",
                         "package_name": "shop", "original_file_path": "models/off.sql",
                         "config": {"materialized": "ephemeral"}});
        ParseIndex::of(&json!({"metadata": {"project_name": "shop"}, "nodes": {
            "model.shop.orders": model("orders", "table", json!("\"shop\".\"main\".\"orders\"")),
            "model.shop.eph": model("eph", "ephemeral", Value::Null),
            "analysis.shop.counts": analysis},
            "sources": {"source.shop.raw.orders": raw},
            "disabled": {"model.shop.off": [off]}}))
    }

    /// The project at /p that `index()` was parsed from: the root `shop`, its source `raw.orders`, and no files,
    /// since `hinted` runs in two tests at once.
    fn lookup_project() -> Project {
        let text = "version: 2\nsources:\n  - name: raw\n    tables:\n      - name: orders\n";
        let sources = project::source_tables_in(
            "shop",
            Path::new("/p/models/_sources.yml"),
            text,
            &crate::yaml::parse(text).unwrap(),
        );
        Project {
            root_package: "shop".into(),
            packages: HashMap::from([("shop".to_owned(), PathBuf::from("/p"))]),
            entities: Vec::new(),
            sources,
            files: HashMap::new(),
        }
    }

    /// The labels of TEXT's hints with the text up to each, all of TEXT asked.
    fn hinted(text: &str) -> Vec<(String, String)> {
        let (project, index, path) = (lookup_project(), index(), Path::new("/p/models/x.sql"));
        let hints = inlay_hints(text, scan::calls(text), &Lookup::new(&project, Some(&index)), path, 0..text.len());
        let lines = LineStarts::new(text);
        let before = |hint: &Value| text[..lines.offset_of(&hint["position"]).unwrap()].to_owned();
        hints.iter().map(|hint| (hint["label"].as_str().unwrap().to_owned(), before(hint))).collect()
    }

    #[test]
    fn a_relation_hint_follows_the_closing_parenthesis_of_each_call() {
        let orders = "\"shop\".\"main\".\"orders\"".to_owned();
        let shapes = [
            "{{ ref('orders') }}",
            "{{ ref( 'orders' ) }}",
            "{{ ref(\n    'orders'\n) }}",
            "{{- ref('orders') -}}",
            "{{ ref('shop', 'orders') }}",
        ];
        for text in shapes {
            let end = text.rfind(')').unwrap() + 1;
            assert_eq!(hinted(text), [(orders.clone(), text[..end].to_owned())], "{text}");
        }
        let source = "select * from {{ source('raw', 'orders') }}";
        assert_eq!(hinted(source), [("\"raw_orders\"".to_owned(), source[..source.len() - 3].to_owned())]);
    }

    #[test]
    fn a_relation_hint_is_dbt_s_or_none() {
        assert_eq!(hinted("{{ ref('eph') }}"), [("ephemeral".to_owned(), "{{ ref('eph')".to_owned())]);
        // A `ref` that a keyword argument follows, with no closing parenthesis right after its name, a disabled node,
        // an analysis, an unknown node.
        for text in [
            "{{ ref('orders', v=2) }}",
            "{{ ref('orders', version=2) }}",
            "{{ ref('off') }}",
            "{{ ref('counts') }}",
            "{{ ref('nowhere') }}",
        ] {
            assert_eq!(hinted(text), Vec::new(), "{text}");
        }
        let (project, index, path) = (lookup_project(), index(), Path::new("/p/models/x.sql"));
        let hints = inlay_hints(
            "{{ ref('orders') }}",
            scan::calls("{{ ref('orders') }}"),
            &Lookup::new(&project, Some(&index)),
            path,
            0..3,
        );
        assert_eq!(hints, Vec::<Value>::new());
    }

    #[test]
    fn a_relation_hint_has_its_kind_tooltip_and_place_in_utf_16() {
        // 𝄞 is two UTF-16 units before the call on its line.
        let text = "select '𝄞' as x, * from {{ ref('orders') }}";
        let (project, index, path) = (lookup_project(), index(), Path::new("/p/models/x.sql"));
        let hints = inlay_hints(text, scan::calls(text), &Lookup::new(&project, Some(&index)), path, 0..text.len());
        let expected = json!([{"position": {"line": 0, "character": 41}, "label": "\"shop\".\"main\".\"orders\"",
                               "kind": 1, "paddingLeft": true, "tooltip": "model.shop.orders"}]);
        assert_eq!(Value::Array(hints), expected);
    }

    #[test]
    fn a_hover_is_markdown_with_hard_breaks_or_plain_text() {
        let text = "model table, x\ntags a";
        let cases = [
            (true, true, json!({"kind": "markdown", "value": "model table, x  \ntags a"})),
            (false, true, json!({"kind": "markdown", "value": text})),
            (true, false, json!({"kind": "plaintext", "value": text})),
            (false, false, json!({"kind": "plaintext", "value": text})),
        ];
        for (hard, markdown, contents) in cases {
            assert_eq!(hover(text, hard, markdown), json!({"contents": contents}), "hard {hard}, markdown {markdown}");
        }
    }
}
