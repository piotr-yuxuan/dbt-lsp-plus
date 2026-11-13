// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The answers of the features the proxy adds, as LSP JSON.  Pure: the
//! texts, the project and the graph are inputs.

use crate::graph::{Edge, Graph, Item};
use crate::project::Project;
use crate::properties;
use crate::scan::{self, Kind, Symbol};
use crate::text::{Encoding, LineIndex};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use url::Url;

/// At most this many workspace symbols per answer.
pub const MAX_WORKSPACE_SYMBOLS: usize = 20_000;

/// LSP's `SymbolKind` of a dbt resource type.
fn resource_kind(resource_type: &str) -> u32 {
    match resource_type {
        "seed" => 23,   // Struct
        "source" => 11, // Interface
        "macro" => 12,  // Function
        "test" => 24,   // Event
        "doc" => 15,    // String
        _ => 5,         // Class: model, snapshot, analysis, and what dbt adds
    }
}

/// LSP's `SymbolKind` and a label of a block or CTE of the outline.
fn outline_kind(kind: Kind) -> (u32, &'static str) {
    match kind {
        Kind::Macro => (12, "macro"),
        Kind::Test => (12, "test"),
        Kind::Materialization => (12, "materialization"),
        Kind::Snapshot => (5, "snapshot"),
        Kind::Docs => (15, "docs"),
        Kind::Set => (13, "set"),
        Kind::Cte => (23, "CTE"),
    }
}

/// LSP's `SymbolKind` of an entry of a properties file, by its label;
/// any other entry, a model, snapshot, analysis or semantic model, is 5,
/// Class.
const ENTRY_KINDS: [(&str, u32); 10] = [
    ("seed", 23),       // Struct
    ("macro", 12),      // Function
    ("source", 2),      // Module
    ("table", 11),      // Interface, as a source table is a workspace symbol
    ("column", 8),      // Field
    ("exposure", 19),   // Object
    ("metric", 16),     // Number
    ("saved query", 6), // Method
    ("unit test", 24),  // Event
    ("group", 3),       // Namespace
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
fn close(stack: &mut Vec<(Outlined, Vec<Value>)>, roots: &mut Vec<Value>, index: &LineIndex, encoding: Encoding) {
    let Some((symbol, children)) = stack.pop() else { return };
    let value = json!({
        "name": symbol.name,
        "detail": symbol.detail,
        "kind": symbol.kind,
        "range": index.range(symbol.range.start, symbol.range.end, encoding),
        "selectionRange": index.range(symbol.name_range.start, symbol.name_range.end, encoding),
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
pub fn document_symbols(text: &str, encoding: Encoding, hierarchical: bool, uri: &str) -> Value {
    let index = LineIndex::new(text);
    let contains = |outer: &Outlined, inner: &Outlined| {
        outer.range.start <= inner.range.start && inner.range.end <= outer.range.end
    };
    let symbols: Vec<Outlined> = if uri.ends_with(".yml") || uri.ends_with(".yaml") {
        let outlined = |entry: properties::Entry| Outlined {
            kind: ENTRY_KINDS.iter().find(|(label, _)| *label == entry.label).map_or(5, |(_, kind)| *kind),
            detail: entry.label,
            name: entry.name,
            range: entry.range,
            name_range: entry.name_range,
        };
        properties::entries(text).into_iter().map(outlined).collect()
    } else {
        let outlined = |symbol: Symbol| {
            let (kind, detail) = outline_kind(symbol.kind);
            Outlined { name: symbol.name, kind, detail, range: symbol.range, name_range: symbol.name_range }
        };
        scan::outline(text, !uri.ends_with(".md")).into_iter().map(outlined).collect()
    };
    let mut stack: Vec<(Outlined, Vec<Value>)> = Vec::new();
    let mut roots = Vec::new();
    let mut flat = Vec::new();
    for symbol in symbols {
        while stack.last().is_some_and(|(open, _)| !contains(open, &symbol)) {
            close(&mut stack, &mut roots, &index, encoding);
        }
        let container = stack.last().map(|(open, _)| open.name.clone());
        let location = json!({"uri": uri, "range": index.range(symbol.range.start, symbol.range.end, encoding)});
        let mut information = json!({"name": symbol.name, "kind": symbol.kind, "location": location});
        if let Some(container) = container {
            information["containerName"] = json!(container);
        }
        flat.push(information);
        stack.push((symbol, Vec::new()));
    }
    while !stack.is_empty() {
        close(&mut stack, &mut roots, &index, encoding);
    }
    Value::Array(if hierarchical { roots } else { flat })
}

/// Every resource of PROJECT whose name holds the characters of QUERY in
/// order, ignoring case, as LSP 3.17 `WorkspaceSymbol`s that are also valid
/// `SymbolInformation`s: the package as container, and dbt's unique id and
/// resource type as data.
pub fn workspace_symbols(project: &Project, query: &str, encoding: Encoding) -> Value {
    let query = query.to_lowercase();
    let matches = |name: &str| {
        let mut chars = name.chars().flat_map(char::to_lowercase);
        query.chars().all(|q| chars.any(|c| c == q))
    };
    let mut indexes: HashMap<&Path, LineIndex> = HashMap::new();
    let mut symbols = Vec::new();
    for entity in project.entities.iter().filter(|entity| matches(&entity.name)).take(MAX_WORKSPACE_SYMBOLS) {
        let text = project.texts.get(&entity.path).map_or("", String::as_str);
        let index = indexes.entry(&entity.path).or_insert_with(|| LineIndex::new(text));
        symbols.push(json!({
            "name": entity.name,
            "kind": resource_kind(entity.resource_type),
            "location": {"uri": uri(&entity.path), "range": index.range(entity.name_range.start, entity.name_range.end, encoding)},
            "containerName": entity.package,
            "data": {"uniqueId": entity.id, "resourceType": entity.resource_type},
        }));
    }
    Value::Array(symbols)
}

/// The `CallHierarchyItem` of ITEM.
fn hierarchy_item(item: &Item, project: &Project, encoding: Encoding) -> Value {
    let index = LineIndex::new(project.texts.get(&item.path).map_or("", String::as_str));
    json!({
        "name": item.name,
        "kind": resource_kind(&item.resource_type),
        "detail": item.id,
        "uri": uri(&item.path),
        "range": index.range(item.range.start, item.range.end, encoding),
        "selectionRange": index.range(item.name_range.start, item.name_range.end, encoding),
        "data": {"uniqueId": item.id, "resourceType": item.resource_type},
    })
}

/// The item at OFFSET of the file at PATH: the resource that a call there
/// names, else the innermost resource defined there.
pub fn prepare_call_hierarchy(
    project: &Project,
    graph: &Graph,
    path: &Path,
    offset: usize,
    encoding: Encoding,
) -> Value {
    let in_file = |id: &str| graph.items.get(id).is_some_and(|item| item.path == path);
    let called = graph
        .edges
        .iter()
        .find(|edge| in_file(&edge.from) && edge.sites.iter().any(|site| site.start <= offset && offset <= site.end));
    let defined = || {
        let around = graph
            .items
            .values()
            .filter(|item| item.path == path && item.range.start <= offset && offset <= item.range.end);
        around.min_by_key(|item| (item.range.len(), item.id.clone()))
    };
    match called.map(|edge| &graph.items[&edge.to]).or_else(defined) {
        Some(item) => json!([hierarchy_item(item, project, encoding)]),
        None => Value::Null,
    }
}

/// The calls of an item: those that call ID when INCOMING, else those ID makes.
pub fn calls(project: &Project, graph: &Graph, id: &str, incoming: bool, encoding: Encoding) -> Value {
    let edges = graph.edges.iter().filter(|edge| if incoming { edge.to == id } else { edge.from == id });
    let answer = edges.map(|edge: &Edge| {
        let (other, key) = if incoming { (&edge.from, "from") } else { (&edge.to, "to") };
        let caller = &graph.items[&edge.from];
        let index = LineIndex::new(project.texts.get(&caller.path).map_or("", String::as_str));
        let ranges: Vec<Value> = edge.sites.iter().map(|site| index.range(site.start, site.end, encoding)).collect();
        json!({key: hierarchy_item(&graph.items[other], project, encoding), "fromRanges": ranges})
    });
    Value::Array(answer.collect())
}

/// The server's answer to a hover, MESSAGE, with SUMMARY put first: before
/// a rule in markdown, above a blank line in plain text, as the first item
/// of an array of marked strings.  A null result becomes the summary alone,
/// in markdown when MARKDOWN; an error, or contents of a shape LSP does not
/// define, gives none, so that the answer passes as it came.
pub fn merge_hover(message: &Value, summary: &str, markdown: bool) -> Option<Value> {
    let mut merged = message.clone();
    let result = merged.get_mut("result")?;
    // A line break in markdown needs two spaces before it.
    let marked = summary.replace('\n', "  \n");
    let kind = result.pointer("/contents/kind").and_then(Value::as_str).map(str::to_owned);
    let kind = kind.or_else(|| result.is_null().then(|| if markdown { "markdown" } else { "plaintext" }.to_owned()));
    let contents = result.get("contents").cloned().unwrap_or(Value::Null);
    let joined = match (kind.as_deref(), contents) {
        (Some(kind), contents) => {
            let (first, rule) = if kind == "markdown" { (marked, "---\n\n") } else { (summary.to_owned(), "") };
            let rest = contents["value"].as_str().map_or_else(String::new, |value| format!("\n\n{rule}{value}"));
            json!({"kind": kind, "value": format!("{first}{rest}")})
        }
        (None, Value::Array(items)) => json!(std::iter::once(json!(marked)).chain(items).collect::<Vec<_>>()),
        (None, Value::String(text)) => json!(format!("{marked}\n\n---\n\n{text}")),
        (None, item @ Value::Object(_)) => json!([marked, item]),
        _ => return None,
    };
    if result.is_null() {
        *result = json!({});
    }
    result["contents"] = joined;
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let symbols = document_symbols(SNAPSHOT, Encoding::Utf16, true, "file:///s.sql");
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
    fn a_flat_outline_names_the_container() {
        let symbols = document_symbols(SNAPSHOT, Encoding::Utf16, false, "file:///s.sql");
        let flat: Vec<(&Value, Option<&Value>)> =
            symbols.as_array().unwrap().iter().map(|s| (&s["name"], s.get("containerName"))).collect();
        assert_eq!(flat, [(&json!("s"), None), (&json!("a"), Some(&json!("s"))), (&json!("b"), Some(&json!("s")))]);
        assert_eq!(symbols[1]["location"]["uri"], "file:///s.sql");
    }

    const PROPERTIES: &str = "models:\n  - name: orders\n    description: 'with x as (select 1) {% macro hidden() %}'\n    \
        columns:\n      - name: id\nseeds:\n  - name: s\nsnapshots:\n  - name: sn\nanalyses:\n  - name: a\n\
        macros:\n  - name: m\nsources:\n  - name: raw\n    tables:\n      - name: t\nexposures:\n  - name: e\n\
        metrics:\n  - name: me\nsemantic_models:\n  - name: sm\nsaved_queries:\n  - name: q\nunit_tests:\n  \
        - name: u\ngroups:\n  - name: g\n";

    #[test]
    fn outlines_a_properties_file_with_the_agreed_kinds_and_no_sql() {
        let symbols = document_symbols(PROPERTIES, Encoding::Utf16, false, "file:///p.yml");
        let flat: Vec<(&str, u64, Option<&str>)> = symbols
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].as_str().unwrap(), s["kind"].as_u64().unwrap(), s["containerName"].as_str()))
            .collect();
        let expected = [
            ("orders", 5, None),
            ("id", 8, Some("orders")),
            ("s", 23, None),
            ("sn", 5, None),
            ("a", 5, None),
            ("m", 12, None),
            ("raw", 2, None),
            ("t", 11, Some("raw")),
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
        let symbols = document_symbols(PROPERTIES, Encoding::Utf16, true, "file:///p.yaml");
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
        assert_eq!(symbols[5]["children"][0]["detail"], "table");
        assert_eq!(
            symbols[0]["range"],
            json!({"start": {"line": 1, "character": 4}, "end": {"line": 4, "character": 16}})
        );
        assert_eq!(
            symbols[0]["selectionRange"],
            json!({"start": {"line": 1, "character": 10}, "end": {"line": 1, "character": 16}})
        );
    }

    #[test]
    fn workspace_symbols_match_a_subsequence_and_name_the_type() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        let symbols = workspace_symbols(&project, "EVPGVW", Encoding::Utf16);
        assert_eq!(symbols.as_array().unwrap().len(), 1);
        assert_eq!(symbols[0]["name"], "events.page_views");
        assert_eq!(symbols[0]["containerName"], "shop");
        assert_eq!(symbols[0]["data"], json!({"uniqueId": "source.shop.events.page_views", "resourceType": "source"}));
        assert_eq!(symbols[0]["location"]["range"]["start"], json!({"line": 13, "character": 15}));
        let all = workspace_symbols(&project, "", Encoding::Utf16);
        assert_eq!(all.as_array().unwrap().len(), project.entities.len());
    }

    #[test]
    fn call_hierarchy_goes_both_ways_from_a_call_site_or_a_definition() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        let graph = Graph::scanned(&project);
        let orders = fixture().join("models/marts/orders.sql");
        let text = &project.texts[&orders];
        let on_ref = text.find("stg_orders").unwrap();
        let prepared = prepare_call_hierarchy(&project, &graph, &orders, on_ref, Encoding::Utf16);
        assert_eq!(prepared[0]["data"]["uniqueId"], "model.shop.stg_orders");
        let on_select = text.rfind("select").unwrap();
        let prepared = prepare_call_hierarchy(&project, &graph, &orders, on_select, Encoding::Utf16);
        assert_eq!(prepared[0]["data"]["uniqueId"], "model.shop.orders");

        let incoming = calls(&project, &graph, "model.shop.orders", true, Encoding::Utf16);
        let mut callers: Vec<&str> =
            incoming.as_array().unwrap().iter().map(|c| c["from"]["detail"].as_str().unwrap()).collect();
        callers.sort();
        assert_eq!(callers, ["analysis.shop.order_counts", "model.shop.customers", "test.shop.assert_positive_amount"]);
        let outgoing = calls(&project, &graph, "model.shop.orders", false, Encoding::Utf16);
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
        let graph = Graph::scanned(&project);
        let readme = fixture().join("packages.yml");
        assert_eq!(prepare_call_hierarchy(&project, &graph, &readme, 0, Encoding::Utf16), Value::Null);
    }

    #[test]
    fn puts_the_summary_first_in_every_shape_of_hover() {
        let summary = "model table, x\ntags a";
        let answer = |result: Value| json!({"jsonrpc": "2.0", "id": 1, "result": result});
        let contents = |merged: Option<Value>| merged.unwrap()["result"]["contents"].clone();
        let markdown = answer(json!({"contents": {"kind": "markdown", "value": "Server"}, "range": "kept"}));
        let merged = merge_hover(&markdown, summary, true).unwrap();
        assert_eq!(merged["result"]["contents"]["value"], "model table, x  \ntags a\n\n---\n\nServer");
        assert_eq!(merged["result"]["range"], "kept");
        let plain = answer(json!({"contents": {"kind": "plaintext", "value": "Server"}}));
        assert_eq!(contents(merge_hover(&plain, summary, true))["value"], "model table, x\ntags a\n\nServer");
        let string = answer(json!({"contents": "Server"}));
        assert_eq!(contents(merge_hover(&string, summary, true)), "model table, x  \ntags a\n\n---\n\nServer");
        let code = answer(json!({"contents": {"language": "sql", "value": "select 1"}}));
        assert_eq!(contents(merge_hover(&code, summary, true))[1]["language"], "sql");
        let array = answer(json!({"contents": ["a", "b"]}));
        assert_eq!(contents(merge_hover(&array, summary, true)), json!(["model table, x  \ntags a", "a", "b"]));
        let null = answer(Value::Null);
        assert_eq!(contents(merge_hover(&null, summary, false)), json!({"kind": "plaintext", "value": summary}));
        let error = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "x"}});
        assert_eq!(merge_hover(&error, summary, true), None);
        assert_eq!(merge_hover(&answer(json!({"contents": 3})), summary, true), None);
    }
}
