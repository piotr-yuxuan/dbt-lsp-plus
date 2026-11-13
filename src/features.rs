// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The answers of the features the proxy adds, as LSP JSON.  Pure: the
//! texts, the project and the graph are inputs.

use crate::graph::{Edge, Graph, Item};
use crate::project::Project;
use crate::scan::{self, Kind, Symbol};
use crate::text::{Encoding, LineIndex};
use serde_json::{Value, json};
use std::collections::HashMap;
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

pub fn uri(path: &Path) -> String {
    Url::from_file_path(path).map(String::from).unwrap_or_default()
}

/// Closes the innermost open symbol of STACK into its parent, or into ROOTS.
fn close(stack: &mut Vec<(Symbol, Vec<Value>)>, roots: &mut Vec<Value>, index: &LineIndex, encoding: Encoding) {
    let Some((symbol, children)) = stack.pop() else { return };
    let (kind, detail) = outline_kind(symbol.kind);
    let value = json!({
        "name": symbol.name,
        "detail": detail,
        "kind": kind,
        "range": index.range(symbol.range.start, symbol.range.end, encoding),
        "selectionRange": index.range(symbol.name_range.start, symbol.name_range.end, encoding),
        "children": children,
    });
    match stack.last_mut() {
        Some((_, siblings)) => siblings.push(value),
        None => roots.push(value),
    }
}

/// The outline of TEXT: `DocumentSymbol`s nested by extent, or, for a
/// client without hierarchical support, `SymbolInformation`s at URI.
pub fn document_symbols(text: &str, encoding: Encoding, hierarchical: bool, uri: &str) -> Value {
    let index = LineIndex::new(text);
    let contains =
        |outer: &Symbol, inner: &Symbol| outer.range.start <= inner.range.start && inner.range.end <= outer.range.end;
    let mut stack: Vec<(Symbol, Vec<Value>)> = Vec::new();
    let mut roots = Vec::new();
    let mut flat = Vec::new();
    for symbol in scan::outline(text, !uri.ends_with(".md")) {
        while stack.last().is_some_and(|(open, _)| !contains(open, &symbol)) {
            close(&mut stack, &mut roots, &index, encoding);
        }
        let container = stack.last().map(|(open, _)| open.name.clone());
        let (kind, _) = outline_kind(symbol.kind);
        let location = json!({"uri": uri, "range": index.range(symbol.range.start, symbol.range.end, encoding)});
        let mut information = json!({"name": symbol.name, "kind": kind, "location": location});
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

    #[test]
    fn workspace_symbols_match_a_subsequence_and_name_the_type() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        let symbols = workspace_symbols(&project, "PGVW", Encoding::Utf16);
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
}
