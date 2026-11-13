// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy in front of the real dbt lsp.  Opt-in, since it needs dbt 2:
//!
//!     DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test proxy -- --ignored
//!
//! It runs on a copy of the fixture, from an empty environment with a
//! scratch home, so that dbt writes nothing into the repository nor the
//! user's home.

use crate::support::{Session, copy_fixture, uri};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[test]
#[ignore = "needs DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable"]
fn in_front_of_the_real_dbt_lsp() {
    let dbt = std::env::var("DBT_LSP_PLUS_TEST_DBT").expect("DBT_LSP_PLUS_TEST_DBT names a dbt 2 executable");
    let project = copy_fixture("live");
    std::fs::write(project.join("models/marts/_models.yml"), LIVE_MODELS).unwrap();
    let home = project.with_file_name("home");
    std::fs::create_dir_all(&home).unwrap();
    let project_arg = project.to_string_lossy().into_owned();
    let server = [dbt, "lsp".into(), "--project-dir".into(), project_arg.clone(), "--profiles-dir".into(), project_arg];
    // DO_NOT_TRACK does not stop the telemetry of dbt 2.0.9; this flag does.
    let server = [&server[..], &["--no-send-anonymous-usage-stats".to_owned()]].concat();
    let before = paths(&project);
    // An empty environment but for PATH and these, run in the project: dbt
    // writes nowhere else.
    let env = [("HOME", home.as_path()), ("DO_NOT_TRACK", Path::new("1"))];
    let mut session = Session::start(&server, &env, Some(&project));
    session.timeout = Duration::from_secs(180);

    let result = session.initialize(&project);
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp", "{result}");
    let announced = [
        "documentSymbolProvider",
        "callHierarchyProvider",
        "hoverProvider",
        "foldingRangeProvider",
        "referencesProvider",
        "renameProvider",
    ];
    for capability in announced {
        assert_eq!(result["capabilities"][capability], json!(true), "{capability}");
    }
    document_features(&mut session, &project);
    navigation(&mut session, &project);
    lineage(&mut session, &project);
    let symbols = session.request("workspace/symbol", json!({"query": "events.page"}));
    assert_eq!(symbols["result"][0]["name"], "events.page_views");
    recorded_observations(&mut session, &result);
    node_information(&mut session, &project);
    rename_a_source_table_and_compile(&mut session, &project);
    assert!(session.finish().success());
    let added: Vec<PathBuf> = paths(&project).difference(&before).cloned().collect();
    assert!(added.is_empty(), "the project gained {added:?}");
}

/// Every path under DIR, relative to it, but dbt lsp's own `target/` and
/// `logs/`.
fn paths(dir: &Path) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        for path in std::fs::read_dir(&next).into_iter().flatten().flatten().map(|entry| entry.path()) {
            let relative = path.strip_prefix(dir).unwrap().to_owned();
            if relative == Path::new("target") || relative == Path::new("logs") {
                continue;
            }
            if path.is_dir() && !path.is_symlink() {
                pending.push(path);
            }
            found.insert(relative);
        }
    }
    found
}

/// The proxy's own `dbt parse`: a node's information and the summary on a
/// hover of its ref.
fn node_information(session: &mut Session, project: &Path) {
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    let orders = session.poll(Duration::from_secs(120), request, |result| result.is_object());
    assert_eq!(orders["config"]["materialized"], "table", "{orders}");
    assert_eq!(orders["relation"], "\"shop\".\"main\".\"orders\"");
    let on_ref = json!({"textDocument": {"uri": uri(&project.join("models/marts/orders.sql"))}, "position": {"line": 4, "character": 28}});
    let hover = session.request("textDocument/hover", on_ref);
    let value = hover["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.starts_with("model view, \"shop\".\"main\".\"stg_orders\""), "{hover}");
    let kept = value.split_once("\n\n---\n\n").map(|(_, server)| server.trim());
    assert!(kept.is_some_and(|server| !server.is_empty()), "the server's text is gone: {hover}");
    // Inlay hints: dbt lsp 2.0.9 answers [] on a model; the proxy's relations follow.
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 100, "character": 0}});
    let orders = uri(&project.join("models/marts/orders.sql"));
    let hints = session.request("textDocument/inlayHint", json!({"textDocument": {"uri": orders}, "range": range}));
    let labels: Vec<&str> = hints["result"].as_array().unwrap().iter().filter_map(|h| h["label"].as_str()).collect();
    let expected = ["\"shop\".\"main\".\"stg_orders\"", "\"shop\".\"main\".\"stg_customers\""];
    assert_eq!(labels, expected, "{hints}");
    assert_eq!(hints["result"][0]["position"], json!({"line": 4, "character": 38}), "{hints}");
    // The server's own answers, observed: null on a macro file and on a
    // properties file without a ref, where the proxy has nothing to add.
    for file in ["macros/money.sql", "models/staging/_sources.yml"] {
        let params = json!({"textDocument": {"uri": uri(&project.join(file))}, "range": range});
        let hints = session.request("textDocument/inlayHint", params);
        assert_eq!(hints["result"], Value::Null, "{file}: {hints}");
    }
}

/// The proxy's outline and folding of a model, the outline of a properties
/// file, and the server's hover through it.
fn document_features(session: &mut Session, project: &Path) {
    let orders = project.join("models/marts/orders.sql");
    // dbt lsp 2.0.9 compiles nothing until a document opens or a request comes.
    session.open(&orders, None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&orders)}}));
    let names: Vec<&str> = outline["result"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["orders", "customers", "final"], "the proxy's outline, not the server's tokens");
    // dbt lsp 2.0.9 answers [] on a properties file: the proxy's are its sources and their tables.
    let properties = project.join("models/staging/_sources.yml");
    session.open(&properties, None);
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&properties)}}));
    let named = |symbol: &Value| symbol["name"].as_str().unwrap().to_owned();
    let tables = |symbol: &Value| symbol["children"].as_array().unwrap().iter().map(named).collect::<Vec<_>>();
    let sources: Vec<(String, Vec<String>)> =
        outline["result"].as_array().unwrap().iter().map(|source| (named(source), tables(source))).collect();
    let expected = [("raw", vec!["orders", "customers"]), ("events", vec!["page_views"])];
    let expected: Vec<(String, Vec<String>)> = expected
        .iter()
        .map(|(source, tables)| (source.to_string(), tables.iter().map(|t| t.to_string()).collect()))
        .collect();
    assert_eq!(sources, expected, "{outline}");
    let folding = session.request("textDocument/foldingRange", json!({"textDocument": {"uri": uri(&orders)}}));
    assert_eq!(folding["result"].as_array().map(Vec::len), Some(4), "{folding}");
    // dbt lsp 2.0.9 announces no highlights: the proxy's are the CTE's definition and its three uses.
    let on_orders = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 17, "character": 10}});
    let highlights = session.request("textDocument/documentHighlight", on_orders);
    assert_eq!(highlights["result"].as_array().map(Vec::len), Some(4), "{highlights}");
    // The server's hover passes through: on ref('stg_orders'), line 4.
    let on_ref = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    session.poll(Duration::from_secs(60), ("textDocument/hover", on_ref), |hover| !hover.is_null());
}

/// A properties file that dbt parse 2.0.9 accepts, for the navigation from
/// YAML: a generic test's arguments go under `arguments:`.
const LIVE_MODELS: &str = "\
version: 2
models:
  - name: orders
    description: \"{{ doc('order_status') }}\"
    columns:
      - name: customer_id
        data_tests:
          - relationships:
              arguments:
                to: ref('customers')
                field: customer_id
";

/// The proxy's definitions and hover in a properties file, where dbt lsp
/// 2.0.9 answers null.
fn navigation(session: &mut Session, project: &Path) {
    let yml = project.join("models/marts/_models.yml");
    session.open(&yml, None);
    let at = |needle: &str, delta: usize| {
        let (line, column) = LIVE_MODELS.lines().enumerate().find_map(|(n, l)| l.find(needle).map(|c| (n, c))).unwrap();
        json!({"textDocument": {"uri": uri(&yml)}, "position": {"line": line, "character": column + delta}})
    };
    let file = |answer: &Value| answer["result"]["uri"].as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    assert_eq!(file(&session.request("textDocument/definition", at("ref('customers')", 6))), "customers.sql");
    assert_eq!(file(&session.request("textDocument/definition", at("order_status", 2))), "docs.md");
    assert_eq!(file(&session.request("textDocument/definition", at("name: orders", 8))), "orders.sql");
    // The server's null, observed: the proxy's text alone, and a position
    // without a target forwarded and answered null.
    let hover = session.request("textDocument/hover", at("order_status", 2));
    let text = "The status of an order: placed, shipped or returned.";
    assert_eq!(hover["result"]["contents"]["value"], text, "{hover}");
    let on_column = session.request("textDocument/definition", at("customer_id", 2));
    assert_eq!(on_column["result"], Value::Null, "{on_column}");
}

/// A call hierarchy item for the node ID at PATH of PROJECT.
fn item(project: &Path, path: &str, id: &str) -> Value {
    let zero = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}});
    json!({"name": id.rsplit('.').next().unwrap(), "kind": 5, "uri": uri(&project.join(path)),
           "range": zero, "selectionRange": zero, "data": {"uniqueId": id}})
}

/// The call hierarchy on the server's graph: only the server resolves
/// order_summary's ref(var('summary_upstream')) to orders.
fn lineage(session: &mut Session, project: &Path) {
    let orders = json!({"item": item(project, "models/marts/orders.sql", "model.shop.orders")});
    let summary = |calls: &Value| {
        calls.as_array().is_some_and(|calls| calls.iter().any(|c| c["from"]["detail"] == "model.shop.order_summary"))
    };
    let callers = session.poll(Duration::from_secs(120), ("callHierarchy/incomingCalls", orders), summary);
    let callers: Vec<&str> =
        callers.as_array().unwrap().iter().map(|c| c["from"]["detail"].as_str().unwrap()).collect();
    assert!(
        callers.contains(&"model.shop.customers") && callers.contains(&"analysis.shop.order_counts"),
        "{callers:?}"
    );
    let stg_customers = json!({"item": item(project, "models/staging/stg_customers.sql", "model.shop.stg_customers")});
    let outgoing = session.request("callHierarchy/outgoingCalls", stg_customers);
    let callees: Vec<&str> =
        outgoing["result"].as_array().unwrap().iter().map(|c| c["to"]["detail"].as_str().unwrap()).collect();
    assert_eq!(callees, ["source.shop.raw.customers"]);
}

/// What design.md records of dbt lsp 2.0.9, and the fake models, from the
/// server's initialize RESULT and its listings.
fn recorded_observations(session: &mut Session, result: &Value) {
    assert!(result["capabilities"].get("positionEncoding").is_none(), "{result}");
    let listing = |selectors: &[&str]| json!({"command": "dbt.listNodes", "arguments": selectors});
    let types = |answer: &Value| -> Vec<String> {
        let nodes = answer["result"]["nodes"].as_array().unwrap_or_else(|| panic!("no nodes in {answer}"));
        nodes.iter().map(|node| node["resource_type"].as_str().unwrap().to_owned()).collect()
    };
    let all = types(&session.request("workspace/executeCommand", listing(&["*"])));
    assert!(!all.is_empty() && !all.iter().any(|t| t == "source"), "`*` lists no sources: {all:?}");
    let first_only = types(&session.request("workspace/executeCommand", listing(&["source:*", "*"])));
    assert!(!first_only.is_empty() && first_only.iter().all(|t| t == "source"), "{first_only:?}");
    for id in ["first", "second"] {
        let request =
            json!({"jsonrpc": "2.0", "id": id, "method": "workspace/executeCommand", "params": listing(&["*"])});
        session.send(&request);
    }
    let first = session.wait_for(|m| m["id"] == "first");
    assert_eq!(first["result"]["error"], "Operation cancelled", "{first}");
    session.wait_for(|m| m["id"] == "second");
}

/// Writes the edits of a `WorkspaceEdit`'s CHANGES to disk.
fn apply(changes: &serde_json::Map<String, Value>) {
    for (file, edits) in changes {
        let path = url::Url::parse(file).unwrap().to_file_path().unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        let mut edits: Vec<&Value> = edits.as_array().unwrap().iter().collect();
        edits.sort_by_key(|e| {
            std::cmp::Reverse((e["range"]["start"]["line"].as_u64(), e["range"]["start"]["character"].as_u64()))
        });
        for edit in edits {
            let change = json!({"range": edit["range"], "text": edit["newText"]});
            dbt_lsp_plus::text::apply_change(&mut text, &change, dbt_lsp_plus::text::Encoding::Utf16);
        }
        std::fs::write(&path, text).unwrap();
    }
}

/// Renames `events.page_views`, which has no `identifier:`, through the
/// proxy; once dbt has read the files again, the model that selects from it
/// must still compile to the relation `page_views`.
fn rename_a_source_table_and_compile(session: &mut Session, project: &Path) {
    let model = project.join("models/staging/stg_page_views.sql");
    session.open(&model, None);
    let compiled = |session: &mut Session| {
        let compile = json!({"command": "dbt.compileFile", "arguments": [uri(&model)]});
        let answer = session.request("workspace/executeCommand", compile);
        let file = answer["result"]["file_uri"].as_str().unwrap_or_else(|| panic!("no compiled file: {answer}"));
        url::Url::parse(file).unwrap().to_file_path().unwrap()
    };
    // A compiled file left from before the rename must not answer for after it.
    let before = compiled(session);
    let at =
        json!({"textDocument": {"uri": uri(&model)}, "position": {"line": 0, "character": 37}, "newName": "visits"});
    let edit = session.request("textDocument/rename", at);
    let changes = edit["result"]["changes"].as_object().unwrap_or_else(|| panic!("no edit: {edit}"));
    apply(changes);
    let text = std::fs::read_to_string(&model).unwrap();
    assert_eq!(text, "select * from {{ source('events', 'visits') }}\n");
    let version = json!({"uri": uri(&model), "version": 2});
    let whole = json!({"textDocument": version, "contentChanges": [{"text": text}]});
    session.notify("textDocument/didChange", whole);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&model)}}));
    let yaml = uri(&project.join("models/staging/_sources.yml"));
    session.notify("workspace/didChangeWatchedFiles", json!({"changes": [{"uri": yaml, "type": 2}]}));
    let list = json!({"command": "dbt.listNodes", "arguments": ["source:*"]});
    let renamed = |result: &Value| {
        result["nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.iter().any(|n| n["unique_id"] == "source.shop.events.visits"))
    };
    session.poll(Duration::from_secs(120), ("workspace/executeCommand", list), renamed);
    std::fs::remove_file(&before).unwrap();
    let sql = std::fs::read_to_string(compiled(session)).unwrap();
    assert!(sql.contains("page_views") && !sql.contains("visits"), "the relation changed: {sql}");
}
