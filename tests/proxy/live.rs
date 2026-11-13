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
use std::path::Path;
use std::time::Duration;

#[test]
#[ignore = "needs DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable"]
fn in_front_of_the_real_dbt_lsp() {
    let dbt = std::env::var("DBT_LSP_PLUS_TEST_DBT").expect("DBT_LSP_PLUS_TEST_DBT names a dbt 2 executable");
    let project = copy_fixture("live");
    let home = project.with_file_name("home");
    std::fs::create_dir_all(&home).unwrap();
    let project_arg = project.to_string_lossy().into_owned();
    let server = [dbt, "lsp".into(), "--project-dir".into(), project_arg.clone(), "--profiles-dir".into(), project_arg];
    // An empty environment but for PATH and these, run in the project: dbt
    // writes nowhere else.
    let env = [("HOME", home.as_path()), ("DO_NOT_TRACK", Path::new("1"))];
    let mut session = Session::start(&server, &env, Some(&project));
    session.timeout = Duration::from_secs(180);

    let result = session.initialize(&project);
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp", "{result}");
    for capability in ["documentSymbolProvider", "callHierarchyProvider", "hoverProvider"] {
        assert_eq!(result["capabilities"][capability], json!(true), "{capability}");
    }
    document_features(&mut session, &project);
    lineage(&mut session, &project);
    let symbols = session.request("workspace/symbol", json!({"query": "page"}));
    assert_eq!(symbols["result"][0]["name"], "events.page_views");
    recorded_observations(&mut session, &result);
    assert!(session.finish().success());
}

/// The proxy's outline of a model, and the server's hover through it.
fn document_features(session: &mut Session, project: &Path) {
    let orders = project.join("models/marts/orders.sql");
    // dbt lsp 2.0.9 compiles nothing until a document opens or a request comes.
    session.open(&orders, None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&orders)}}));
    let names: Vec<&str> = outline["result"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["orders", "customers", "final"], "the proxy's outline, not the server's tokens");
    // The server's hover passes through: on ref('stg_orders'), line 4.
    let on_ref = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    session.poll(Duration::from_secs(60), ("textDocument/hover", on_ref), |hover| !hover.is_null());
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
