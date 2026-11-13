// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy in front of the real dbt lsp.  Opt-in, since it needs dbt 2:
//!
//!     DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test live -- --ignored
//!
//! It runs on a copy of the fixture, from an empty environment with a
//! scratch home, so that dbt writes nothing into the repository nor the
//! user's home.

mod support;

use serde_json::json;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use support::{Session, scratch, uri};

fn copy_tree(from: &Path, to: &Path) {
    let status = Command::new("cp").arg("-R").arg(from).arg(to).status().expect("run cp");
    assert!(status.success(), "copy {from:?} to {to:?}");
}

#[test]
#[ignore = "needs DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable"]
fn in_front_of_the_real_dbt_lsp() {
    let dbt = std::env::var("DBT_LSP_PLUS_TEST_DBT").expect("DBT_LSP_PLUS_TEST_DBT names a dbt 2 executable");
    let dir = scratch("live");
    let (project, home) = (dir.join("shop"), dir.join("home"));
    copy_tree(&support::fixture(), &project);
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
    assert_eq!(result["capabilities"]["documentSymbolProvider"], json!(true));
    assert_eq!(result["capabilities"]["callHierarchyProvider"], json!(true));
    assert_eq!(result["capabilities"]["hoverProvider"], json!(true));

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

    // Only the server resolves order_summary's ref(var('summary_upstream')) to orders.
    let item = json!({"name": "orders", "kind": 5, "uri": uri(&orders),
                      "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                      "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                      "data": {"uniqueId": "model.shop.orders"}});
    let callers =
        session.poll(Duration::from_secs(120), ("callHierarchy/incomingCalls", json!({"item": item})), |calls| {
            calls
                .as_array()
                .is_some_and(|calls| calls.iter().any(|c| c["from"]["detail"] == "model.shop.order_summary"))
        });
    let mut callers: Vec<&str> =
        callers.as_array().unwrap().iter().map(|c| c["from"]["detail"].as_str().unwrap()).collect();
    callers.sort();
    assert!(
        callers.contains(&"model.shop.customers") && callers.contains(&"analysis.shop.order_counts"),
        "{callers:?}"
    );

    let stg_customers = json!({"name": "stg_customers", "kind": 5, "uri": uri(&project.join("models/staging/stg_customers.sql")),
                               "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                               "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                               "data": {"uniqueId": "model.shop.stg_customers"}});
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": stg_customers}));
    let callees: Vec<&str> =
        outgoing["result"].as_array().unwrap().iter().map(|c| c["to"]["detail"].as_str().unwrap()).collect();
    assert_eq!(callees, ["source.shop.raw.customers"]);

    let symbols = session.request("workspace/symbol", json!({"query": "page"}));
    assert_eq!(symbols["result"][0]["name"], "events.page_views");
    assert!(session.finish().success());
}
