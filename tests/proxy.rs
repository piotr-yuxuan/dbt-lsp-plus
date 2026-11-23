// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The server, with a scripted fake of dbt as the user's dbt.

#[path = "proxy/cli.rs"]
mod cli;
#[path = "proxy/commands.rs"]
mod commands;
// Opt-in, ignored; in this crate so that one crate uses all of `support`.
#[path = "proxy/conformance.rs"]
mod conformance;
#[path = "proxy/live.rs"]
mod live;
#[path = "proxy/locate.rs"]
mod locate;
#[path = "proxy/plugin.rs"]
mod plugin;
#[path = "proxy/routes.rs"]
mod routes;
mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use support::{Session, copy_fixture, fixture, received_by_server, scratch, until_logged, uri};

fn names(symbols: &Value) -> Vec<&str> {
    symbols.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect()
}

fn logged(log: &Path) -> Vec<Value> {
    received_by_server(log).iter().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

/// Whether the process PID runs, or waits to be reaped.
fn alive(pid: u32) -> bool {
    let mut probe = std::process::Command::new("kill");
    probe.args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null());
    probe.status().expect("run kill").success()
}

fn node(id: &str, path: &str, depends_on: &[&str]) -> Value {
    let name = id.rsplit('.').next().unwrap();
    json!({"unique_id": id, "name": name, "resource_type": id.split('.').next().unwrap(), "package_name": "shop",
           "original_file_path": path, "depends_on": {"nodes": depends_on}, "config": {}})
}

fn item(path: &str, id: &str) -> Value {
    let zero = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}});
    json!({"name": id.rsplit('.').next().unwrap(), "kind": 5, "uri": uri(&fixture().join(path)),
           "range": zero, "selectionRange": zero, "data": {"uniqueId": id}})
}

fn show_messages(session: &Session) -> usize {
    session.received.iter().filter(|m| m["method"] == "window/showMessage").count()
}

fn details(calls: &Value, side: &str) -> Vec<String> {
    let mut found: Vec<String> =
        calls.as_array().unwrap().iter().map(|c| c[side]["detail"].as_str().unwrap().to_owned()).collect();
    found.sort();
    found
}

#[test]
fn answers_the_outline_itself_and_follows_unsaved_edits_in_utf_16() {
    let (mut session, _) = Session::fake("outline", json!({}));
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "final"]);
    assert_eq!(outline["result"][2]["selectionRange"]["start"], json!({"line": 11, "character": 0}));
    // In the buffer only: puts 𝄞, two UTF-16 units and one UTF-32 unit,
    // before the CTE final on its line; renames final to résultat by its
    // position after 𝄞; then edits a line before it.
    let edits = json!({"textDocument": {"uri": uri(&path), "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 11, "character": 0}, "end": {"line": 11, "character": 0}}, "text": "/*𝄞*/ "},
        {"range": {"start": {"line": 11, "character": 7}, "end": {"line": 11, "character": 12}}, "text": "résultat"},
        {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 0}}, "text": "-- é\n"}]});
    session.notify("textDocument/didChange", edits);
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "résultat"]);
    let range = json!({"start": {"line": 12, "character": 7}, "end": {"line": 12, "character": 15}});
    assert_eq!(outline["result"][2]["selectionRange"], range);
    assert!(session.finish().success());
}

#[test]
fn outlines_a_properties_file_from_its_unsaved_text_and_no_other_yaml() {
    let (mut session, _) = Session::fake("yaml-outline", json!({}));
    session.initialize(&fixture());
    let path = fixture().join("models/staging/_sources.yml");
    // Unsaved: 𝄞 is two UTF-16 units, and the description reads as a CTE.
    let text = "models:\n  - name: \"𝄞_orders\"\n    description: with x as (select 1)\n    columns:\n      \
                - name: id\nsources:\n  - name: raw\n    tables:\n      - name: orders\n";
    session.open(&path, Some(text));
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    let model = &outline["result"][0];
    assert_eq!(names(&outline["result"]), ["𝄞_orders", "raw"]);
    assert_eq!((&model["detail"], &model["kind"]), (&json!("model"), &json!(5)));
    assert_eq!(names(&model["children"]), ["id"]);
    assert_eq!(names(&outline["result"][1]["children"]), ["orders"]);
    let name = json!({"start": {"line": 1, "character": 11}, "end": {"line": 1, "character": 20}});
    assert_eq!(model["selectionRange"], name);
    assert_eq!(model["range"]["end"], json!({"line": 4, "character": 16}));
    let project = fixture().join("dbt_project.yml");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&project)}}));
    assert_eq!(outline["result"], json!([]));
    assert!(session.finish().success());
}

#[test]
fn gives_a_flat_outline_to_a_client_without_hierarchy() {
    let (mut session, _) = Session::fake("flat", json!({}));
    session.initialize_with(&fixture(), false);
    let path = fixture().join("snapshots/orders_snapshot.sql");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    let first = &outline["result"][0];
    assert_eq!((first["name"].as_str(), first.get("containerName")), (Some("orders_snapshot"), None));
    assert_eq!(first["location"]["uri"], uri(&path));
    assert!(first.get("children").is_none());
    assert!(session.finish().success());
}

#[test]
fn gives_a_flat_outline_of_a_properties_file_with_containers() {
    let (mut session, _) = Session::fake("flat-yaml", json!({}));
    session.initialize_with(&fixture(), false);
    let path = fixture().join("models/staging/_sources.yml");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    let flat: Vec<(&str, u64, Option<&str>)> = outline["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap(), s["kind"].as_u64().unwrap(), s["containerName"].as_str()))
        .collect();
    let expected = [
        ("raw", 2, None),
        ("orders", 11, Some("raw")),
        ("customers", 11, Some("raw")),
        ("events", 2, None),
        ("page_views", 11, Some("events")),
    ];
    assert_eq!(flat, expected);
    assert!(outline["result"][0].get("children").is_none());
    assert!(session.finish().success());
}

#[test]
fn workspace_symbols_list_every_resource_with_its_package_and_id() {
    let (mut session, _) = Session::fake("workspace-symbols", json!({}));
    session.initialize(&fixture());
    let all = session.request("workspace/symbol", json!({"query": ""}));
    let all = all["result"].as_array().unwrap();
    assert_eq!(all.len(), 22);
    for symbol in all {
        let (container, data) = (&symbol["containerName"], &symbol["data"]);
        let typed = container.as_str().unwrap().starts_with(&format!("{}.", data["resourceType"].as_str().unwrap()));
        assert!(*container == data["uniqueId"] && typed, "{symbol}");
    }
    let found = session.request("workspace/symbol", json!({"query": "ctd"}));
    assert_eq!(names(&found["result"]), ["cents_to_dollars"]);
    assert_eq!(found["result"][0]["containerName"], "macro.shop.cents_to_dollars");
    assert_eq!(found["result"][0]["data"], json!({"uniqueId": "macro.shop.cents_to_dollars", "resourceType": "macro"}));
    assert_eq!(found["result"][0]["location"]["uri"], uri(&fixture().join("macros/money.sql")));
    assert_eq!(found["result"][0]["location"]["range"]["start"], json!({"line": 0, "character": 9}));
    assert!(session.finish().success());
}

#[test]
fn the_server_command_s_project_dir_wins_over_the_client_s_root() {
    let (mut session, _) = Session::fake("root-precedence", json!({}));
    session.initialize(&fixture().join("dbt_packages/shop_utils"));
    let symbols = session.request("workspace/symbol", json!({"query": "orders"}));
    assert!(names(&symbols["result"]).contains(&"orders"), "{symbols}");
    assert!(session.finish().success());
}

#[test]
fn the_client_s_spelling_of_the_root_wins_and_its_paths_match() {
    let link = scratch("root-spelling").join("shop");
    std::os::unix::fs::symlink(fixture(), &link).unwrap();
    let (mut session, _) = Session::fake("root-spelling-session", json!({}));
    session.initialize(&link);
    let symbols = session.request("workspace/symbol", json!({"query": "stg_orders"}));
    assert_eq!(symbols["result"][0]["location"]["uri"], uri(&link.join("models/staging/stg_orders.sql")));
    // A buffer opened through the link, with an unsaved ref, is part of the graph.
    let path = link.join("models/marts/order_summary.sql");
    session.open(&path, Some("select * from {{ ref('stg_customers') }}"));
    let on_select = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 0, "character": 2}});
    let prepared = session.request("textDocument/prepareCallHierarchy", on_select);
    assert_eq!(prepared["result"][0]["data"]["uniqueId"], "model.shop.order_summary");
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": prepared["result"][0]}));
    assert_eq!(details(&outgoing["result"], "to"), ["model.shop.stg_customers"]);
    assert!(session.finish().success());
}

#[test]
fn with_no_dbt_it_serves_the_features_of_the_files() {
    let project = fixture().to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    let result = session.initialize(&fixture());
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus");
    assert_eq!(result["capabilities"]["documentSymbolProvider"], json!(true));
    assert_eq!(result["capabilities"]["textDocumentSync"], json!({"openClose": true, "change": 2, "save": {}}));
    let path = fixture().join("models/marts/orders.sql");
    let position = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 4, "character": 25}});
    // Nothing at the position: its nothing, not a failure, though the dbt cannot run.
    let hover = session.request("textDocument/hover", position);
    assert_eq!((hover.get("error"), &hover["result"], show_messages(&session)), (None, &Value::Null, 0), "{hover}");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "final"]);
    // A document that is no file holds no resource.
    let document = json!({"uri": "untitled:Untitled-1", "languageId": "sql", "version": 1, "text": "select 1"});
    session.notify("textDocument/didOpen", json!({"textDocument": document}));
    let untitled = json!({"textDocument": {"uri": "untitled:Untitled-1"}, "position": {"line": 0, "character": 0}});
    let prepared = session.request("textDocument/prepareCallHierarchy", untitled);
    assert_eq!(prepared.get("result"), Some(&Value::Null), "{prepared}");
    // A method the server does not answer is not found; one assertion with the warnings', as clippy counts.
    let completion = session.request("textDocument/completion", json!({}));
    let message = completion["error"]["message"].as_str().unwrap_or_default();
    let unknown =
        completion["error"]["code"] == -32601 && message == "dbt-lsp-plus does not answer textDocument/completion";
    assert_eq!((show_messages(&session), unknown), (0, true), "no warning: {completion}");
    assert!(session.finish().success());
}

#[test]
fn a_bad_option_is_a_usage_error_and_starts_nothing() {
    let scratch = scratch("bad-option");
    let log = scratch.join("log.jsonl");
    for bad in [&["--mode", "hover=dbt"][..], &["--backend", "nevr"], &["--mode"]] {
        let fake = env!("CARGO_BIN_EXE_fake-dbt");
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_dbt-lsp-plus"))
            .args(bad)
            .args(["--", fake, "lsp"])
            .env_clear()
            .env("FAKE_DBT_LOG", &log)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{bad:?}");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.starts_with("dbt-lsp-plus: ") && said.contains("usage:"), "{said}");
        assert!(!log.exists(), "{bad:?} started the server");
    }
}

#[test]
fn a_dbt_without_lsp_is_served_and_said_once_in_the_log() {
    let (mut session, _) = Session::fake("standalone", json!({}));
    let result = session.initialize(&fixture());
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus");
    let logged = session.wait_for(|m| m["method"] == "window/logMessage");
    let line = format!("dbt-lsp-plus: project {}, dbt {}.", fixture().display(), env!("CARGO_BIN_EXE_fake-dbt"));
    assert_eq!(logged["params"]["message"], json!(line), "{logged}");
    // Past what the client would get first.
    session.request("workspace/symbol", json!({"query": "summary"}));
    assert_eq!(show_messages(&session), 0, "no warning");
    let registered = session.received.iter().any(|m| m["method"] == "client/registerCapability");
    assert!(!registered, "a watcher, for a client that does not let a server register one");
    assert_eq!(session.received.iter().filter(|m| m["method"] == "window/logMessage").count(), 1);
    // A method the server does not answer.
    let completion = session.request("textDocument/completion", json!({}));
    let unknown = json!({"code": -32601, "message": "dbt-lsp-plus does not answer textDocument/completion"});
    assert_eq!((completion.get("result"), &completion["error"]), (None, &unknown), "{completion}");
    assert!(session.finish().success());
}

#[test]
fn an_unknown_request_gets_method_not_found_and_a_rename_at_nothing_null() {
    let unknown = |name: &str| json!({"code": -32601, "message": format!("dbt-lsp-plus does not answer {name}")});
    // Before a project: an unknown method is not found, a method of the server's null.
    let folder = scratch("unknown-before-a-project");
    let mut session = Session::start(&[], &[], Some(&folder));
    session.initialize(&folder);
    let implementation = session.request("textDocument/implementation", json!({}));
    let expected = unknown("textDocument/implementation");
    assert_eq!((implementation.get("result"), &implementation["error"]), (None, &expected), "{implementation}");
    let symbols = session.request("workspace/symbol", json!({"query": "orders"}));
    assert_eq!((symbols.get("error"), &symbols["result"]), (None, &Value::Null), "{symbols}");
    assert!(session.finish().success());
    // With a project: an unknown method, a command gone, a command's name sent as a method.
    let (mut session, _) = Session::fake("unknown-with-a-project", json!({}));
    let result = session.initialize(&fixture());
    let commands = json!(["dbt-lsp-plus.nodeInfo", "dbt-lsp-plus.compile", "dbt-lsp-plus.nodes"]);
    assert_eq!(result["capabilities"]["executeCommandProvider"]["commands"], commands, "{result}");
    let at_select = position_in(&fixture().join("models/marts/orders.sql"), "select", 1);
    let show = json!({"command": "dbt-lsp-plus.show", "arguments": [{"inline": "select 1"}]});
    let unknowns = [
        ("textDocument/completion", at_select.clone(), "textDocument/completion"),
        ("workspace/executeCommand", show, "dbt-lsp-plus.show"),
        ("dbt-lsp-plus.nodeInfo", json!({"arguments": ["model.shop.orders"]}), "dbt-lsp-plus.nodeInfo"),
    ];
    for (method, params, named) in unknowns {
        let answer = session.request(method, params);
        assert_eq!((answer.get("result"), &answer["error"]), (None, &unknown(named)), "{method}: {answer}");
    }
    // A rename at no source and no CTE: nothing to rename.
    let mut rename = at_select;
    rename["newName"] = json!("x");
    let nothing = session.request("textDocument/rename", rename);
    assert_eq!((nothing.get("error"), &nothing["result"]), (None, &Value::Null), "{nothing}");
    assert!(session.finish().success());
}

#[test]
fn a_command_with_no_name_is_invalid_and_the_server_s_notification_asked_is_unknown() {
    let (mut session, _) = Session::fake("invalid-with-a-project", json!({}));
    session.initialize(&fixture());
    let nameless = session.request("workspace/executeCommand", json!({"arguments": []}));
    let invalid = json!({"code": -32602, "message": "invalid parameters"});
    assert_eq!((nameless.get("result"), &nameless["error"]), (None, &invalid), "{nameless}");
    // The server's notification, sent as a request: no method of the server's.
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let published = session.request("textDocument/publishDiagnostics", json!({"uri": orders, "diagnostics": []}));
    let unknown = json!({"code": -32601, "message": "dbt-lsp-plus does not answer textDocument/publishDiagnostics"});
    assert_eq!((published.get("result"), &published["error"]), (None, &unknown), "{published}");
    assert!(session.finish().success());
}

#[test]
fn a_command_named_as_a_method_is_unknown() {
    let (mut session, _) = Session::fake("command-as-method", json!({}));
    session.initialize(&fixture());
    let unknown = |name: &str| json!({"code": -32601, "message": format!("dbt-lsp-plus does not answer {name}")});
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let asked = [
        ("dbt-lsp-plus.compile", json!({"arguments": [orders]}), "dbt-lsp-plus.compile"),
        ("workspace/executeCommand", json!({"command": "textDocument/hover", "arguments": []}), "textDocument/hover"),
    ];
    for (method, params, named) in asked {
        let answer = session.request(method, params);
        assert_eq!((answer.get("result"), &answer["error"]), (None, &unknown(named)), "{method}: {answer}");
    }
    assert!(session.finish().success());
}

#[test]
fn initialization_options_are_not_read() {
    let (mut session, _) = Session::fake("initialization-options", json!({}));
    let root = uri(&fixture());
    let params = json!({"processId": null, "rootUri": root, "capabilities": {},
                        "workspaceFolders": [{"uri": root, "name": "shop"}],
                        "initializationOptions": {"dbtLspPlus": {"defaultMode": "dbt"}}});
    let result = session.request("initialize", params)["result"].clone();
    session.notify("initialized", json!({}));
    assert_eq!(result["capabilities"]["documentSymbolProvider"], json!(true), "{result}");
    let path = fixture().join("models/marts/orders.sql");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "final"]);
    assert_eq!(show_messages(&session), 0);
    assert!(session.finish().success());
}

#[test]
fn outline_symbols_folding_and_calls_answer_from_the_files() {
    let (mut session, _) = Session::fake("standalone-features", json!({}));
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "final"]);
    let symbols = session.request("workspace/symbol", json!({"query": "summary"}));
    assert_eq!(names(&symbols["result"]), ["order_summary"]);
    let folding = session.request("textDocument/foldingRange", json!({"textDocument": {"uri": uri(&path)}}));
    assert!(!folding["result"].as_array().unwrap().is_empty(), "{folding}");
    let on_with = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 3, "character": 1}});
    let prepared = session.request("textDocument/prepareCallHierarchy", on_with);
    assert_eq!(prepared["result"][0]["data"]["uniqueId"], "model.shop.orders");
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": prepared["result"][0]}));
    let to = ["macro.shop_utils.is_positive", "model.shop.stg_customers", "model.shop.stg_orders"];
    assert_eq!(details(&outgoing["result"], "to"), to);
    assert!(session.finish().success());
}

#[test]
fn a_request_the_proxy_answers_without_its_parameters_is_invalid() {
    let (mut session, _) = Session::fake("standalone-invalid", json!({}));
    session.initialize(&fixture());
    let invalid = json!({"code": -32602, "message": "invalid parameters"});
    for method in ["workspace/symbol", "textDocument/documentSymbol", "textDocument/foldingRange"] {
        let answer = session.request(method, json!({}));
        assert_eq!((answer.get("result"), &answer["error"]), (None, &invalid), "{method}: {answer}");
    }
    assert!(session.finish().success());
}

#[test]
fn the_graph_takes_dbt_s_resolution_from_the_manifest() {
    let mut manifest = manifest();
    let summary_node = node("model.shop.order_summary", "models/marts/order_summary.sql", &["model.shop.orders"]);
    manifest["nodes"]["model.shop.order_summary"] = summary_node;
    manifest["nodes"]["model.shop.orders"]["depends_on"] = json!({"nodes": ["model.shop.stg_orders"]});
    let gate = scratch("standalone-graph-gate").join("open");
    let script = json!({"manifest": manifest, "parse_gate": gate});
    let (mut session, log) = Session::fake("standalone-graph", script);
    session.initialize(&fixture());
    let summary = fixture().join("models/marts/order_summary.sql");
    let on_select = json!({"textDocument": {"uri": uri(&summary)}, "position": {"line": 1, "character": 2}});
    // The hierarchy asks for the parse that resolves its graph, and the
    // scan, which cannot resolve `ref(var(...))`, answers meanwhile.
    let prepared = session.request("textDocument/prepareCallHierarchy", on_select);
    let summary_item = json!({"item": prepared["result"][0]});
    assert_eq!(session.request("callHierarchy/outgoingCalls", summary_item.clone())["result"], json!([]));
    until_logged(&log, "the parse", |_| parses(&log).len() == 1);
    std::fs::write(&gate, "").unwrap();
    let found = |calls: &Value| calls.as_array().is_some_and(|calls| !calls.is_empty());
    let outgoing = session.poll(Duration::from_secs(10), ("callHierarchy/outgoingCalls", summary_item), found);
    assert_eq!(details(&outgoing, "to"), ["model.shop.orders"]);
    // The edges of macros stay the scan's.
    let orders = json!({"item": item("models/marts/orders.sql", "model.shop.orders")});
    let outgoing = session.request("callHierarchy/outgoingCalls", orders);
    assert_eq!(details(&outgoing["result"], "to"), ["macro.shop_utils.is_positive", "model.shop.stg_orders"]);
    assert!(session.finish().success());
}

#[test]
fn each_request_of_the_call_hierarchy_asks_for_a_parse() {
    let orders = json!({"item": item("models/marts/orders.sql", "model.shop.orders")});
    let path = fixture().join("models/marts/orders.sql");
    let on_with = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 3, "character": 1}});
    // The graph is the last parse's: a follower asks for one, with no document opened.
    let (mut session, log) = Session::fake("hierarchy-parse-alone", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    session.request("callHierarchy/outgoingCalls", orders.clone());
    until_logged(&log, "the parse", |_| parses(&log).len() == 1);
    assert!(session.finish().success());
    // So do its two other requests, each the first of a session of its own.
    let others = [
        ("prepare", "textDocument/prepareCallHierarchy", &on_with),
        ("incoming", "callHierarchy/incomingCalls", &orders),
    ];
    for (name, method, params) in others {
        let script = json!({"manifest": manifest()});
        let (mut session, log) = Session::fake(&format!("hierarchy-parse-alone-{name}"), script);
        session.initialize(&fixture());
        session.request(method, params.clone());
        until_logged(&log, &format!("the parse {method} asks for"), |_| parses(&log).len() == 1);
        assert!(session.finish().success());
    }
}

#[test]
fn the_server_watches_the_project_for_a_client_that_lets_it() {
    let (mut session, log) = Session::fake("standalone-watch", json!({"manifest": manifest()}));
    session.initialize_as(&fixture(), json!({"workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}}));
    let registered = session.wait_for(|m| m["method"] == "client/registerCapability");
    let watcher = json!({"id": "dbt-lsp-plus-file-watcher", "method": "workspace/didChangeWatchedFiles",
                         "registerOptions": {"watchers": [{"globPattern": "**/*"}]}});
    assert_eq!(registered["params"]["registrations"], json!([watcher]));
    let event = |path: &str| json!({"changes": [{"uri": uri(&fixture().join(path)), "type": 2}]});
    // dbt writes its own output at each parse: no parse for it.
    session.notify("workspace/didChangeWatchedFiles", event("target/manifest.json"));
    parses_stay(&log, 0);
    session.notify("workspace/didChangeWatchedFiles", event("models/marts/orders.sql"));
    until_logged(&log, "the parse", |_| parses(&log).len() == 1);
    assert!(session.finish().success());
}

#[test]
fn exit_without_shutdown_fails() {
    let (mut session, _) = Session::fake("exit-early", json!({}));
    session.initialize(&fixture());
    session.notify("exit", Value::Null);
    assert_eq!(session.wait().code(), Some(1));
}

/// The position of `page_views` in `stg_page_views.sql`, with that file's URI.
fn on_page_views() -> Value {
    let path = fixture().join("models/staging/stg_page_views.sql");
    json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 0, "character": 37}})
}

#[test]
fn answers_references_on_a_source_and_on_a_ref() {
    let (mut session, _) = Session::fake("source-references", json!({}));
    session.initialize(&fixture());
    let mut params = on_page_views();
    params["context"] = json!({"includeDeclaration": true});
    let found = session.request("textDocument/references", params);
    let mut files: Vec<&str> = found["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["uri"].as_str().unwrap().rsplit('/').next().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, ["_sources.yml", "stg_page_views.sql"]);
    // On a ref, the references too.
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}, "context": {"includeDeclaration": true}});
    let found = session.request("textDocument/references", on_ref);
    assert_eq!(found["result"].as_array().map(Vec::len), Some(3), "with its declaration: {found}");
    assert!(session.finish().success());
}

#[test]
fn renames_a_source_table_and_keeps_its_relation() {
    let (mut session, _) = Session::fake("source-rename", json!({}));
    session.initialize(&fixture());
    let mut params = on_page_views();
    params["newName"] = json!("visits");
    let edit = session.request("textDocument/rename", params);
    let changes = edit["result"]["changes"].as_object().unwrap();
    let model = &changes[&uri(&fixture().join("models/staging/stg_page_views.sql"))];
    assert_eq!(
        model,
        &json!([{"range": {"start": {"line": 0, "character": 35}, "end": {"line": 0, "character": 45}}, "newText": "visits"}])
    );
    let mut yaml: Vec<&str> = changes[&uri(&fixture().join("models/staging/_sources.yml"))]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["newText"].as_str().unwrap())
        .collect();
    // LSP leaves the order of edits at different places open.
    yaml.sort();
    assert_eq!(yaml, ["\n        identifier: page_views", "visits"]);
    let mut refused = on_page_views();
    refused["newName"] = json!("it's");
    let error = session.request("textDocument/rename", refused);
    assert_eq!(error["error"]["code"], json!(-32803));
    assert!(error["error"]["message"].as_str().unwrap().contains("cannot hold"), "{error}");
    assert!(session.finish().success());
}

#[test]
fn with_no_dbt_it_still_answers_on_sources() {
    let project = fixture().to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    let result = session.initialize(&fixture());
    assert_eq!(result["capabilities"]["referencesProvider"], json!(true));
    assert_eq!(result["capabilities"]["renameProvider"], json!(true));
    let mut params = on_page_views();
    params["context"] = json!({"includeDeclaration": false});
    let found = session.request("textDocument/references", params);
    assert_eq!(found["result"].as_array().unwrap().len(), 1);
    let mut params = on_page_views();
    params["newName"] = json!("visits");
    let edit = session.request("textDocument/rename", params);
    assert!(edit["result"]["changes"].as_object().is_some_and(|changes| changes.len() == 2), "{edit}");
    assert!(session.finish().success());
}

#[test]
fn a_closed_document_s_unsaved_edits_leave_the_graph() {
    let (mut session, _) = Session::fake("closed", json!({}));
    session.initialize(&fixture());
    let path = fixture().join("models/marts/order_summary.sql");
    session.open(&path, Some("select * from {{ ref('stg_customers') }}"));
    let summary = item("models/marts/order_summary.sql", "model.shop.order_summary");
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": summary}));
    assert_eq!(details(&outgoing["result"], "to"), ["model.shop.stg_customers"]);
    session.notify("textDocument/didClose", json!({"textDocument": {"uri": uri(&path)}}));
    // On disk, order_summary refs a var: no edge the scan can tell.
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": summary}));
    assert_eq!(outgoing["result"], json!([]));
    assert!(session.finish().success());
}

/// A copy of the fixture for test NAME, and a session on it with the fake
/// dbt; the client watches no file, so that nothing tells the server of a
/// change on disk.
fn on_a_copy(name: &str) -> (PathBuf, Session) {
    let project = copy_fixture(&format!("{name}-project"));
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with(name, json!({}), &flags, None);
    session.initialize(&project);
    (project, session)
}

/// What the node ID calls, as the proxy's call hierarchy says.
fn callees(session: &mut Session, id: &str) -> Vec<String> {
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": {"data": {"uniqueId": id}}}));
    details(&outgoing["result"], "to")
}

/// The proxy's workspace symbols for QUERY.
fn symbols(session: &mut Session, query: &str) -> Value {
    session.request("workspace/symbol", json!({"query": query}))["result"].clone()
}

#[test]
fn an_edit_on_disk_that_no_one_announced_is_read_at_the_next_request() {
    let (project, mut session) = on_a_copy("edit-on-disk");
    assert!(callees(&mut session, "model.shop.order_summary").is_empty());
    let summary = project.join("models/marts/order_summary.sql");
    std::fs::write(summary, "select * from {{ ref('stg_customers') }}").unwrap();
    assert_eq!(callees(&mut session, "model.shop.order_summary"), ["model.shop.stg_customers"]);
    assert!(session.finish().success());
}

#[test]
fn an_open_document_is_read_again_at_each_change_and_each_open() {
    let (project, mut session) = on_a_copy("open-document");
    let path = project.join("models/marts/order_summary.sql");
    let (customers, orders) = ("select * from {{ ref('stg_customers') }}", "select * from {{ ref('stg_orders') }}");
    session.open(&path, Some(customers));
    assert_eq!(callees(&mut session, "model.shop.order_summary"), ["model.shop.stg_customers"]);
    let change = json!({"textDocument": {"uri": uri(&path), "version": 2}, "contentChanges": [{"text": orders}]});
    session.notify("textDocument/didChange", change);
    assert_eq!(callees(&mut session, "model.shop.order_summary"), ["model.shop.stg_orders"]);
    // Closed unsaved: the file on disk, whose upstream is a variable.
    session.notify("textDocument/didClose", json!({"textDocument": {"uri": uri(&path)}}));
    assert!(callees(&mut session, "model.shop.order_summary").is_empty());
    // Each open starts again at version 1, whatever the text.
    session.open(&path, Some(customers));
    assert_eq!(callees(&mut session, "model.shop.order_summary"), ["model.shop.stg_customers"]);
    session.notify("textDocument/didClose", json!({"textDocument": {"uri": uri(&path)}}));
    session.open(&path, Some(orders));
    assert_eq!(callees(&mut session, "model.shop.order_summary"), ["model.shop.stg_orders"]);
    assert!(session.finish().success());
}

#[test]
fn a_file_created_then_deleted_comes_and_goes() {
    let (project, mut session) = on_a_copy("created-deleted");
    assert_eq!(symbols(&mut session, "brand_new"), json!([]));
    let path = project.join("models/marts/brand_new.sql");
    std::fs::write(&path, "select 1").unwrap();
    let found = symbols(&mut session, "brand_new");
    assert_eq!(names(&found), ["brand_new"]);
    assert_eq!(found[0]["location"]["uri"], uri(&path));
    std::fs::remove_file(&path).unwrap();
    assert_eq!(symbols(&mut session, "brand_new"), json!([]));
    assert!(session.finish().success());
}

#[test]
fn the_project_s_name_and_model_paths_are_read_again() {
    let (project, mut session) = on_a_copy("project-file");
    assert_eq!(symbols(&mut session, "stg_orders")[0]["data"]["uniqueId"], "model.shop.stg_orders");
    let file = project.join("dbt_project.yml");
    let config = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, config.replace("name: 'shop'", "name: 'store'")).unwrap();
    assert_eq!(symbols(&mut session, "stg_orders")[0]["data"]["uniqueId"], "model.store.stg_orders");
    std::fs::write(&file, config.replace(r#"model-paths: ["models"]"#, r#"model-paths: ["models/marts"]"#)).unwrap();
    assert_eq!(symbols(&mut session, "stg_orders"), json!([]), "outside the model paths");
    assert_eq!(symbols(&mut session, "order_summary")[0]["data"]["uniqueId"], "model.shop.order_summary");
    assert!(session.finish().success());
}

#[test]
fn an_open_project_file_s_name_is_read_again_once_saved() {
    let (project, mut session) = on_a_copy("open-project-file");
    let file = project.join("dbt_project.yml");
    session.open(&file, None);
    let renamed = std::fs::read_to_string(&file).unwrap().replace("name: 'shop'", "name: 'store'");
    let change = json!({"textDocument": {"uri": uri(&file), "version": 2}, "contentChanges": [{"text": renamed}]});
    session.notify("textDocument/didChange", change);
    // A request between the edit and the save: the open document's key no
    // longer changes, though the name on disk does.
    symbols(&mut session, "stg_orders");
    std::fs::write(&file, &renamed).unwrap();
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&file)}}));
    assert_eq!(symbols(&mut session, "stg_orders")[0]["data"]["uniqueId"], "model.store.stg_orders");
    assert!(session.finish().success());
}

#[test]
fn a_package_unpacked_again_with_the_same_sizes_and_times_is_read_again() {
    let (project, mut session) = on_a_copy("package-replaced");
    assert_eq!(names(&symbols(&mut session, "is_positive")), ["is_positive"]);
    // As an archive unpacks a new version: a new file, its size and time kept.
    let path = project.join("dbt_packages/shop_utils/macros/is_positive.sql");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let unpacked = project.with_file_name("unpacked.sql");
    std::fs::write(&unpacked, std::fs::read_to_string(&path).unwrap().replace("is_positive", "is_negative")).unwrap();
    std::fs::File::options().write(true).open(&unpacked).unwrap().set_modified(modified).unwrap();
    std::fs::rename(&unpacked, &path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
    assert_eq!(names(&symbols(&mut session, "is_negative")), ["is_negative"]);
    assert_eq!(symbols(&mut session, "is_positive"), json!([]));
    assert!(session.finish().success());
}

#[test]
fn a_document_under_a_link_inside_the_project_is_found_where_it_was_opened() {
    let project = copy_fixture("link-inside");
    std::fs::create_dir(project.join("elsewhere")).unwrap();
    std::fs::write(project.join("elsewhere/linked.sql"), "select * from {{ ref('stg_orders') }}").unwrap();
    std::os::unix::fs::symlink("../elsewhere", project.join("models/link")).unwrap();
    let (mut session, _) =
        Session::fake_with("link-inside-session", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let path = project.join("models/link/linked.sql");
    let symbols = session.request("workspace/symbol", json!({"query": "linked"}));
    assert_eq!(symbols["result"][0]["location"]["uri"], uri(&path));
    // An unsaved first line moves the ref to line 1.
    session.open(&path, Some("-- unsaved\nselect * from {{ ref('stg_orders') }}"));
    let on_ref = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 1, "character": 25}});
    let prepared = session.request("textDocument/prepareCallHierarchy", on_ref);
    assert_eq!(prepared["result"][0]["data"]["uniqueId"], "model.shop.stg_orders", "{prepared}");
    assert!(session.finish().success());
}

#[test]
fn a_document_opened_through_another_spelling_maps_onto_the_root() {
    let link = scratch("other-spelling").join("shop");
    std::os::unix::fs::symlink(fixture(), &link).unwrap();
    let (mut session, _) = Session::fake("other-spelling-session", json!({}));
    session.initialize(&fixture());
    let path = link.join("models/marts/order_summary.sql");
    session.open(&path, Some("select * from {{ ref('stg_customers') }}"));
    let on_select = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 0, "character": 2}});
    let prepared = session.request("textDocument/prepareCallHierarchy", on_select);
    assert_eq!(prepared["result"][0]["data"]["uniqueId"], "model.shop.order_summary", "{prepared}");
    assert_eq!(prepared["result"][0]["uri"], uri(&fixture().join("models/marts/order_summary.sql")));
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": prepared["result"][0]}));
    assert_eq!(details(&outgoing["result"], "to"), ["model.shop.stg_customers"]);
    assert!(session.finish().success());
}

#[test]
fn without_project_dir_the_client_s_folder_is_the_root() {
    let elsewhere = scratch("no-project-dir");
    let (mut session, _) = Session::fake_with("no-project-dir-session", json!({}), &[], Some(&elsewhere));
    session.initialize(&fixture());
    let all = session.request("workspace/symbol", json!({"query": ""}));
    assert_eq!(all["result"].as_array().unwrap().len(), 22);
    assert!(session.finish().success());
}

#[test]
fn without_project_dir_nor_a_project_folder_the_working_directory_names_nothing() {
    let (mut session, _) = Session::fake_with("working-directory", json!({}), &[], Some(&fixture()));
    session.initialize(&fixture().join("models"));
    let found = session.request("workspace/symbol", json!({"query": "stg_orders"}));
    assert_eq!(found["result"], Value::Null, "{found}");
    assert!(session.finish().success());
}

#[test]
fn the_project_dir_keeps_its_spelling_when_the_client_names_a_folder_above_it() {
    let link = scratch("spelt-project-dir").join("repo");
    std::os::unix::fs::symlink(fixture().parent().unwrap(), &link).unwrap();
    let project = link.join("shop");
    let (mut session, _) =
        Session::fake_with("spelt-session", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&link);
    let found = session.request("workspace/symbol", json!({"query": "stg_orders"}));
    assert_eq!(found["result"][0]["location"]["uri"], uri(&project.join("models/staging/stg_orders.sql")));
    assert!(session.finish().success());
}

#[test]
fn a_source_rename_on_a_file_the_scan_does_not_hold_is_null_but_not_its_references() {
    let project = copy_fixture("source-unscanned");
    let call = "select * from {{ source('events', 'page_views') }}";
    let outside = project.join("integration_tests/i.sql");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, call).unwrap();
    let (mut session, _) =
        Session::fake_with("source-unscanned-session", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let unsaved = project.join("models/new_model.sql");
    session.open(&unsaved, Some(call));
    for path in [&unsaved, &outside] {
        let at = json!({"textDocument": {"uri": uri(path)}, "position": {"line": 0, "character": 37}, "newName": "v"});
        let answer = session.request("textDocument/rename", at.clone());
        assert_eq!((answer.get("error"), &answer["result"]), (None, &Value::Null), "{answer}");
        // References come from the scan whichever file of the project asks.
        let answer = session.request("textDocument/references", at);
        let page_views = uri(&project.join("models/staging/stg_page_views.sql"));
        assert!(
            answer["result"].as_array().is_some_and(|found| found.iter().any(|l| l["uri"] == page_views)),
            "{answer}"
        );
    }
    assert!(session.finish().success());
}

#[test]
fn a_source_request_counts_no_byte_order_mark() {
    let project = copy_fixture("source-bom");
    let model = project.join("models/staging/stg_page_views.sql");
    let text = std::fs::read_to_string(&model).unwrap();
    std::fs::write(&model, format!("\u{feff}{text}")).unwrap();
    let (mut session, _) =
        Session::fake_with("source-bom-session", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    // The first letter of page_views, as an editor that leaves the mark out counts it.
    let at = json!({"textDocument": {"uri": uri(&model)}, "position": {"line": 0, "character": 35},
                    "context": {"includeDeclaration": false}});
    let found = session.request("textDocument/references", at);
    assert_eq!(found["result"].as_array().map(Vec::len), Some(1), "{found}");
    assert!(session.finish().success());
}

#[test]
fn a_rename_names_an_open_document_by_the_uri_it_was_opened_under() {
    let project = copy_fixture("rename-spelling");
    let link = project.with_file_name("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    let (mut session, _) =
        Session::fake_with("rename-spelling-session", json!({}), &[Path::new("--project-dir"), &link], None);
    session.initialize(&link);
    // Opened under the resolved spelling, with an unsaved first line.
    let model = project.join("models/staging/stg_page_views.sql");
    let text = format!("-- unsaved\n{}", std::fs::read_to_string(&model).unwrap());
    session.open(&model, Some(&text));
    let at =
        json!({"textDocument": {"uri": uri(&model)}, "position": {"line": 1, "character": 37}, "newName": "visits"});
    let edit = session.request("textDocument/rename", at);
    let changes = edit["result"]["changes"].as_object().unwrap_or_else(|| panic!("no edit: {edit}"));
    let range = json!({"start": {"line": 1, "character": 35}, "end": {"line": 1, "character": 45}});
    assert_eq!(changes[&uri(&model)], json!([{"range": range, "newText": "visits"}]));
    assert!(!changes.contains_key(&uri(&link.join("models/staging/stg_page_views.sql"))));
    assert!(changes.contains_key(&uri(&link.join("models/staging/_sources.yml"))));
    assert!(session.finish().success());
}

/// The position of `orders` in `from orders` in `orders.sql`, with that file's URI.
fn on_from_orders() -> Value {
    let path = fixture().join("models/marts/orders.sql");
    json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 17, "character": 11}})
}

#[test]
fn folds_highlights_and_selects_within_a_document() {
    let (mut session, _) = Session::fake("document-features", json!({}));
    session.initialize(&fixture());
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let folding = session.request("textDocument/foldingRange", json!({"textDocument": {"uri": orders}}));
    let lines: Vec<(u64, u64)> = folding["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["startLine"].as_u64().unwrap(), r["endLine"].as_u64().unwrap()))
        .collect();
    assert_eq!(lines, [(0, 1), (3, 4), (7, 8), (11, 18)]);
    let highlights = session.request("textDocument/documentHighlight", on_from_orders());
    let kinds: Vec<(u64, u64)> = highlights["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["range"]["start"]["line"].as_u64().unwrap(), h["kind"].as_u64().unwrap()))
        .collect();
    assert_eq!(kinds, [(3, 3), (13, 2), (15, 2), (17, 2)]);
    let at = json!({"textDocument": {"uri": orders}, "positions": [{"line": 17, "character": 11}]});
    let selection = session.request("textDocument/selectionRange", at);
    assert_eq!(
        selection["result"][0]["range"],
        json!({"start": {"line": 17, "character": 9}, "end": {"line": 17, "character": 15}})
    );
    assert_eq!(selection["result"][0]["parent"]["parent"]["range"]["start"], json!({"line": 11, "character": 0}));
    assert!(session.finish().success());
}

#[test]
fn renames_a_cte_and_gives_null_at_a_ref() {
    let (mut session, _) = Session::fake("cte-rename", json!({}));
    session.initialize(&fixture());
    let orders_sql = uri(&fixture().join("models/marts/orders.sql"));
    let on_customers =
        json!({"textDocument": {"uri": orders_sql}, "position": {"line": 18, "character": 14}, "newName": "c"});
    let edit = session.request("textDocument/rename", on_customers.clone());
    let edits = edit["result"]["changes"][&orders_sql].as_array().unwrap_or_else(|| panic!("no edit: {edit}")).clone();
    let lines: Vec<u64> = edits.iter().map(|e| e["range"]["start"]["line"].as_u64().unwrap()).collect();
    assert_eq!(lines, [7, 14, 18]);
    assert!(edits.iter().all(|e| e["newText"] == "c"));
    let mut clash = on_customers;
    clash["newName"] = json!("final");
    assert_eq!(session.request("textDocument/rename", clash)["error"]["code"], json!(-32803));
    // The Jinja string 'orders.amount', which is_positive renders as SQL, names the CTE orders.
    let mut in_jinja = on_from_orders();
    in_jinja["newName"] = json!("o");
    let refused = session.request("textDocument/rename", in_jinja);
    assert!(refused["error"]["message"].as_str().unwrap_or_default().contains("Jinja string"), "{refused}");
    // A rename at no source and no CTE has nothing to rename.
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}, "newName": "x"});
    let nothing = session.request("textDocument/rename", on_ref);
    assert_eq!((nothing.get("error"), &nothing["result"]), (None, &Value::Null), "{nothing}");
    assert!(session.finish().success());
}

#[test]
fn leaves_the_folding_and_selection_of_yaml_to_the_client() {
    let (mut session, _) = Session::fake("yaml-folding", json!({}));
    session.initialize(&fixture());
    let yaml = uri(&fixture().join("models/staging/_sources.yml"));
    let folding = session.request("textDocument/foldingRange", json!({"textDocument": {"uri": yaml}}));
    assert_eq!(folding.get("result"), Some(&Value::Null), "{folding}");
    let at = json!({"textDocument": {"uri": yaml}, "positions": [{"line": 2, "character": 4}]});
    let selection = session.request("textDocument/selectionRange", at);
    assert_eq!(selection.get("result"), Some(&Value::Null), "{selection}");
    assert!(session.finish().success());
}

#[test]
fn a_cte_in_yaml_is_renamed_as_in_sql_and_not_highlighted() {
    let (mut session, _) = Session::fake("yaml-cte", json!({}));
    session.initialize(&fixture());
    let path = fixture().join("models/staging/notes.yml");
    session.open(&path, Some("query: with x as (select 1) select * from x\n"));
    let at = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 0, "character": 12}, "newName": "y"});
    let renamed = session.request("textDocument/rename", at.clone());
    let edit = |start: u64| {
        json!({"range": {"start": {"line": 0, "character": start}, "end": {"line": 0, "character": start + 1}},
               "newText": "y"})
    };
    assert_eq!(renamed["result"]["changes"][uri(&path)], json!([edit(12), edit(42)]), "{renamed}");
    let highlight = session.request("textDocument/documentHighlight", at);
    assert_eq!(highlight.get("result"), Some(&Value::Null), "{highlight}");
    assert!(session.finish().success());
}

#[test]
fn with_no_dbt_the_features_within_a_document_remain() {
    let project = fixture().to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    let folding = session.request("textDocument/foldingRange", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(folding["result"].as_array().map(Vec::len), Some(4), "{folding}");
    let on_customers =
        json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 18, "character": 14}, "newName": "c"});
    let renamed = session.request("textDocument/rename", on_customers);
    assert_eq!(renamed["result"]["changes"][uri(&path)].as_array().map(Vec::len), Some(3), "{renamed}");
    assert!(session.finish().success());
}

/// A manifest of the fixture as `dbt parse` writes it, in part.
fn manifest() -> Value {
    let model = |name: &str, path: &str, materialized: &str| {
        json!({"unique_id": format!("model.shop.{name}"), "name": name, "resource_type": "model",
               "package_name": "shop", "original_file_path": path, "access": "protected",
               "relation_name": format!("\"shop\".\"main\".\"{name}\""), "config": {"materialized": materialized}})
    };
    let customers = json!({"unique_id": "source.shop.raw.customers", "name": "customers", "source_name": "raw",
                           "resource_type": "source", "package_name": "shop", "relation_name": "raw_customers"});
    json!({"metadata": {"project_name": "shop"}, "nodes": {
        "model.shop.orders": model("orders", "models/marts/orders.sql", "table"),
        "model.shop.stg_orders": model("stg_orders", "models/staging/stg_orders.sql", "view")},
        "sources": {"source.shop.raw.customers": customers}})
}

/// A run holds the lock of the test runs from its first scratch directory
/// on, so that a second run in this checkout waits.  A lock belongs to one
/// opening of the file: a second opening in this process is refused too.
#[test]
fn scratch_holds_the_turn() {
    scratch("turn");
    let lock = std::fs::File::open(Path::new(env!("CARGO_TARGET_TMPDIR")).join("turn.lock")).expect("open the lock");
    assert!(matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "the lock is free");
}

/// The scratch directory of the fake session NAME, as it stands.
fn scratch_of(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Asserts that the fake has logged COUNT parses, and no more for half a
/// second: a parse wrongly started takes a moment to log.
fn parses_stay(log: &Path, count: usize) {
    let end = Instant::now() + Duration::from_millis(500);
    while Instant::now() < end {
        assert_eq!(parses(log).len(), count);
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The answer to `dbt-lsp-plus.nodeInfo` with ARGUMENT.
fn node_info(session: &mut Session, argument: Value) -> Value {
    session.request("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": [argument]}))
}

/// What the fake logged of each parse, beside LOG: its arguments and pid.
fn parses(log: &Path) -> Vec<Value> {
    logged(&log.with_extension("jsonl.parses"))
}

/// Opens orders.sql, which starts a parse, then asks for the node orders
/// until a parse has given it; returns its information.
fn parsed_orders(session: &mut Session) -> Value {
    session.open(&fixture().join("models/marts/orders.sql"), None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object())
}

#[test]
fn node_info_by_id_needs_no_document_opened() {
    let (mut session, _) = Session::fake("node-info-alone", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    // The request starts the parse, as the first document would.
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    let orders = session.poll(Duration::from_secs(10), request, |result| result.is_object());
    assert_eq!(orders["unique_id"], "model.shop.orders");
    assert!(session.finish().success());
}

/// The files named NAME under DIR.
fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        for path in std::fs::read_dir(&next).into_iter().flatten().flatten().map(|entry| entry.path()) {
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|file| file == name) {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn node_info_parses_with_the_server_s_dbt_whose_output_never_reaches_the_client() {
    let (mut session, log) = Session::fake("node-info", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    let orders = parsed_orders(&mut session);
    assert_eq!(orders["relation"], "\"shop\".\"main\".\"orders\"");
    assert_eq!(orders["config"]["materialized"], "table");
    // dbt's noise never reached the client; its stderr is in the cache's log.
    assert!(session.received.iter().all(|m| !m.is_null()), "a body that is not JSON");
    let logs = files_named(&scratch_of("node-info").join("cache"), "parse.log");
    assert!(std::fs::read_to_string(&logs[0]).unwrap().contains("noise on stderr"), "{logs:?}");
    // An index with no change since needs no parse.
    node_info(&mut session, json!("model.shop.orders"));
    node_info(&mut session, json!("model.shop.stg_orders"));
    parses_stay(&log, 1);
    assert!(session.finish().success());
    assert!(files_named(&scratch_of("node-info").join("cache"), "parse.log").is_empty(), "the cache stayed");
}

#[test]
fn a_parse_takes_the_server_s_reading_flags_and_keeps_its_output_in_the_cache() {
    let flags = [
        Path::new("--project-dir"),
        &fixture(),
        Path::new("--target"),
        Path::new("dev"),
        Path::new("--log-level"),
        Path::new("info"),
    ];
    let (mut session, log) = Session::fake_with("parse-flags", json!({"manifest": manifest()}), &flags, None);
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let argv: Vec<String> = serde_json::from_value(parses(&log)[0]["parse"].clone()).unwrap();
    let at = |flag: &str| argv.iter().position(|a| a == flag).map(|i| argv[i + 1].clone());
    assert_eq!(at("--project-dir").as_deref(), Some(fixture().to_str().unwrap()));
    assert_eq!(at("--target").as_deref(), Some("dev"));
    assert!(at("--target-path").unwrap().contains("parse-flags/cache/dbt-lsp-plus/"), "{argv:?}");
    assert!(!argv.contains(&"--log-level".to_owned()) && argv.contains(&"--no-send-anonymous-usage-stats".to_owned()));
    assert!(session.finish().success());
}

#[test]
fn node_info_gives_the_summary_a_hover_shows() {
    let (mut session, _) = Session::fake("node-info-summary", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    let orders = parsed_orders(&mut session);
    assert_eq!(orders["summary"], "model table, \"shop\".\"main\".\"orders\"");
    let file = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"line": 4, "character": 28});
    let info = node_info(&mut session, json!({"uri": file, "position": on_ref}));
    let hover = session.request("textDocument/hover", json!({"textDocument": {"uri": file}, "position": on_ref}));
    let summary = info["result"]["summary"].as_str().unwrap();
    assert_eq!(hover["result"]["contents"]["value"], format!("{summary}\n`model.shop.stg_orders`"), "{hover}");
    assert!(session.finish().success());
}

#[test]
fn node_info_names_a_node_by_id_or_by_position() {
    let (mut session, _) = Session::fake("node-info-at", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    assert_eq!(node_info(&mut session, json!("model.shop.nowhere"))["result"], Value::Null);
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = node_info(&mut session, json!({"uri": orders, "position": {"line": 4, "character": 28}}));
    assert_eq!(on_ref["result"]["unique_id"], "model.shop.stg_orders");
    let elsewhere = node_info(&mut session, json!({"uri": orders, "position": {"line": 0, "character": 0}}));
    assert_eq!(elsewhere["result"]["unique_id"], "model.shop.orders");
    // A relationships test calls ref outside Jinja.
    let yaml = fixture().join("models/marts/_notes.yml");
    session.open(
        &yaml,
        Some("models:\n  - name: orders\n    tests:\n      - relationships:\n          to: ref('stg_orders')\n"),
    );
    let in_yaml = node_info(&mut session, json!({"uri": uri(&yaml), "position": {"line": 4, "character": 20}}));
    assert_eq!(in_yaml["result"]["unique_id"], "model.shop.stg_orders");
    assert!(session.finish().success());
}

#[test]
fn each_change_parses_and_one_more_after_a_change_during_a_parse() {
    let gate = scratch("standalone-gate-file").join("open");
    let script = json!({"manifest": manifest(), "parse_gate": gate});
    let (mut session, log) = Session::fake("standalone-parse", script);
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    until_logged(&log, "the parse of the first document", |_| parses(&log).len() == 1);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    parses_stay(&log, 1);
    std::fs::write(&gate, "").unwrap();
    until_logged(&log, "the parse after the save", |_| parses(&log).len() == 2);
    let orders = node_info(&mut session, json!("model.shop.orders"));
    assert_eq!(orders["result"]["relation"], "\"shop\".\"main\".\"orders\"", "{orders}");
    parses_stay(&log, 2);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    until_logged(&log, "the parse after the second save", |_| parses(&log).len() == 3);
    assert!(session.finish().success());
}

#[test]
fn a_file_changed_on_disk_with_no_notification_is_parsed_at_the_next_request() {
    let project = copy_fixture("standalone-disk-change");
    let gate = scratch("standalone-disk-change-gate").join("open");
    let script = json!({"manifest": manifest(), "parse_gate": gate});
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, log) = Session::fake_with("standalone-disk-change-session", script, &flags, None);
    session.initialize_as(&project, json!({"workspace": {"inlayHint": {"refreshSupport": true}}}));
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    until_logged(&log, "the parse of the first document", |_| parses(&log).len() == 1);
    // Written while it runs, with no notification, as Claude Code's `Edit` of a file it has not opened.
    std::fs::write(project.join("models/marts/during.sql"), "select 1").unwrap();
    std::fs::write(&gate, "").unwrap();
    // The parse has ended once the client is asked to refresh its hints: its end looks at no file.
    session.wait_for(|m| m["method"] == "workspace/inlayHint/refresh");
    parses_stay(&log, 1);
    // The next request that would parse finds the file changed.
    session.received.clear();
    node_info(&mut session, json!("model.shop.orders"));
    until_logged(&log, "the parse after the write", |_| parses(&log).len() == 2);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    session.wait_for(|m| m["method"] == "workspace/inlayHint/refresh");
    // A hover, once there is an index, asks for it after its answer: another write, another parse.
    std::fs::write(project.join("models/marts/later.sql"), "select 2").unwrap();
    session.received.clear();
    session.request("textDocument/hover", position_in(&orders, "stg_orders", 1));
    until_logged(&log, "the parse a hover asks for", |_| parses(&log).len() == 3);
    session.wait_for(|m| m["method"] == "workspace/inlayHint/refresh");
    // An open text is no change on disk.
    let change = json!({"textDocument": {"uri": uri(&orders), "version": 2}, "contentChanges": [{"text": "select 1"}]});
    session.notify("textDocument/didChange", change);
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 3);
    assert!(session.finish().success());
}

#[test]
fn ignores_watched_changes_of_dbt_s_own_output() {
    let link = scratch("dbt-output-link").join("shop");
    std::os::unix::fs::symlink(fixture(), &link).unwrap();
    let (mut session, log) = Session::fake("dbt-output-events", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let changed = |paths: &[PathBuf]| {
        let changes: Vec<Value> = paths.iter().map(|path| json!({"uri": uri(path), "type": 2})).collect();
        json!({"changes": changes})
    };
    let written = ["target", "target/compiled/shop/x.sql", "logs/dbt-lsp.log"].map(|path| fixture().join(path));
    // Through another spelling of the root: the directory itself, as the fixture has none to resolve below.
    session.notify("workspace/didChangeWatchedFiles", changed(&[&written[..], &[link.join("logs")]].concat()));
    // A request that wants the index, which a change of the graph would parse again.
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 1);
    let both = changed(&[fixture().join("models/marts/orders.sql"), fixture().join("logs/dbt-lsp.log")]);
    session.notify("workspace/didChangeWatchedFiles", both);
    node_info(&mut session, json!("model.shop.orders"));
    until_logged(&log, "the parse after a model's change", |_| parses(&log).len() == 2);
    assert!(session.finish().success());
}

#[test]
fn ignores_dbt_s_output_through_a_link_out_of_the_project() {
    let project = copy_fixture("dbt-output-out-link");
    let elsewhere = project.with_file_name("elsewhere-logs");
    std::fs::create_dir_all(&elsewhere).unwrap();
    // As lsp-mode reports it, whatever links the scratch directory sits under.
    let elsewhere = std::fs::canonicalize(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, project.join("logs")).unwrap();
    let arguments = [Path::new("--project-dir"), &project];
    let (mut session, log) =
        Session::fake_with("dbt-output-out-link-session", json!({"manifest": manifest()}), &arguments, None);
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    let written = json!({"changes": [{"uri": uri(&elsewhere.join("dbt-lsp.log")), "type": 2}]});
    session.notify("workspace/didChangeWatchedFiles", written);
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 1);
    assert!(session.finish().success());
}

#[test]
fn a_completion_starts_no_parse() {
    let (mut session, log) = Session::fake("standalone-completion", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    // No method of the server's, whose answer would read the index.
    let orders = fixture().join("models/marts/orders.sql");
    session.request("textDocument/completion", position_in(&orders, "select", 1));
    parses_stay(&log, 0);
    assert!(session.finish().success());
}

/// A line of dbt parse's JSON log, as both builds write it.
fn dbt_event(level: &str, code: &str, msg: &str) -> String {
    json!({"data": {}, "info": {"code": code, "level": level, "msg": msg, "name": "Event"}}).to_string() + "\n"
}

#[test]
fn the_parse_s_problems_are_diagnostics_until_a_parse_without_them() {
    let log_file = scratch("standalone-problems-log").join("parse.jsonl");
    let completed = dbt_event("info", "Q039", "Finished 'parse'");
    let error = "[error] [DependencyNotFound (dbt1048)]: Ref 'nowhere' not found\n  --> models/marts/orders.sql:1:21";
    let in_package = "[warning] [W (dbt1060)]: a warning\n  --> macros/is_positive.sql";
    let nowhere = "[warning] [UnusedResourceConfigPath (dbt1097)]: Configuration paths exist";
    let problems = [dbt_event("error", "1048", error), dbt_event("warn", "1060", in_package)].concat();
    std::fs::write(&log_file, problems + &dbt_event("warn", "1097", nowhere) + &completed).unwrap();
    let script = json!({"manifest": manifest(), "parse_log": log_file});
    let (mut session, _) = Session::fake("standalone-problems", script);
    session.initialize(&fixture());
    let orders = fixture().join("models/marts/orders.sql");
    // dbt counts code points, the client UTF-16 units: the emoji is two.
    session.open(&orders, Some("select '\u{1F600}' as x, {{ ref('nowhere') }}\n"));
    let published = |path: &Path| {
        let uri = uri(path);
        move |m: &Value| m["params"]["uri"] == uri
    };
    let found = session.wait_for(published(&orders))["params"]["diagnostics"][0].clone();
    let at = json!({"line": 0, "character": 21});
    let message = "[DependencyNotFound (dbt1048)]: Ref 'nowhere' not found\n  --> models/marts/orders.sql:1:21";
    let expected = json!({"range": {"start": at, "end": at}, "severity": 1, "source": "dbt parse", "code": "1048",
                          "message": message});
    assert_eq!(found, expected);
    let macro_file = fixture().join("dbt_packages/shop_utils/macros/is_positive.sql");
    let warned = session.wait_for(published(&macro_file))["params"]["diagnostics"][0].clone();
    assert_eq!((&warned["severity"], &warned["range"]["start"]), (&json!(2), &json!({"line": 0, "character": 0})));
    let logged = session.wait_for(|m| m["method"] == "window/logMessage" && m["params"]["type"] == 2);
    assert!(logged["params"]["message"].as_str().unwrap().contains("dbt1097"), "{logged}");
    // A clean parse clears them.
    std::fs::write(&log_file, &completed).unwrap();
    session.received.clear();
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&orders)}}));
    let cleared = session.wait_for(published(&orders));
    assert_eq!(cleared["params"]["diagnostics"], json!([]));
    assert!(session.finish().success());
}

#[test]
fn a_broken_project_file_is_shown_and_a_problem_in_no_file_logged_once() {
    let project = copy_fixture("standalone-broken-project");
    // dbt's log alone says where the project file is broken: one that the
    // guard on packages cannot read is refused before dbt runs.
    let log_file = scratch("standalone-broken-project-log").join("parse.jsonl");
    let nowhere = dbt_event("warn", "1097", "[warning] [UnusedResourceConfigPath (dbt1097)]: Configuration paths");
    let at =
        |line: u32| dbt_event("error", "1013", &format!("[error] [YAML (dbt1013)]\n  --> dbt_project.yml:{line}:1"));
    let completed = dbt_event("info", "Q039", "Finished 'parse'");
    std::fs::write(&log_file, at(2) + &nowhere + &completed).unwrap();
    let flags = [Path::new("--project-dir"), &project];
    let script = json!({"parse_log": log_file});
    let (mut session, _) = Session::fake_with("standalone-broken-project-session", script, &flags, None);
    session.initialize(&project);
    let model = project.join("models/marts/orders.sql");
    session.open(&model, None);
    let on_line = |line: u64| move |params: &Value| params["diagnostics"][0]["range"]["start"]["line"] == line;
    published(&mut session, &project.join("dbt_project.yml"), on_line(1));
    // The next parse moves the error; the warning stays logged once.
    std::fs::write(&log_file, at(3) + &nowhere + &completed).unwrap();
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&model)}}));
    published(&mut session, &project.join("dbt_project.yml"), on_line(2));
    let warned = |m: &&Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("dbt1097"));
    assert_eq!(session.received.iter().filter(warned).count(), 1);
    assert!(session.finish().success());
}

#[test]
fn a_yaml_error_named_from_dbt_s_directory_joins_the_last_run_s_failures() {
    let project = copy_fixture("standalone-cwd-project");
    std::fs::write(project.join("models/marts/_models.yml"), RUN_MODELS).unwrap();
    write_run(&project, &run_results("build", true), &run_manifest());
    // A second package holds the macro's path: dbt's place is no longer one file.
    std::fs::create_dir_all(project.join("dbt_packages/other/macros")).unwrap();
    std::fs::write(project.join("dbt_packages/other/dbt_project.yml"), "name: other\n").unwrap();
    std::fs::write(project.join("dbt_packages/other/macros/is_positive.sql"), "").unwrap();
    // dbt runs where the proxy runs, here the project's parent, as an editor
    // opened at a repository's top starts it.
    let error = "[error] [YAML (dbt1013)]\n  --> shop/models/marts/_models.yml:3:5";
    let twice = dbt_event("warn", "1060", "[warning] [W (dbt1060)]: twice\n  --> macros/is_positive.sql");
    let log_file = scratch("standalone-cwd-log").join("parse.jsonl");
    let log = [dbt_event("error", "1013", error), twice, dbt_event("info", "Q039", "Finished")].concat();
    std::fs::write(&log_file, log).unwrap();
    let script = json!({"parse_log": log_file});
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with("standalone-cwd", script, &flags, project.parent());
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    let both = |params: &Value| params["diagnostics"].as_array().is_some_and(|found| found.len() == 2);
    let models = project.join("models/marts/_models.yml");
    let found = published(&mut session, &models, both);
    let sources: Vec<&Value> = found["diagnostics"].as_array().unwrap().iter().map(|d| &d["source"]).collect();
    assert_eq!(sources, [&json!("dbt-lsp-plus"), &json!("dbt parse")]);
    let logged = session.wait_for(|m| m["method"] == "window/logMessage" && m["params"]["type"] == 2);
    assert!(logged["params"]["message"].as_str().unwrap().contains("twice"), "{logged}");
    // An edit drops the failure; the client moves the parse's range, which
    // a publication now would put back where it was.
    session.received.clear();
    let edit = json!({"textDocument": {"uri": uri(&models), "version": 2}, "contentChanges": []});
    session.notify("textDocument/didChange", edit);
    session.request("workspace/symbol", json!({"query": "orders"}));
    let on_models = |m: &&Value| m["params"]["uri"] == uri(&models);
    assert_eq!(session.received.iter().filter(on_models).count(), 0, "{:?}", session.received);
    // The save's parse finds the same problem: published again, the failure gone.
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&models)}}));
    let parsed =
        published(&mut session, &models, |params| params["diagnostics"].as_array().is_some_and(|d| d.len() == 1));
    assert_eq!(parsed["diagnostics"][0]["source"], "dbt parse");
    assert!(session.finish().success());
}

/// A copy of the fixture for test NAME with `RUN_MODELS`, and a session
/// with the fake, whose parses log ERROR at PLACE.
fn with_a_problem(name: &str, place: &str) -> (PathBuf, Session) {
    let project = copy_fixture(&format!("{name}-project"));
    std::fs::write(project.join("models/marts/_models.yml"), RUN_MODELS).unwrap();
    let error = format!("[warning] [W (dbt1060)]: x\n  --> {place}");
    let log_file = scratch(&format!("{name}-log")).join("parse.jsonl");
    std::fs::write(&log_file, dbt_event("warn", "1060", &error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"parse_log": log_file, "manifest": manifest()});
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with(name, script, &flags, None);
    session.initialize_as(&project, json!({"workspace": {"inlayHint": {"refreshSupport": true}}}));
    (project, session)
}

#[test]
fn a_crlf_file_opened_with_lf_counts_as_saved() {
    let (project, mut session) = with_a_problem("standalone-crlf", "models/marts/customers.sql:3:1");
    let customers = project.join("models/marts/customers.sql");
    let text = std::fs::read_to_string(&customers).unwrap();
    std::fs::write(&customers, text.replace('\n', "\r\n")).unwrap();
    // As Emacs sends it: its buffer, with LF.
    session.open(&customers, Some(&text));
    published(&mut session, &customers, |params| params["diagnostics"] != json!([]));
    session.wait_for(|m| m["method"] == "workspace/inlayHint/refresh");
    session.received.clear();
    session.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": uri(&project.join("models/marts/orders.sql"))}}),
    );
    published(&mut session, &customers, |params| params["diagnostics"] != json!([]));
    assert!(session.finish().success());
}

#[test]
fn a_new_run_leaves_an_unsaved_document_s_ranges_to_the_client() {
    let (project, mut session) = with_a_problem("standalone-run-unsaved", "models/marts/_models.yml:3:5");
    let models = project.join("models/marts/_models.yml");
    session.open(&models, None);
    published(&mut session, &models, |params| params["diagnostics"] != json!([]));
    let top = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}});
    let edit = json!({"textDocument": {"uri": uri(&models), "version": 2},
                      "contentChanges": [{"range": top, "text": "# a\n# b\n"}]});
    session.notify("textDocument/didChange", edit);
    // The edit handled before the build ends: its failures are newer.
    session.request("workspace/symbol", json!({"query": "orders"}));
    session.received.clear();
    // A build fails, on the model and on a test of this file.
    write_run(&project, &run_results("build", true), &run_manifest());
    published(&mut session, &project.join("models/marts/orders.sql"), |params| params["diagnostics"] != json!([]));
    session.request("workspace/symbol", json!({"query": "orders"}));
    assert!(session.received.iter().all(|m| m["params"]["uri"] != uri(&models)), "{:?}", session.received);
    assert!(session.finish().success());
}

#[test]
fn a_parse_cut_short_keeps_the_last_problems() {
    let log_file = scratch("standalone-cut-short-log").join("parse.jsonl");
    let error = |what: &str| {
        let message = format!("[error] [DependencyNotFound (dbt1048)]: {what}\n  --> models/marts/orders.sql:1:1");
        dbt_event("error", "1048", &message)
    };
    std::fs::write(&log_file, error("first") + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"parse_log": log_file, "parse_exit": 1});
    let (mut session, _) = Session::fake("standalone-cut-short", script);
    session.initialize(&fixture());
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    published(&mut session, &orders, |params| params["diagnostics"] != json!([]));
    // A parse that dies before its end: its failure logged, nothing published.
    std::fs::write(&log_file, error("second")).unwrap();
    session.received.clear();
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&orders)}}));
    let second = |m: &Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("second"));
    session.wait_for(second);
    // Past whatever else the end of that parse sent.
    session.request("workspace/symbol", json!({"query": "orders"}));
    assert!(session.received.iter().all(|m| m["method"] != "textDocument/publishDiagnostics"));
    assert!(session.finish().success());
}

/// The URI under which the server publishes dbt's error at PLACE,
/// named from the working directory that DIRECTORY gives of a copy of the
/// fixture, as test NAME.
fn published_from(name: &str, directory: impl Fn(&Path) -> PathBuf, place: &str) -> (PathBuf, Value) {
    let project = copy_fixture(&format!("{name}-project"));
    let cwd = directory(&project);
    std::fs::create_dir_all(&cwd).unwrap();
    let error = format!("[error] [DependencyNotFound (dbt1048)]\n  --> {place}:1:1");
    let log_file = scratch(&format!("{name}-log")).join("parse.jsonl");
    std::fs::write(&log_file, dbt_event("error", "1048", &error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"parse_log": log_file});
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with(name, script, &flags, Some(&cwd));
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    let found = |m: &Value| m["method"] == "textDocument/publishDiagnostics" && m["params"]["diagnostics"] != json!([]);
    let published = session.wait_for(found)["params"]["uri"].clone();
    assert!(session.finish().success());
    (project, published)
}

#[test]
fn a_problem_named_through_dot_dot_is_published_as_the_root_spells_it() {
    // From a sibling, dbt's path reads from the root too.
    let (project, sibling) =
        published_from("standalone-sibling", |p| p.with_file_name("elsewhere"), "../shop/models/marts/orders.sql");
    assert_eq!(sibling, uri(&project.join("models/marts/orders.sql")));
    // From within the project, only from dbt's working directory.
    let (project, within) = published_from("standalone-within", |p| p.join("sub"), "../models/marts/orders.sql");
    assert_eq!(within, uri(&project.join("models/marts/orders.sql")));
}

#[test]
fn a_parse_leaves_an_unsaved_document_s_ranges_to_the_client() {
    let error = "[error] [DependencyNotFound (dbt1048)]: x\n  --> models/marts/customers.sql:3:1";
    let log_file = scratch("standalone-unsaved-log").join("parse.jsonl");
    std::fs::write(&log_file, dbt_event("error", "1048", error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"parse_log": log_file, "manifest": manifest()});
    let (mut session, _) = Session::fake("standalone-unsaved", script);
    session.initialize_as(&fixture(), json!({"workspace": {"inlayHint": {"refreshSupport": true}}}));
    let customers = fixture().join("models/marts/customers.sql");
    session.open(&customers, None);
    published(&mut session, &customers, |params| params["diagnostics"] != json!([]));
    session.wait_for(|m| m["method"] == "workspace/inlayHint/refresh");
    // Two lines above dbt's, unsaved: the client moves the range itself.
    let top = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}});
    let edit = json!({"textDocument": {"uri": uri(&customers), "version": 2},
                      "contentChanges": [{"range": top, "text": "-- a\n-- b\n"}]});
    session.notify("textDocument/didChange", edit);
    session.received.clear();
    // Another file's save parses again, and dbt finds the same problem on disk.
    session.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": uri(&fixture().join("models/marts/orders.sql"))}}),
    );
    session.wait_for(|m| m["method"] == "workspace/inlayHint/refresh");
    session.request("workspace/symbol", json!({"query": "orders"}));
    assert!(session.received.iter().all(|m| m["params"]["uri"] != uri(&customers)), "{:?}", session.received);
    assert!(session.finish().success());
}

#[test]
fn no_parse_while_packages_need_the_network() {
    let project = copy_fixture("unsettled-packages");
    std::fs::write(project.join("packages.yml"), "packages:\n  - package: dbt-labs/dbt_utils\n    version: 1.4.1\n")
        .unwrap();
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, log) = Session::fake_with("unsettled-session", json!({"manifest": manifest()}), &flags, None);
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    let refused = node_info(&mut session, json!("model.shop.orders"));
    assert!(refused["error"]["message"].as_str().unwrap().contains("run dbt deps"), "{refused}");
    node_info(&mut session, json!("model.shop.orders"));
    let deps = |m: &&Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("run dbt deps"));
    let logged_once = session.received.iter().filter(|m| m["method"] == "window/logMessage").filter(deps).count();
    assert_eq!(logged_once, 1);
    assert!(parses(&log).is_empty());
    assert!(session.finish().success());
}

#[test]
fn a_refused_parse_leaves_the_last_index_to_answer() {
    let project = copy_fixture("refused-parse-project");
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, log) = Session::fake_with("refused-parse", json!({"manifest": manifest()}), &flags, None);
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    // Written outside the editor: a hub package that no lock names, and a model.
    std::fs::write(project.join("packages.yml"), "packages:\n  - package: dbt-labs/dbt_utils\n    version: 1.4.1\n")
        .unwrap();
    std::fs::write(project.join("models/marts/written.sql"), "select 1").unwrap();
    let meanwhile = node_info(&mut session, json!("model.shop.orders"));
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 1);
    assert_eq!(
        (meanwhile.get("error"), &meanwhile["result"]["unique_id"]),
        (None, &json!("model.shop.orders")),
        "answered from the last index meanwhile: {meanwhile}"
    );
    let deps = |m: &&Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("run dbt deps"));
    let logged_once = session.received.iter().filter(|m| m["method"] == "window/logMessage").filter(deps).count();
    assert_eq!(logged_once, 1);
    assert!(session.finish().success());
}

#[test]
fn exit_stops_a_parse_under_way() {
    let (mut session, log) = Session::fake("parse-hangs", json!({"manifest": manifest(), "parse_hang": true}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    node_info(&mut session, json!("model.shop.orders"));
    until_logged(&log, "the parse", |_| parses(&log).len() == 1);
    let pid = parses(&log)[0]["pid"].as_u64().unwrap();
    assert!(session.finish().success());
    let mut alive = std::process::Command::new("kill");
    alive.args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null());
    assert!(!alive.status().unwrap().success(), "the parse {pid} outlived the proxy");
}

#[test]
fn a_hover_on_a_ref_gets_the_summary_first_and_any_other_passes_byte_for_byte() {
    let (mut session, _) = Session::fake("hover-summary", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}});
    let hover = session.request("textDocument/hover", on_ref);
    let value = hover["result"]["contents"]["value"].as_str().unwrap();
    assert_eq!(value, "model view, \"shop\".\"main\".\"stg_orders\"\n`model.shop.stg_orders`");
    // Line 1, column 1 stands for the file's own model.
    let start = json!({"textDocument": {"uri": orders}, "position": {"line": 0, "character": 0}});
    let own = session.request("textDocument/hover", start);
    let value = own["result"]["contents"]["value"].as_str().unwrap();
    assert_eq!(value, "model table, \"shop\".\"main\".\"orders\"\n`model.shop.orders`");
    let elsewhere = json!({"textDocument": {"uri": orders}, "position": {"line": 3, "character": 0}});
    assert_eq!(session.request("textDocument/hover", elsewhere)["result"], Value::Null);
    assert!(session.finish().success());
}

#[test]
fn with_no_dbt_node_info_says_why_it_has_nothing() {
    let project = fixture().to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    session.initialize(&fixture());
    let answer = node_info(&mut session, json!("model.shop.orders"));
    assert_eq!(answer["error"]["code"], json!(-32803), "{answer}");
    // The parse is tried with the same dbt, which cannot start either: said once.
    let failed = |m: &Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("dbt parse failed"));
    session.wait_for(|m| m["method"] == "window/logMessage" && failed(m));
    let again = node_info(&mut session, json!("model.shop.orders"));
    assert!(again["error"]["message"].as_str().is_some_and(|text| text.contains("dbt parse failed")), "{again}");
    assert_eq!(session.received.iter().filter(|m| failed(m)).count(), 1);
    assert!(session.finish().success());
}

#[test]
fn no_parse_after_shutdown_but_one_after_a_crash() {
    let (mut session, log) = Session::fake("parse-shutdown", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let path = fixture().join("models/marts/orders.sql");
    assert_eq!(session.request("shutdown", Value::Null)["result"], Value::Null);
    // Neither a change nor a request that would parse parses.
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 1);
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
}

#[test]
fn a_successful_parse_replaces_the_index_and_a_failed_one_lays_over_it() {
    let (mut session, _) = Session::fake("parse-replace", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let script = scratch_of("parse-replace").join("script.json");
    let path = fixture().join("models/marts/orders.sql");
    let rewrite_and_save = |session: &mut Session, script_text: Value| {
        std::fs::write(&script, script_text.to_string()).unwrap();
        session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    };
    let only_orders = json!({"metadata": {"project_name": "shop"}, "nodes": {"model.shop.orders": {
        "unique_id": "model.shop.orders", "name": "orders", "resource_type": "model", "relation_name": "new", "config": {}}}});
    rewrite_and_save(&mut session, json!({"manifest": only_orders}));
    let request =
        |id: &str| ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": [id]}));
    session.poll(Duration::from_secs(10), request("model.shop.orders"), |node| node["relation"] == "new");
    assert_eq!(node_info(&mut session, json!("model.shop.stg_orders"))["result"], Value::Null, "replaced");
    let partial = json!({"metadata": {"project_name": "shop"}, "nodes": {"model.shop.stg_orders": {
        "unique_id": "model.shop.stg_orders", "name": "stg_orders", "resource_type": "model", "relation_name": "s", "config": {}}}});
    rewrite_and_save(&mut session, json!({"manifest": partial, "parse_exit": 1}));
    session.poll(Duration::from_secs(10), request("model.shop.stg_orders"), Value::is_object);
    assert_eq!(node_info(&mut session, json!("model.shop.orders"))["result"]["relation"], "new", "laid over");
    assert!(session.finish().success());
}

#[test]
fn the_summary_alone_follows_the_client_s_first_hover_format() {
    for (formats, kind) in
        [(json!(["plaintext", "markdown"]), "plaintext"), (json!(["markdown", "plaintext"]), "markdown")]
    {
        let (mut session, _) = Session::fake(&format!("hover-format-{kind}"), json!({"manifest": manifest()}));
        let capabilities = json!({"textDocument": {"hover": {"contentFormat": formats}}});
        let params = json!({"processId": null, "rootUri": uri(&fixture()), "capabilities": capabilities});
        session.request("initialize", params);
        session.notify("initialized", json!({}));
        parsed_orders(&mut session);
        let customers = uri(&fixture().join("models/staging/stg_customers.sql"));
        let on_source = json!({"textDocument": {"uri": customers}, "position": {"line": 5, "character": 23}});
        let hover = session.request("textDocument/hover", on_source);
        // A line break in markdown needs two spaces before it.
        let breaks = if kind == "markdown" { "  \n" } else { "\n" };
        let value = format!("source, raw_customers{breaks}`source.shop.raw.customers`");
        assert_eq!(hover["result"]["contents"], json!({"kind": kind, "value": value}), "{formats}");
        assert!(session.finish().success());
    }
}

#[test]
fn hovers_and_hints_come_from_the_parse_and_saves_are_asked_for() {
    let (mut session, _) = Session::fake("standalone-hover", json!({"manifest": manifest()}));
    let capabilities = session.initialize(&fixture())["capabilities"].clone();
    assert_eq!(capabilities["textDocumentSync"], json!({"openClose": true, "change": 2, "save": {}}));
    assert_eq!([&capabilities["hoverProvider"], &capabilities["inlayHintProvider"]], [&json!(true), &json!(true)]);
    let orders = fixture().join("models/marts/orders.sql");
    let on_ref = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    // Before any document, the hover asks for the parse whose reason it gives, which the proxy has started
    // before it answers.
    let early = session.request("textDocument/hover", on_ref.clone());
    let running = json!({"code": -32803, "message": "dbt parse has not finished: it is running"});
    assert_eq!((early.get("result"), &early["error"]), (None, &running), "before a parse: {early}");
    session.open(&orders, None);
    let hover = session.poll(Duration::from_secs(10), ("textDocument/hover", on_ref), |result| !result.is_null());
    let summary =
        json!({"kind": "plaintext", "value": "model view, \"shop\".\"main\".\"stg_orders\"\n`model.shop.stg_orders`"});
    assert_eq!(hover["contents"], summary);
    let hints = hints_of(&mut session, &orders)["result"].clone();
    assert!(hints.as_array().unwrap().iter().any(|hint| hint["tooltip"] == "model.shop.stg_orders"), "{hints}");
    let elsewhere = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 3, "character": 0}});
    assert_eq!(session.request("textDocument/hover", elsewhere)["result"], Value::Null);
    // `ref(var(...))` names no node the scan can see: no hint.
    assert_eq!(hints_of(&mut session, &fixture().join("models/marts/order_summary.sql"))["result"], json!([]));
    assert!(session.finish().success());
}

#[test]
fn a_hover_at_a_file_s_start_shows_its_node_and_its_facts() {
    let mut manifest = manifest();
    let orders = &mut manifest["nodes"]["model.shop.orders"];
    orders["patch_path"] = json!("shop://models/marts/_orders.yml");
    orders["columns"] = json!({"order_id": {"name": "order_id", "data_type": "integer"}, "status": {"name": "status"}});
    // A generic test that a properties file declares: the manifest places it there.
    manifest["nodes"]["test.shop.not_null_customers_id"] = json!({"unique_id": "test.shop.not_null_customers_id",
        "name": "not_null_customers_id", "resource_type": "test", "package_name": "shop",
        "original_file_path": "models/staging/_sources.yml", "attached_node": "source.shop.raw.customers"});
    let (mut session, _) = Session::fake("hover-start", json!({"manifest": manifest}));
    session.initialize(&fixture());
    let at = |file: &str, line: u64| json!({"textDocument": {"uri": uri(&fixture().join(file))}, "position": {"line": line, "character": 0}});
    session.open(&fixture().join("models/marts/orders.sql"), None);
    let request = ("textDocument/hover", at("models/marts/orders.sql", 0));
    let hover = session.poll(Duration::from_secs(10), request, |result| !result.is_null());
    let value = "model table, \"shop\".\"main\".\"orders\"\n\
                 `model.shop.orders`, properties `models/marts/_orders.yml`\n\
                 columns `order_id` `integer`, `status`";
    assert_eq!(hover["contents"], json!({"kind": "plaintext", "value": value}));
    // Elsewhere in the file, at the start of a file the manifest lacks or of a properties file: nothing.
    let none =
        [("models/marts/orders.sql", 3), ("models/marts/order_summary.sql", 0), ("models/staging/_sources.yml", 0)];
    for (file, line) in none {
        let hover = session.request("textDocument/hover", at(file, line));
        assert_eq!((hover.get("error"), &hover["result"]), (None, &Value::Null), "{file}:{line}: {hover}");
    }
    let start =
        json!({"uri": uri(&fixture().join("models/marts/orders.sql")), "position": {"line": 0, "character": 0}});
    assert_eq!(node_info(&mut session, start)["result"]["summary"], "model table, \"shop\".\"main\".\"orders\"");
    // nodeInfo by the same rule: not the generic test that the manifest places in a properties file.
    let properties =
        json!({"uri": uri(&fixture().join("models/staging/_sources.yml")), "position": {"line": 0, "character": 0}});
    let info = node_info(&mut session, properties);
    assert_eq!((info.get("error"), &info["result"]), (None, &Value::Null), "{info}");
    assert!(session.finish().success());
}

#[test]
fn a_markdown_hover_gives_the_facts_byte_for_byte_as_claude_code_asks() {
    let mut manifest = manifest();
    let orders = &mut manifest["nodes"]["model.shop.orders"];
    orders["patch_path"] = json!("shop://models/marts/_orders.yml");
    orders["columns"] = json!({"tags": {"name": "tags", "data_type": "array<string>"}});
    let (mut session, _) = Session::fake("hover-start-markdown", json!({"manifest": manifest}));
    // Claude Code's hover capability; it hands the agent the contents as they come.
    let capabilities = json!({"textDocument": {"hover": {"contentFormat": ["markdown", "plaintext"]}}});
    session.request("initialize", json!({"processId": null, "rootUri": uri(&fixture()), "capabilities": capabilities}));
    session.notify("initialized", json!({}));
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    let start = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 0, "character": 0}});
    let hover = session.poll(Duration::from_secs(10), ("textDocument/hover", start), |result| !result.is_null());
    let value = "model table, \"shop\".\"main\".\"orders\"  \n\
                 `model.shop.orders`, properties `models/marts/_orders.yml`  \n\
                 columns `tags` `array<string>`";
    assert_eq!(hover["contents"], json!({"kind": "markdown", "value": value}));
    let path = hover["contents"]["value"].as_str().unwrap().contains("models/marts/_orders.yml");
    assert!(path, "the path, byte for byte");
    assert!(session.finish().success());
}

#[test]
fn a_yaml_snapshot_s_file_names_no_node_at_its_start() {
    let project = copy_fixture("hover-yaml-snapshot-project");
    let snapshot = project.join("snapshots/_recent.yml");
    let yaml = "snapshots:\n  - name: recent\n    relation: ref('orders')\n    config:\n      unique_key: order_id\n      strategy: check\n      check_cols: all\n";
    std::fs::write(&snapshot, yaml).unwrap();
    let mut manifest = manifest();
    manifest["nodes"]["snapshot.shop.recent"] = json!({"unique_id": "snapshot.shop.recent", "name": "recent",
        "resource_type": "snapshot", "package_name": "shop", "original_file_path": "snapshots/_recent.yml",
        "config": {"materialized": "snapshot"}});
    let gate = scratch("hover-yaml-snapshot-gate").join("open");
    let script = json!({"manifest": manifest, "parse_gate": gate});
    let (mut session, _) =
        Session::fake_with("hover-yaml-snapshot", script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    let start = json!({"textDocument": {"uri": uri(&snapshot)}, "position": {"line": 0, "character": 0}});
    // Before the index, the scan sees the one snapshot; once indexed, the manifest has it too: neither names it.
    let before = session.request("textDocument/hover", start.clone());
    assert_eq!((before.get("error"), &before["result"]), (None, &Value::Null), "before the index: {before}");
    std::fs::write(&gate, "").unwrap();
    let request = (
        "workspace/executeCommand",
        json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["snapshot.shop.recent"]}),
    );
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    let after = session.request("textDocument/hover", start);
    assert_eq!((after.get("error"), &after["result"]), (None, &Value::Null), "after the index: {after}");
    let info = node_info(&mut session, json!({"uri": uri(&snapshot), "position": {"line": 0, "character": 0}}));
    assert_eq!(info["result"], Value::Null, "{info}");
    assert!(session.finish().success());
}

#[test]
fn a_hover_on_a_node_before_any_index_says_why() {
    let gate = scratch("hover-unindexed-gate").join("open");
    let script = json!({"manifest": manifest(), "parse_gate": gate});
    let (mut session, _) = Session::fake("hover-unindexed", script);
    session.initialize(&fixture());
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    let at = |line: u64, character: u64| json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": line, "character": character}});
    // The first document opened started the parse: on a ref and at the file's start, its reason.
    let running = json!({"code": -32803, "message": "dbt parse has not finished: it is running"});
    let staging = fixture().join("models/staging/stg_customers.sql");
    let on_source = json!({"textDocument": {"uri": uri(&staging)}, "position": {"line": 5, "character": 23}});
    let on_entry = position_in(&fixture().join("models/staging/_sources.yml"), "name: customers", 8);
    for params in [at(4, 28), at(0, 0), on_source, on_entry] {
        let hover = session.request("textDocument/hover", params.clone());
        assert_eq!((hover.get("result"), &hover["error"]), (None, &running), "{params}: {hover}");
    }
    // At `with`, in the argument of a macro call, at a properties file's start: nothing named, null.
    let properties = fixture().join("models/staging/_sources.yml");
    let start = json!({"textDocument": {"uri": uri(&properties)}, "position": {"line": 0, "character": 0}});
    for params in [at(3, 0), position_in(&orders, "'orders.amount'", 3), start] {
        let hover = session.request("textDocument/hover", params);
        assert_eq!((hover.get("error"), &hover["result"]), (None, &Value::Null), "{hover}");
    }
    std::fs::write(&gate, "").unwrap();
    let hover = session.poll(Duration::from_secs(10), ("textDocument/hover", at(4, 28)), |result| !result.is_null());
    assert_eq!(hover["contents"]["value"], "model view, \"shop\".\"main\".\"stg_orders\"\n`model.shop.stg_orders`");
    assert!(session.finish().success());
}

#[test]
fn a_hover_after_a_failed_parse_says_what_node_info_says() {
    let (mut session, _) = Session::fake("hover-unindexed-failed", json!({"parse_exit": 2}));
    session.initialize(&fixture());
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    let end = Instant::now() + Duration::from_secs(10);
    let reason = loop {
        let info = node_info(&mut session, json!("model.shop.orders"));
        if info["error"]["message"] != "dbt parse has not finished: it is running" {
            break info["error"]["message"].clone();
        }
        assert!(Instant::now() < end, "the parse did not end: {info}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(reason.as_str().is_some_and(|reason| reason.starts_with("dbt parse has not finished: ")), "{reason}");
    let on_ref = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    let hover = session.request("textDocument/hover", on_ref);
    assert_eq!((hover.get("result"), &hover["error"]), (None, &json!({"code": -32803, "message": reason})), "{hover}");
    assert!(session.finish().success());
}

/// A models properties file for plan 006's navigation.
const MODELS_YML: &str = "\
version: 2
models:
  - name: orders
    description: \"{{ doc('order_status') }}\"
    columns:
      - name: amount
        data_tests:
          - is_even
          - unique
          - shop_utils.not_negative
      - name: customer_id
        data_tests:
          - relationships:
              arguments:
                to: ref('customers')
                field: customer_id
  - name: customers
    description: \"{{ doc('shop', 'order_status') }}\"
    data_tests:
      - relationships:
          arguments:
            to: ref('orders', v=2)
            field: id
seeds:
  - name: raw_customers
exposures:
  - name: weekly
    depends_on:
      - source('raw', 'orders')
unit_tests:
  - name: t
    model: orders
";

/// A copy of the fixture, for test NAME, with `MODELS_YML` and a generic
/// test in the package `shop_utils`; apart from the session's scratch
/// directory, which the session empties.
fn navigable(name: &str) -> PathBuf {
    let project = copy_fixture(&format!("{name}-project"));
    std::fs::write(project.join("models/marts/_models.yml"), MODELS_YML).unwrap();
    let test = "{% test not_negative(model, column_name) %}\nselect 1\n{% endtest %}\n";
    std::fs::write(project.join("dbt_packages/shop_utils/macros/not_negative.sql"), test).unwrap();
    project
}

/// The position DELTA characters into the first NEEDLE of the ASCII file PATH.
fn position_in(path: &Path, needle: &str, delta: usize) -> Value {
    let text = std::fs::read_to_string(path).unwrap();
    let at = text.find(needle).unwrap() + delta;
    let line = text[..at].matches('\n').count();
    let character = at - text[..at].rfind('\n').map_or(0, |n| n + 1);
    json!({"textDocument": {"uri": uri(path)}, "position": {"line": line, "character": character}})
}

/// A location in FILE of PROJECT, on LINE from START to END.
fn location(project: &Path, file: &str, line: u64, start: u64, end: u64) -> Value {
    let range = json!({"start": {"line": line, "character": start}, "end": {"line": line, "character": end}});
    json!({"uri": uri(&project.join(file)), "range": range})
}

#[test]
fn a_project_that_no_longer_reads_says_so_where_no_symbol_is_empty() {
    let project = navigable("unreadable");
    let (mut session, _) = Session::fake_with("unreadable", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let configuration = project.join("dbt_project.yml");
    let written = std::fs::read_to_string(&configuration).unwrap();
    std::fs::write(&configuration, "name: [").unwrap();
    let on_ref = position_in(&orders, "ref('stg_orders')", 6);
    let message = format!(
        "no dbt project at {}: its dbt_project.yml is missing, is not YAML, or names no project",
        project.display()
    );
    let unreadable = json!({"code": -32803, "message": message});
    let at_4_28 = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    let asked = [
        ("textDocument/definition", on_ref.clone()),
        ("textDocument/references", without_declaration(on_ref.clone())),
        ("workspace/symbol", json!({"query": "orders"})),
        ("textDocument/prepareCallHierarchy", at_4_28),
    ];
    for (method, params) in asked {
        let answer = session.request(method, params);
        assert_eq!((answer.get("result"), &answer["error"]), (None, &unreadable), "{method}: {answer}");
    }
    // Where no target is, no symbol either: empty, as before.
    let nowhere = position_in(&orders, "select", 1);
    let definition = session.request("textDocument/definition", nowhere.clone());
    assert_eq!((definition.get("error"), &definition["result"]), (None, &Value::Null), "{definition}");
    let references = session.request("textDocument/references", without_declaration(nowhere));
    assert_eq!((references.get("error"), &references["result"]), (None, &json!([])), "{references}");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&orders)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "final"]);
    std::fs::write(&configuration, &written).unwrap();
    let found = session.request("textDocument/definition", on_ref);
    assert_eq!(found["result"], location(&project, "models/staging/stg_orders.sql", 0, 0, 0), "{found}");
    assert!(session.finish().success());
}

#[test]
fn a_hover_and_node_information_on_a_project_that_no_longer_reads_say_so() {
    let project = navigable("unreadable-hover");
    let script = json!({"manifest": manifest()});
    let (mut session, _) =
        Session::fake_with("unreadable-hover", script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let by_id =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), by_id, |result| result["unique_id"] == "model.shop.orders");
    // While the project reads, a `ref` that leads nowhere names no node: null, no reason.
    let scratch = project.join("models/marts/scratch.sql");
    std::fs::write(&scratch, "select * from {{ ref('nowhere') }}\n").unwrap();
    session.open(&scratch, None);
    let nowhere = position_in(&scratch, "ref('nowhere')", 6);
    let hover = session.request("textDocument/hover", nowhere.clone());
    assert_eq!((hover.get("error"), &hover["result"]), (None, &Value::Null), "{hover}");
    let info = node_info(&mut session, json!({"uri": nowhere["textDocument"]["uri"], "position": nowhere["position"]}));
    assert_eq!((info.get("error"), &info["result"]), (None, &Value::Null), "{info}");
    let configuration = project.join("dbt_project.yml");
    let written = std::fs::read_to_string(&configuration).unwrap();
    std::fs::write(&configuration, "name: [").unwrap();
    let on_ref = position_in(&orders, "ref('stg_orders')", 6);
    let message = format!(
        "no dbt project at {}: its dbt_project.yml is missing, is not YAML, or names no project",
        project.display()
    );
    let unreadable = json!({"code": -32803, "message": message});
    let hover = session.request("textDocument/hover", on_ref.clone());
    assert_eq!((hover.get("result"), &hover["error"]), (None, &unreadable), "{hover}");
    let info = node_info(&mut session, json!({"uri": on_ref["textDocument"]["uri"], "position": on_ref["position"]}));
    assert_eq!((info.get("result"), &info["error"]), (None, &unreadable), "{info}");
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 20, "character": 0}});
    let hints =
        session.request("textDocument/inlayHint", json!({"textDocument": {"uri": uri(&orders)}, "range": range}));
    assert_eq!((hints.get("error"), &hints["result"]), (None, &json!([])), "{hints}");
    std::fs::write(&configuration, &written).unwrap();
    let hover = session.request("textDocument/hover", on_ref);
    assert!(hover.get("error").is_none() && hover["result"]["contents"].is_object(), "{hover}");
    assert!(session.finish().success());
}

#[test]
fn a_request_on_another_project_s_document_says_why_at_once() {
    let project = navigable("elsewhere-merge");
    let other = project.with_file_name("other");
    let m = other.join("models/m.sql");
    std::fs::create_dir_all(m.parent().unwrap()).unwrap();
    std::fs::write(other.join("dbt_project.yml"), "name: other\n").unwrap();
    std::fs::write(&m, "select * from {{ ref('orders') }}\n").unwrap();
    let script = json!({"manifest": manifest()});
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with("elsewhere-merge", script, &flags, None);
    session.initialize(&project);
    session.open(&m, None);
    let on_ref = position_in(&m, "ref('orders')", 6);
    let elsewhere = format!(
        "{} lies in the dbt project at {}, and this session serves only the one at {}",
        m.display(),
        other.display(),
        project.display()
    );
    // The reason, at once.
    let why = json!({"code": -32803, "message": elsewhere});
    let methods = ["textDocument/hover", "textDocument/references", "textDocument/prepareCallHierarchy"];
    for method in methods.into_iter().chain(["textDocument/definition"]) {
        let answer = session.request(method, without_declaration(on_ref.clone()));
        assert_eq!((answer.get("result"), &answer["error"]), (None, &why), "{method}: {answer}");
    }
    // Over a project that no longer reads, once there is an index, a hover's reason.
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let by_id =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), by_id, |result| result["unique_id"] == "model.shop.orders");
    std::fs::write(project.join("dbt_project.yml"), "name: [").unwrap();
    let refused = session.request("textDocument/hover", position_in(&orders, "ref('stg_orders')", 6));
    let message = format!(
        "no dbt project at {}: its dbt_project.yml is missing, is not YAML, or names no project",
        project.display()
    );
    let why = json!({"code": -32803, "message": message});
    assert_eq!((refused.get("result"), &refused["error"]), (None, &why), "{refused}");
    assert!(session.finish().success());
}

#[test]
fn a_definition_in_yaml_leads_where_dbt_looks() {
    let project = navigable("definition-yaml");
    let (mut session, _) =
        Session::fake_with("definition-yaml", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let yml = project.join("models/marts/_models.yml");
    let mut definition = |needle: &str, delta: usize| {
        session.request("textDocument/definition", position_in(&yml, needle, delta))["result"].clone()
    };
    let expected = [
        ("ref('customers')", 6, location(&project, "models/marts/customers.sql", 0, 0, 0)),
        ("order_status", 2, location(&project, "models/docs.md", 0, 8, 20)),
        ("name: orders", 8, location(&project, "models/marts/orders.sql", 0, 0, 0)),
        ("- is_even", 4, location(&project, "tests/generic/is_even.sql", 0, 8, 15)),
        (
            "shop_utils.not_negative",
            12,
            location(&project, "dbt_packages/shop_utils/macros/not_negative.sql", 0, 8, 20),
        ),
        ("'raw', 'orders'", 9, location(&project, "models/staging/_sources.yml", 6, 14, 20)),
        ("model: orders", 9, location(&project, "models/marts/orders.sql", 0, 0, 0)),
        ("'shop', 'order_status'", 10, location(&project, "models/docs.md", 0, 8, 20)),
        ("name: raw_customers", 8, location(&project, "seeds/raw_customers.csv", 0, 0, 0)),
        // dbt's own test is in no file of the project.
        ("- unique", 4, Value::Null),
    ];
    for (needle, delta, location) in expected {
        assert_eq!(definition(needle, delta), location, "{needle}");
    }
    // A source's and a table's `name:` lead to themselves.
    let sources = project.join("models/staging/_sources.yml");
    let on_source = session.request("textDocument/definition", position_in(&sources, "name: raw", 7));
    assert_eq!(on_source["result"], location(&project, "models/staging/_sources.yml", 3, 10, 13));
    let on_table = session.request("textDocument/definition", position_in(&sources, "name: orders", 8));
    assert_eq!(on_table["result"], location(&project, "models/staging/_sources.yml", 6, 14, 20));
    assert!(session.finish().success());
}

#[test]
fn a_name_anywhere_leads_where_the_lookup_resolves_it() {
    let (mut session, _) = Session::fake("name-anywhere", json!({}));
    let result = session.initialize(&fixture());
    assert!(result["capabilities"].get("experimental").is_none(), "{result}");
    // An entry's `name:` in an unsaved properties file: the model's references.
    let models = fixture().join("models/marts/_models.yml");
    session.open(&models, Some("models:\n  - name: orders\n"));
    let on_entry = json!({"textDocument": {"uri": uri(&models)}, "position": {"line": 1, "character": 11},
                          "context": {"includeDeclaration": false}});
    let used = session.request("textDocument/references", on_entry);
    let in_customers = location(&fixture(), "models/marts/customers.sql", 2, 17, 23);
    assert!(used["result"].as_array().is_some_and(|found| found.contains(&in_customers)), "{used}");
    // A `ref` in a docs block of a file that is no SQL, YAML or Python: its model.
    let overview = fixture().join("models/overview.md");
    let block = "{% docs overview %}{{ ref('orders') }}{% enddocs %}";
    session.open(&overview, Some(block));
    let at = block.find("orders").unwrap() + 2;
    let on_ref = json!({"textDocument": {"uri": uri(&overview)}, "position": {"line": 0, "character": at}});
    let found = session.request("textDocument/definition", on_ref);
    assert_eq!(found["result"], location(&fixture(), "models/marts/orders.sql", 0, 0, 0), "{found}");
    assert!(session.finish().success());
}

#[test]
fn a_hover_in_yaml_shows_a_doc_s_text_and_a_test_s_signature() {
    let project = navigable("hover-yaml");
    let (mut session, _) = Session::fake_with("hover-yaml", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let yml = project.join("models/marts/_models.yml");
    let on_doc = session.request("textDocument/hover", position_in(&yml, "order_status", 2));
    // In the format of a client that names none.
    let text = "The status of an order: placed, shipped or returned.";
    assert_eq!(on_doc["result"]["contents"], json!({"kind": "plaintext", "value": text}));
    let on_test = session.request("textDocument/hover", position_in(&yml, "- is_even", 4));
    let signature = "test is_even(model, column_name)\n\nin package shop";
    assert_eq!(on_test["result"]["contents"], json!({"kind": "plaintext", "value": signature}));
    // Elsewhere, null.
    assert_eq!(session.request("textDocument/hover", position_in(&yml, "customer_id", 2))["result"], Value::Null);
    assert!(session.finish().success());
}

#[test]
fn an_entry_s_name_is_its_node_for_hover_and_node_info() {
    let project = navigable("hover-entry");
    let script = json!({"manifest": manifest()});
    let (mut session, _) = Session::fake_with("hover-entry", script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    let on_name = position_in(&project.join("models/marts/_models.yml"), "name: orders", 8);
    let hover = session.request("textDocument/hover", on_name.clone());
    assert_eq!(hover["result"]["contents"]["value"], "model table, \"shop\".\"main\".\"orders\"\n`model.shop.orders`");
    let argument = json!({"uri": on_name["textDocument"]["uri"], "position": on_name["position"]});
    assert_eq!(node_info(&mut session, argument)["result"]["unique_id"], "model.shop.orders");
    // A source table's `name:` too.
    let on_table = position_in(&project.join("models/staging/_sources.yml"), "name: customers", 8);
    let value = "source, raw_customers\n`source.shop.raw.customers`";
    assert_eq!(session.request("textDocument/hover", on_table)["result"]["contents"]["value"], value);
    assert!(session.finish().success());
}

#[test]
fn a_markdown_hover_keeps_a_doc_s_line_breaks_and_fences_a_signature() {
    let project = navigable("hover-markdown");
    let docs = "{% docs order_status %}\nThe status of an order:\nplaced, shipped or returned.\n{% enddocs %}\n";
    std::fs::write(project.join("models/docs.md"), docs).unwrap();
    let (mut session, _) =
        Session::fake_with("hover-markdown", json!({}), &[Path::new("--project-dir"), &project], None);
    let capabilities = json!({"textDocument": {"hover": {"contentFormat": ["markdown", "plaintext"]}}});
    session.request("initialize", json!({"processId": null, "rootUri": uri(&project), "capabilities": capabilities}));
    session.notify("initialized", json!({}));
    let yml = project.join("models/marts/_models.yml");
    let on_doc = session.request("textDocument/hover", position_in(&yml, "order_status", 2));
    let text = "The status of an order:\nplaced, shipped or returned.";
    assert_eq!(on_doc["result"]["contents"], json!({"kind": "markdown", "value": text}));
    let on_test = session.request("textDocument/hover", position_in(&yml, "- is_even", 4));
    let signature = "```jinja\ntest is_even(model, column_name)\n```\n\nin package `shop`";
    assert_eq!(on_test["result"]["contents"], json!({"kind": "markdown", "value": signature}));
    assert!(session.finish().success());
}

#[test]
fn a_definition_names_an_open_document_by_the_uri_it_was_opened_under() {
    let project = navigable("definition-spelling");
    let link = project.with_file_name("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    let (mut session, _) =
        Session::fake_with("definition-spelling", json!({}), &[Path::new("--project-dir"), &link], None);
    session.initialize(&link);
    // The model opened under its resolved spelling, the properties through the link.
    let customers = project.join("models/marts/customers.sql");
    session.open(&customers, None);
    let on_ref = position_in(&link.join("models/marts/_models.yml"), "ref('customers')", 6);
    let found = session.request("textDocument/definition", on_ref);
    assert_eq!(found["result"]["uri"], uri(&customers));
    assert!(session.finish().success());
}

#[test]
fn a_ref_and_a_macro_call_in_sql_lead_where_dbt_resolves_them() {
    let project = copy_fixture("sql-definitions-project");
    let package_models = project.join("dbt_packages/shop_utils/models");
    std::fs::create_dir_all(&package_models).unwrap();
    std::fs::write(package_models.join("orders.sql"), "select 1 as order_id").unwrap();
    std::fs::write(package_models.join("uses.sql"), "select * from {{ ref('orders') }}").unwrap();
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with("sql-definitions", json!({}), &flags, None);
    session.initialize(&project);
    let mut definition = |file: &str, needle: &str, delta: usize| {
        session.request("textDocument/definition", position_in(&project.join(file), needle, delta))["result"].clone()
    };
    let on_orders = definition("models/marts/orders.sql", "ref('stg_orders')", 6);
    assert_eq!(on_orders, location(&project, "models/staging/stg_orders.sql", 0, 0, 0));
    // A macro leads to its whole block, as dbt lsp answers, from its package or its name.
    let block = |file: &str| {
        json!({"uri": uri(&project.join(file)),
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 15}}})
    };
    let is_positive = block("dbt_packages/shop_utils/macros/is_positive.sql");
    assert_eq!(definition("models/marts/orders.sql", "shop_utils.is_positive", 3), is_positive);
    assert_eq!(definition("models/marts/orders.sql", "is_positive(", 2), is_positive);
    assert_eq!(definition("models/staging/stg_orders.sql", "cents_to_dollars", 2), block("macros/money.sql"));
    // A package's model looks in its own package first, unless the flag says the root.
    let uses = "dbt_packages/shop_utils/models/uses.sql";
    assert_eq!(
        definition(uses, "ref('orders')", 6),
        location(&project, "dbt_packages/shop_utils/models/orders.sql", 0, 0, 0)
    );
    let config = std::fs::read_to_string(project.join("dbt_project.yml")).unwrap();
    let flag = "flags:\n  require_ref_searches_node_package_before_root: false\n";
    std::fs::write(project.join("dbt_project.yml"), format!("{config}{flag}")).unwrap();
    assert_eq!(definition(uses, "ref('orders')", 6), location(&project, "models/marts/orders.sql", 0, 0, 0));
    // dbt's own macros are no file of the project.
    let scratch = project.join("models/marts/scratch.sql");
    session.open(&scratch, Some("select {{ dbt.current_timestamp() }}"));
    let on_dbt = json!({"textDocument": {"uri": uri(&scratch)}, "position": {"line": 0, "character": 14}});
    let answer = session.request("textDocument/definition", on_dbt);
    assert_eq!((answer.get("error"), &answer["result"]), (None, &Value::Null), "{answer}");
    assert!(session.finish().success());
}

#[test]
fn the_references_of_a_ref_and_a_macro_span_the_project() {
    let (mut session, _) = Session::fake("references-alone", json!({}));
    session.initialize(&fixture());
    let mut references = |file: &str, needle: &str, delta: usize, declaration: bool| {
        let mut params = position_in(&fixture().join(file), needle, delta);
        params["context"] = json!({"includeDeclaration": declaration});
        session.request("textDocument/references", params)["result"].clone()
    };
    // Each location as its file, relative to the fixture, its line and its characters.
    let at = |answer: &Value| -> Vec<(String, u64, u64, u64)> {
        let root = uri(&fixture());
        let place = |location: &Value| {
            let (start, end) = (&location["range"]["start"], &location["range"]["end"]);
            let file = location["uri"].as_str().unwrap().strip_prefix(&root).unwrap().trim_start_matches('/');
            let number = |value: &Value| value.as_u64().unwrap();
            (file.to_owned(), number(&start["line"]), number(&start["character"]), number(&end["character"]))
        };
        answer.as_array().unwrap().iter().map(place).collect()
    };
    let stg_orders = references("models/marts/orders.sql", "ref('stg_orders')", 6, false);
    let uses = [("models/marts/orders.sql", 4, 26, 36), ("snapshots/orders_snapshot.sql", 2, 22, 32)];
    assert_eq!(at(&stg_orders), uses.map(|(file, line, start, end)| (file.to_owned(), line, start, end)));
    let declared = references("snapshots/orders_snapshot.sql", "stg_orders", 2, true);
    let mut with_file = uses.map(|(file, line, start, end)| (file.to_owned(), line, start, end)).to_vec();
    with_file.insert(1, ("models/staging/stg_orders.sql".to_owned(), 0, 0, 0));
    assert_eq!(at(&declared), with_file, "its file's start too");
    let cents = references("macros/money.sql", "cents_to_dollars", 2, false);
    assert_eq!(at(&cents), [("models/staging/stg_orders.sql".to_owned(), 10, 11, 27)]);
    assert!(session.finish().success());
}

/// PARAMS without the declaration, as references.
fn without_declaration(mut params: Value) -> Value {
    params["context"] = json!({"includeDeclaration": false});
    params
}

/// A copy of the fixture for test NAME with a function `f`, its entry, and a
/// model `uses_f` that calls it.
fn with_a_function(name: &str) -> PathBuf {
    let project = copy_fixture(&format!("{name}-project"));
    std::fs::create_dir_all(project.join("functions")).unwrap();
    std::fs::write(project.join("functions/f.sql"), "x * 2").unwrap();
    std::fs::write(project.join("functions/_f.yml"), "functions:\n  - name: f\n").unwrap();
    std::fs::write(project.join("models/marts/uses_f.sql"), "select {{ function('f') }}(1) as v").unwrap();
    project
}

#[test]
fn a_function_call_leads_to_its_file_and_back() {
    let project = with_a_function("function-alone");
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with("function-alone", json!({}), &flags, None);
    session.initialize(&project);
    let on_f = position_in(&project.join("models/marts/uses_f.sql"), "function('f')", 10);
    let found = session.request("textDocument/definition", on_f.clone());
    assert_eq!(found["result"], location(&project, "functions/f.sql", 0, 0, 0), "{found}");
    let mut params = on_f;
    params["context"] = json!({"includeDeclaration": false});
    let used = session.request("textDocument/references", params);
    let name = "select {{ function('f') }}".find("'f'").unwrap() as u64 + 1;
    let at_name = location(&project, "models/marts/uses_f.sql", 0, name, name + 1);
    assert_eq!(used["result"], json!([at_name]), "{used}");
    assert!(session.finish().success());
}

#[test]
fn a_ref_in_a_python_model_leads_to_its_model_and_back() {
    let python = "def model(dbt, session):\n    return dbt.ref('stg_orders')\n";
    let project = copy_fixture("python-ref-project");
    std::fs::write(project.join("models/marts/py_orders.py"), python).unwrap();
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with("python-ref", json!({}), &flags, None);
    session.initialize(&project);
    let on_ref = position_in(&project.join("models/marts/py_orders.py"), "ref('stg_orders')", 6);
    let found = session.request("textDocument/definition", on_ref.clone());
    let mut params = on_ref;
    params["context"] = json!({"includeDeclaration": false});
    let used = session.request("textDocument/references", params);
    assert_eq!(found["result"], location(&project, "models/staging/stg_orders.sql", 0, 0, 0), "{found}");
    let files: Vec<&str> = used["result"].as_array().unwrap().iter().map(|l| l["uri"].as_str().unwrap()).collect();
    let uses = ["models/marts/orders.sql", "models/marts/py_orders.py", "snapshots/orders_snapshot.sql"];
    assert_eq!(files, uses.map(|file| uri(&project.join(file))), "{used}");
    assert!(session.finish().success());
}

#[test]
fn references_where_a_position_names_nothing_are_empty() {
    let (mut session, _) = Session::fake("alone-references-nothing", json!({}));
    session.initialize(&fixture());
    // The first `select` of `orders.sql`.
    let nowhere = without_declaration(position_in(&fixture().join("models/marts/orders.sql"), "select", 1));
    let answer = session.request("textDocument/references", nowhere);
    assert_eq!((answer.get("error"), &answer["result"]), (None, &json!([])), "{answer}");
    assert!(session.finish().success());
}

/// A copy of the fixture for test NAME with a model `dim` in two versions,
/// v1 the latest, and a properties file whose test refs its version 2; and
/// the manifest that dbt parses there.
fn versioned(name: &str) -> (PathBuf, Value) {
    let project = copy_fixture(&format!("{name}-project"));
    for version in [1, 2] {
        std::fs::write(project.join(format!("models/marts/dim_v{version}.sql")), "select 1 as id").unwrap();
    }
    let properties = "version: 2\nmodels:\n  - name: orders\n    data_tests:\n      - relationships:\n          \
                      to: ref('dim', v=2)\n          field: id\n";
    std::fs::write(project.join("models/marts/_dim.yml"), properties).unwrap();
    let mut manifest = manifest();
    for version in [1, 2] {
        let id = format!("model.shop.dim.v{version}");
        manifest["nodes"][&id] = json!({"unique_id": id, "name": "dim", "resource_type": "model",
            "package_name": "shop", "original_file_path": format!("models/marts/dim_v{version}.sql"),
            "version": version, "latest_version": 1});
    }
    (project, manifest)
}

#[test]
fn a_versioned_ref_in_yaml_leads_to_its_version_once_parsed() {
    let (project, manifest) = versioned("versioned-alone");
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with("versioned-alone", json!({"manifest": manifest}), &flags, None);
    session.initialize(&project);
    let on_ref = position_in(&project.join("models/marts/_dim.yml"), "ref('dim'", 5);
    // The first definition asks for the parse; the scan knows no versions meanwhile.
    assert_eq!(session.request("textDocument/definition", on_ref.clone())["result"], Value::Null);
    let v2 = location(&project, "models/marts/dim_v2.sql", 0, 0, 0);
    let found = session.poll(Duration::from_secs(10), ("textDocument/definition", on_ref), |result| *result == v2);
    assert_eq!(found, v2);
    assert!(session.finish().success());
}

#[test]
fn with_no_dbt_a_definition_is_found_or_null() {
    let project = navigable("definition-alone");
    let project_arg = project.to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project_arg];
    let mut session = Session::start(&command, &[], None);
    let result = session.initialize(&project);
    assert_eq!(result["capabilities"]["definitionProvider"], json!(true));
    let on_ref = position_in(&project.join("models/marts/_models.yml"), "ref('customers')", 6);
    let found = session.request("textDocument/definition", on_ref);
    assert_eq!(found["result"], location(&project, "models/marts/customers.sql", 0, 0, 0));
    let on_sql_ref = position_in(&project.join("models/marts/orders.sql"), "ref('stg_orders')", 6);
    let found = session.request("textDocument/definition", on_sql_ref);
    assert_eq!(found["result"], location(&project, "models/staging/stg_orders.sql", 0, 0, 0), "{found}");
    let nowhere =
        session.request("textDocument/definition", position_in(&project.join("models/marts/orders.sql"), "select", 1));
    assert_eq!((nowhere.get("error"), &nowhere["result"]), (None, &Value::Null), "{nowhere}");
    assert!(session.finish().success());
}

/// An inlay hint request on the whole of PATH.
fn hints_of(session: &mut Session, path: &Path) -> Value {
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 1000, "character": 0}});
    session.request("textDocument/inlayHint", json!({"textDocument": {"uri": uri(path)}, "range": range}))
}

#[test]
fn inlay_hints_follow_the_server_s_once_a_parse_has_finished() {
    let (mut session, log) = Session::fake("hints", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    let orders = fixture().join("models/marts/orders.sql");
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 1000, "character": 0}});
    let request = ("textDocument/inlayHint", json!({"textDocument": {"uri": uri(&orders)}, "range": range}));
    // No index yet: none, and a parse asked for by the hint request itself.
    let hints =
        session.poll(Duration::from_secs(10), request, |result| result.as_array().is_some_and(|h| !h.is_empty()));
    let relation = json!({"position": {"line": 4, "character": 38}, "label": "\"shop\".\"main\".\"stg_orders\"",
                          "kind": 1, "paddingLeft": true, "tooltip": "model.shop.stg_orders"});
    assert_eq!(hints, json!([relation]));
    // With an index, a hint request starts no parse.
    let parsed = parses(&log).len();
    hints_of(&mut session, &orders);
    parses_stay(&log, parsed);
    assert!(session.finish().success());
}

#[test]
fn hints_asked_before_the_first_compilation_come_by_a_refresh() {
    let gate = scratch("hints-early-gate").join("open");
    let script = json!({"manifest": manifest(), "parse_gate": gate});
    let (mut session, log) = Session::fake("hints-early", script);
    let capabilities = json!({"workspace": {"inlayHint": {"refreshSupport": true}}});
    session.request("initialize", json!({"processId": null, "rootUri": uri(&fixture()), "capabilities": capabilities}));
    session.notify("initialized", json!({}));
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    // On open, before the parse that the open started has ended, as clients ask.
    assert_eq!(hints_of(&mut session, &orders)["result"], json!([]));
    std::fs::write(&gate, "").unwrap();
    session.wait_for(|message| message["method"] == "workspace/inlayHint/refresh");
    assert_eq!(parses(&log).len(), 1);
    let hints = hints_of(&mut session, &orders);
    assert!(hints["result"].as_array().is_some_and(|hints| hints.iter().any(|h| h["kind"] == 1)), "{hints}");
    assert!(session.finish().success());
}

#[test]
fn a_parse_asks_a_client_that_can_refresh_its_hints_and_keeps_its_answer() {
    let (mut session, _) = Session::fake("hints-refresh", json!({"manifest": manifest()}));
    let capabilities = json!({"workspace": {"inlayHint": {"refreshSupport": true}}});
    session.request("initialize", json!({"processId": null, "rootUri": uri(&fixture()), "capabilities": capabilities}));
    session.notify("initialized", json!({}));
    parsed_orders(&mut session);
    // The session answers the proxy's request by itself.
    let refresh = session.wait_for(|message| message["method"] == "workspace/inlayHint/refresh");
    let id = refresh["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("dbt-lsp-plus/"), "{refresh}");
    session.request("workspace/symbol", json!({"query": "orders"}));
    let refreshes = session.received.iter().filter(|m| m["method"] == "workspace/inlayHint/refresh").count();
    assert_eq!(refreshes, 1, "one per parse");
    assert!(session.finish().success());
    // A client that cannot refresh is not asked.
    let (mut session, _) = Session::fake("hints-no-refresh", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    session.request("workspace/symbol", json!({"query": "orders"}));
    assert!(!session.received.iter().any(|message| message["method"] == "workspace/inlayHint/refresh"));
    assert!(session.finish().success());
}

#[test]
fn hints_come_from_the_open_text_of_sql_and_yaml() {
    let (mut session, _) = Session::fake("hints-open", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    // An unsaved line above the ref moves its hint.
    let orders = fixture().join("models/marts/orders.sql");
    let edit = json!({"textDocument": {"uri": uri(&orders), "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": "-- new\n"}]});
    session.notify("textDocument/didChange", edit);
    let hints = hints_of(&mut session, &orders);
    assert_eq!(hints["result"][0]["position"], json!({"line": 5, "character": 38}), "{hints}");
    // In YAML, a ref outside Jinja.
    let yml = fixture().join("models/staging/_sources.yml");
    let text = "version: 2\nmodels:\n  - name: x\n    data_tests:\n      - relationships:\n          arguments:\n            to: ref('stg_orders')\n";
    session.open(&yml, Some(text));
    let hints = hints_of(&mut session, &yml);
    assert_eq!(hints["result"][0]["position"], json!({"line": 6, "character": 33}), "{hints}");
    assert!(session.finish().success());
}

/// Writes the files of a run, RESULTS and MANIFEST, into PROJECT's target.
fn write_run(project: &Path, results: &Value, manifest: &Value) {
    let target = project.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("run_results.json"), results.to_string()).unwrap();
    std::fs::write(target.join("manifest.json"), manifest.to_string()).unwrap();
}

/// A manifest of the fixture's model `orders` and of a generic test on its
/// column `order_id`, written in `models/marts/_models.yml`.
fn run_manifest() -> Value {
    json!({"nodes": {
        "model.shop.orders": {"unique_id": "model.shop.orders", "resource_type": "model", "name": "orders",
                              "package_name": "shop", "original_file_path": "models/marts/orders.sql"},
        "test.shop.is_even_orders_order_id.a1": {"unique_id": "test.shop.is_even_orders_order_id.a1",
            "resource_type": "test", "name": "is_even_orders_order_id", "package_name": "shop",
            "original_file_path": "models/marts/_models.yml", "attached_node": "model.shop.orders",
            "column_name": "order_id", "test_metadata": {"name": "is_even"}}}})
}

/// The results of a run of COMMAND: the model `orders` erroring and its
/// test failing when FAILING, else both passing.
fn run_results(command: &str, failing: bool) -> Value {
    let (model, test) = if failing { ("error", "fail") } else { ("success", "pass") };
    json!({"args": {"command": command}, "results": [
        {"unique_id": "model.shop.orders", "status": model, "message": "Database Error in model orders",
         "failures": null},
        {"unique_id": "test.shop.is_even_orders_order_id.a1", "status": test, "message": "Error", "failures": 2}]})
}

const RUN_MODELS: &str = "version: 2\nmodels:\n  - name: orders\n    columns:\n      - name: order_id\n        \
                          data_tests:\n          - is_even\n";

/// A copy of the fixture for test NAME with `RUN_MODELS`, and a session with
/// the fake dbt playing SCRIPT.
fn run_session(name: &str, script: Value) -> (PathBuf, Session, PathBuf) {
    let project = copy_fixture(&format!("{name}-project"));
    std::fs::write(project.join("models/marts/_models.yml"), RUN_MODELS).unwrap();
    let (mut session, log) = Session::fake_with(name, script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    (project, session, log)
}

/// The next diagnostics published for PATH that PREDICATE accepts.
fn published(session: &mut Session, path: &Path, predicate: impl Fn(&Value) -> bool) -> Value {
    let uri = uri(path);
    let message = session.wait_for(|m| {
        m["method"] == "textDocument/publishDiagnostics"
            && m["params"]["uri"] == uri.as_str()
            && predicate(&m["params"])
    });
    message["params"].clone()
}

fn last_run(params: &Value) -> Vec<&Value> {
    params["diagnostics"].as_array().unwrap().iter().filter(|d| d["code"] == "last-run").collect()
}

#[test]
fn the_last_run_s_failures_show_where_they_are_written() {
    let (project, mut session, _) = run_session("runs", json!({}));
    write_run(&project, &run_results("build", true), &run_manifest());
    let model = published(&mut session, &project.join("models/marts/orders.sql"), |p| !last_run(p).is_empty());
    let error = last_run(&model)[0];
    assert_eq!(
        (&error["severity"], &error["message"], &error["source"]),
        (&json!(1), &json!("dbt build: Database Error in model orders"), &json!("dbt-lsp-plus"))
    );
    assert_eq!(error["range"]["start"], json!({"line": 0, "character": 0}));
    let test = published(&mut session, &project.join("models/marts/_models.yml"), |p| !last_run(p).is_empty());
    let failed = last_run(&test)[0];
    assert_eq!(failed["message"], "dbt build: is_even failed: 2 rows");
    assert_eq!(failed["range"], json!({"start": {"line": 6, "character": 12}, "end": {"line": 6, "character": 19}}));
    assert!(test.get("version").is_none());
    assert!(session.finish().success());
}

#[test]
fn a_change_of_uri_clears_the_one_published_before() {
    let project = copy_fixture("runs-spelling-project");
    std::fs::write(project.join("models/marts/_models.yml"), RUN_MODELS).unwrap();
    let link = project.with_file_name("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    // Published under the root's spelling, then opened under the resolved path.
    let resolved = project.join("models/marts/_models.yml");
    let spelt = link.join("models/marts/_models.yml");
    let (mut session, _) = Session::fake_with("runs-spelling", json!({}), &[Path::new("--project-dir"), &link], None);
    session.initialize(&link);
    write_run(&project, &run_results("build", true), &run_manifest());
    published(&mut session, &spelt, |p| !last_run(p).is_empty());
    session.open(&resolved, None);
    write_run(&project, &run_results("test", true), &run_manifest());
    published(&mut session, &resolved, |p| !last_run(p).is_empty());
    published(&mut session, &spelt, |p| p["diagnostics"] == json!([]));
    assert!(session.finish().success());
}

#[test]
fn a_new_run_replaces_the_failures_and_one_that_builds_nothing_keeps_them() {
    let (project, mut session, _) = run_session("runs-replaced", json!({}));
    let model = project.join("models/marts/orders.sql");
    write_run(&project, &run_results("build", true), &run_manifest());
    published(&mut session, &model, |p| !last_run(p).is_empty());
    // A compile writes results of nothing built: the failures stay.
    write_run(&project, &json!({"args": {"command": "compile"}, "results": []}), &run_manifest());
    std::thread::sleep(Duration::from_millis(2500));
    session.request("workspace/symbol", json!({"query": "orders"}));
    let cleared = |m: &Value| m["method"] == "textDocument/publishDiagnostics" && last_run(&m["params"]).is_empty();
    assert!(!session.received.iter().any(cleared), "a compile cleared the failures");
    // A partial run, of the test alone, replaces them all: the model's too.
    let partial = json!({"args": {"command": "test"}, "results": [
        {"unique_id": "test.shop.is_even_orders_order_id.a1", "status": "pass", "message": "Pass", "failures": 0}]});
    write_run(&project, &partial, &run_manifest());
    published(&mut session, &model, |p| last_run(p).is_empty());
    // Results gone, as after `dbt clean`: no failure left.
    write_run(&project, &run_results("build", true), &run_manifest());
    published(&mut session, &model, |p| !last_run(p).is_empty());
    std::fs::remove_file(project.join("target/run_results.json")).unwrap();
    published(&mut session, &model, |p| last_run(p).is_empty());
    assert!(session.finish().success());
}

#[test]
fn an_edit_or_a_save_of_a_file_clears_its_failures_and_an_open_keeps_them() {
    let (project, mut session, _) = run_session("runs-edited", json!({}));
    let (model, models) = (project.join("models/marts/orders.sql"), project.join("models/marts/_models.yml"));
    write_run(&project, &run_results("build", true), &run_manifest());
    published(&mut session, &model, |p| !last_run(p).is_empty());
    published(&mut session, &models, |p| !last_run(p).is_empty());
    session.open(&model, None);
    session.request("workspace/symbol", json!({"query": "orders"}));
    let cleared = |m: &&Value| m["method"] == "textDocument/publishDiagnostics" && last_run(&m["params"]).is_empty();
    assert!(!session.received.iter().any(|m| cleared(&m)), "opening the file cleared its failures");
    let edit = json!({"textDocument": {"uri": uri(&model), "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": "-- fixed\n"}]});
    session.notify("textDocument/didChange", edit);
    published(&mut session, &model, |p| last_run(p).is_empty());
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&models)}}));
    published(&mut session, &models, |p| last_run(p).is_empty());
    assert!(session.finish().success());
}

#[test]
fn a_file_newer_than_the_run_gets_no_failure_and_a_package_s_its_own() {
    let (project, mut session, _) = run_session("runs-stale", json!({}));
    let model = project.join("models/marts/orders.sql");
    let package = project.join("dbt_packages/shop_utils/models/pkg_broken.sql");
    std::fs::create_dir_all(package.parent().unwrap()).unwrap();
    std::fs::write(&package, "select * from nowhere\n").unwrap();
    let mut manifest = run_manifest();
    manifest["nodes"]["model.shop_utils.pkg_broken"] = json!({"unique_id": "model.shop_utils.pkg_broken",
        "resource_type": "model", "name": "pkg_broken", "package_name": "shop_utils",
        "original_file_path": "models/pkg_broken.sql"});
    let mut results = run_results("build", true);
    results["results"].as_array_mut().unwrap().push(json!({"unique_id": "model.shop_utils.pkg_broken",
        "status": "error", "message": "Database Error in model pkg_broken"}));
    // The model saved after the run: its failure no longer holds.
    let later = std::time::SystemTime::now() + Duration::from_secs(3600);
    std::fs::File::options().write(true).open(&model).unwrap().set_modified(later).unwrap();
    write_run(&project, &results, &manifest);
    published(&mut session, &project.join("models/marts/_models.yml"), |p| !last_run(p).is_empty());
    let broken = published(&mut session, &package, |p| !last_run(p).is_empty());
    assert_eq!(last_run(&broken)[0]["message"], "dbt build: Database Error in model pkg_broken");
    session.request("workspace/symbol", json!({"query": "orders"}));
    let on_model =
        |m: &Value| m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri(&model).as_str();
    assert!(!session.received.iter().any(on_model), "a failure on a file newer than the run");
    assert!(session.finish().success());
}

#[test]
fn no_failure_is_published_after_shutdown() {
    let (project, mut session, _) = run_session("runs-shutdown", json!({}));
    assert_eq!(session.request("shutdown", Value::Null)["result"], Value::Null);
    write_run(&project, &run_results("build", true), &run_manifest());
    std::thread::sleep(Duration::from_millis(2500));
    session.request("workspace/symbol", json!({"query": "orders"}));
    assert!(!session.received.iter().any(|m| m["method"] == "textDocument/publishDiagnostics"));
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
}

/// The results of `dbt source freshness` as 2.0.9 writes them, LOADED
/// seconds after the last load: `orders` past its warning and `customers`
/// past its error when STALE, else both fresh.
fn freshness(stale: bool, loaded: f64) -> Value {
    let unset = json!({"count": null, "period": null});
    let day = json!({"count": 1, "period": "day"});
    let result = |name: &str, status: &str, warn: &Value, error: &Value| {
        json!({
        "unique_id": format!("source.shop.raw.{name}"), "status": status, "max_loaded_at": "2018-01-02T00:00:00Z",
        "max_loaded_at_time_ago_in_s": loaded, "criteria": {"warn_after": warn, "error_after": error}})
    };
    let (orders, customers) = if stale { ("Warn", "Error") } else { ("Pass", "Pass") };
    json!({"results": [result("orders", orders, &day, &unset), result("customers", customers, &unset, &day)]})
}

#[test]
fn stale_sources_show_at_their_names_apart_from_the_last_run() {
    let (project, mut session, _) = run_session("stale", json!({}));
    let sources = project.join("models/staging/_sources.yml");
    let model = project.join("models/marts/orders.sql");
    let target = project.join("target");
    std::fs::create_dir_all(&target).unwrap();
    let mut manifest = run_manifest();
    let table = |name: &str| {
        json!({"unique_id": format!("source.shop.raw.{name}"), "resource_type": "source",
        "source_name": "raw", "name": name, "package_name": "shop",
        "original_file_path": "models/staging/_sources.yml"})
    };
    manifest["sources"] =
        json!({"source.shop.raw.orders": table("orders"), "source.shop.raw.customers": table("customers")});
    std::fs::write(target.join("manifest.json"), manifest.to_string()).unwrap();
    std::fs::write(target.join("sources.json"), freshness(true, 279_704_850.0).to_string()).unwrap();
    let stale = |params: &Value| params["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "stale-source");
    let both = published(&mut session, &sources, stale);
    let shown: Vec<(&Value, &Value, &Value)> = both["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (&d["severity"], &d["range"]["start"], &d["message"]))
        .collect();
    let message = |table: &str, word: &str| {
        json!(format!(
            "dbt source freshness: raw.{table} last loaded 2018-01-02T00:00:00Z, 3237 days ago ({word} after 1 day)"
        ))
    };
    let expected = [
        (&json!(2), &json!({"line": 6, "character": 14}), &message("orders", "warn")),
        (&json!(1), &json!({"line": 8, "character": 14}), &message("customers", "error")),
    ];
    assert_eq!(shown, expected);
    // A build replaces its own failures only: the stale tables stay.
    std::fs::write(target.join("run_results.json"), run_results("build", true).to_string()).unwrap();
    published(&mut session, &model, |p| !last_run(p).is_empty());
    session.request("workspace/symbol", json!({"query": "orders"}));
    let unstale = |m: &Value| {
        m["method"] == "textDocument/publishDiagnostics"
            && m["params"]["uri"] == uri(&sources).as_str()
            && !stale(&m["params"])
    };
    assert!(!session.received.iter().any(unstale), "a build cleared the stale tables");
    // A fresh check, which may rewrite run_results.json as a command that
    // builds nothing, clears them and leaves the build's failures.
    session.received.clear();
    let checked = json!({"args": {"command": "freshness"}, "results": []});
    std::fs::write(target.join("run_results.json"), checked.to_string()).unwrap();
    std::fs::write(target.join("sources.json"), freshness(false, 60.0).to_string()).unwrap();
    published(&mut session, &sources, |p| !stale(p));
    std::thread::sleep(Duration::from_millis(2500));
    session.request("workspace/symbol", json!({"query": "orders"}));
    let cleared =
        |m: &Value| m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri(&model).as_str();
    assert!(!session.received.iter().any(cleared), "a freshness check cleared the build's failures");
    // An edit of the sources' file drops its stale tables.
    std::fs::write(target.join("sources.json"), freshness(true, 279_704_850.0).to_string()).unwrap();
    published(&mut session, &sources, stale);
    session.open(&sources, None);
    session.received.clear();
    let edit = json!({"textDocument": {"uri": uri(&sources), "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": "# edited\n"}]});
    session.notify("textDocument/didChange", edit);
    published(&mut session, &sources, |p| !stale(p));
    assert!(session.finish().success());
}

#[test]
fn stale_sources_go_with_their_file() {
    let (project, mut session, _) = run_session("stale-gone", json!({}));
    let sources = project.join("models/staging/_sources.yml");
    let target = project.join("target");
    std::fs::create_dir_all(&target).unwrap();
    let mut manifest = run_manifest();
    manifest["sources"] = json!({"source.shop.raw.orders": {"unique_id": "source.shop.raw.orders",
        "resource_type": "source", "source_name": "raw", "name": "orders", "package_name": "shop",
        "original_file_path": "models/staging/_sources.yml"}});
    std::fs::write(target.join("manifest.json"), manifest.to_string()).unwrap();
    std::fs::write(target.join("sources.json"), freshness(true, 279_704_850.0).to_string()).unwrap();
    let stale = |params: &Value| params["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "stale-source");
    published(&mut session, &sources, stale);
    session.received.clear();
    std::fs::remove_file(target.join("sources.json")).unwrap();
    published(&mut session, &sources, |p| !stale(p));
    assert!(session.finish().success());
}
