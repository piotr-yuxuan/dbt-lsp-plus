// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy in front of a scripted fake of dbt lsp 2.0.9.

// Opt-in, ignored; in this crate so that one crate uses all of `support`.
#[path = "proxy/conformance.rs"]
mod conformance;
#[path = "proxy/live.rs"]
mod live;
mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use support::{Session, copy_fixture, fixture, received_by_server, scratch, until_logged, uri};

fn names(symbols: &Value) -> Vec<&str> {
    symbols.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect()
}

fn logged(log: &Path) -> Vec<Value> {
    received_by_server(log).iter().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

fn methods(log: &Path) -> Vec<String> {
    logged(log).iter().filter_map(|m| m["method"].as_str().map(str::to_owned)).collect()
}

/// The proxy's own listings that the fake received, by selector.
fn listings(log: &Path, selector: &str) -> usize {
    let own = |m: &Value| m["id"].as_str().is_some_and(|id| id.starts_with("dbt-lsp-plus/"));
    logged(log).iter().filter(|m| own(m) && m["params"]["arguments"][0] == selector).count()
}

/// What the fake's N-th process received, from 1: the processes of a
/// session share one log, each beginning with its `initialize`.
fn life(log: &Path, n: usize) -> Vec<Value> {
    let mut lives: Vec<Vec<Value>> = Vec::new();
    for message in logged(log) {
        if message["method"] == "initialize" {
            lives.push(Vec::new());
        }
        lives.last_mut().expect("initialize comes first").push(message);
    }
    lives.into_iter().nth(n - 1).unwrap_or_default()
}

/// The pids of the fake's processes, in the order they started.
fn lives(log: &Path) -> Vec<u32> {
    received_by_server(&log.with_extension("jsonl.lives")).iter().map(|pid| pid.parse().unwrap()).collect()
}

/// Waits until the fake's N-th process got its `initialized`.
fn until_initialized(log: &Path, n: usize) {
    until_logged(log, "a restarted server initialized", |_| life(log, n).iter().any(|m| m["method"] == "initialized"));
}

/// Whether the process PID runs, or waits to be reaped.
fn alive(pid: u32) -> bool {
    let mut probe = std::process::Command::new("kill");
    probe.args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null());
    probe.status().expect("run kill").success()
}

/// Sends a request that the server answers, so that everything the proxy
/// sent before it has reached the fake and its log once the answer is back.
fn barrier(session: &mut Session) {
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let position = json!({"textDocument": {"uri": orders}, "position": {"line": 0, "character": 0}});
    let hover = session.request("textDocument/hover", position);
    assert!(hover.get("result").is_some(), "{hover}");
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

/// The call of CALLS whose SIDE is the node ID.
fn call<'a>(calls: &'a Value, side: &str, id: &str) -> &'a Value {
    calls.as_array().unwrap().iter().find(|c| c[side]["detail"] == id).unwrap_or_else(|| panic!("no {id} in {calls}"))
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

const ALL: &str = "workspace/executeCommand dbt.listNodes *";
const SOURCES: &str = "workspace/executeCommand dbt.listNodes source:*";

#[test]
fn forwards_the_client_s_messages_byte_for_byte_and_adds_only_missing_capabilities() {
    let (mut session, log) = Session::fake("client-bytes", json!({}));
    let result = session.initialize(&fixture());
    let capabilities = &result["capabilities"];
    let added = ["documentSymbolProvider", "workspaceSymbolProvider", "callHierarchyProvider", "foldingRangeProvider"];
    for added in added.into_iter().chain(["documentHighlightProvider", "selectionRangeProvider"]) {
        assert_eq!(capabilities[added], json!(true), "{added}");
    }
    assert_eq!(capabilities["hoverProvider"], json!(true));
    assert_eq!(capabilities["executeCommandProvider"]["commands"][0], "dbt.listNodes");
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp");
    // Key order, spacing, a float's trailing zero and an unknown field survive; so does a body that is not JSON.
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let body = format!(
        r#"{{"params": {{"textDocument": {{"uri": "{orders}"}}, "position": {{"line": 4, "character": 25}}}}, "method":"textDocument/hover", "id": 7, "jsonrpc": "2.0", "x-extra": 1.50}}"#
    );
    let change = format!(
        r#"{{"params": {{"contentChanges": [ {{"text": "select 1"}} ], "textDocument": {{"version": 2, "uri": "{orders}"}}}},"jsonrpc":"2.0", "method": "textDocument/didChange"}}"#
    );
    session.send_raw(body.as_bytes());
    session.send_raw(change.as_bytes());
    session.send_raw(b"not json at all");
    let response = session.wait_for(|m| m["id"] == json!(7));
    assert_eq!(response["result"]["echo"], "textDocument/hover");
    until_logged(&log, "the bodies as sent", |lines| {
        lines.contains(&body) && lines.contains(&change) && lines.iter().any(|l| l == "not json at all")
    });
    assert!(session.finish().success());
}

#[test]
fn forwards_the_server_s_messages_byte_for_byte() {
    let answer = r#"{"result": {"z": 1, "a": 1.50},  "id": $ID, "jsonrpc": "2.0"}"#;
    let notification = r#"{"params": {"b": 2, "a": 1.50}, "method": "dbt/custom" , "jsonrpc": "2.0"}"#;
    let request = r#"{"params": {"token": "t", "a": [ ]}, "id": "s1", "method": "window/workDoneProgress/create", "jsonrpc":"2.0"}"#;
    let not_json = "not json from the server";
    let script = json!({"raw_answers": {"textDocument/hover": answer},
                        "raw_on_initialized": [request, not_json, notification]});
    let (mut session, _) = Session::fake("server-bytes", script);
    session.initialize(&fixture());
    session.wait_for(|m| m["method"] == "dbt/custom");
    for sent in [request, not_json] {
        assert!(session.received_raw.iter().any(|raw| raw == sent.as_bytes()), "the bytes of {sent} changed");
    }
    let position = json!({"textDocument": {"uri": "file:///x.sql"}, "position": {"line": 0, "character": 0}});
    let hover = session.request("textDocument/hover", position);
    let expected = answer.replace("$ID", &hover["id"].to_string());
    assert!(session.received_raw.iter().any(|raw| raw == expected.as_bytes()), "the answer's bytes changed");
    assert!(session.received_raw.iter().any(|raw| raw == notification.as_bytes()), "the notification's bytes changed");
    assert!(session.finish().success());
}

#[test]
fn relays_the_server_s_requests_and_the_client_s_answers_unchanged() {
    let (mut session, log) = Session::fake("relays", json!({}));
    session.auto_answer = false;
    session.initialize(&fixture());
    let configuration = session.wait_for(|m| m["method"] == "workspace/configuration");
    assert_eq!(configuration["id"], json!(1));
    let answer = r#"{"result": [ null ], "jsonrpc": "2.0", "id": 1}"#;
    session.send_raw(answer.as_bytes());
    until_logged(&log, "the client's answer as sent", |lines| lines.iter().any(|line| line == answer));
    assert!(session.finish().success());
}

#[test]
fn answers_the_outline_itself_and_follows_unsaved_edits_in_utf_16() {
    let (mut session, log) = Session::fake("outline", json!({}));
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
    assert!(!methods(&log).contains(&"textDocument/documentSymbol".to_owned()));
    until_logged(&log, "didChange", |lines| lines.iter().any(|line| line.contains("textDocument/didChange")));
    assert!(session.finish().success());
}

#[test]
fn outlines_a_properties_file_from_its_unsaved_text_and_no_other_yaml() {
    let (mut session, log) = Session::fake("yaml-outline", json!({}));
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
    assert!(!methods(&log).contains(&"textDocument/documentSymbol".to_owned()));
    assert!(session.finish().success());
}

#[test]
fn adopts_the_server_s_position_encoding() {
    let capabilities = json!({"textDocumentSync": {"change": 2, "openClose": true}, "positionEncoding": "utf-8"});
    let (mut session, _) = Session::fake("utf-8", json!({"capabilities": capabilities}));
    let result = session.initialize(&fixture());
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-8");
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    let edit = json!({"textDocument": {"uri": uri(&path), "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 11, "character": 0}, "end": {"line": 11, "character": 5}}, "text": "résultat"}]});
    session.notify("textDocument/didChange", edit);
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    // résultat is 8 characters and 9 UTF-8 bytes.
    assert_eq!(outline["result"][2]["selectionRange"]["end"], json!({"line": 11, "character": 9}));
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
fn leaves_to_the_server_a_feature_it_announces() {
    let capabilities = json!({"documentSymbolProvider": true, "callHierarchyProvider": {"workDoneProgress": false},
                              "textDocumentSync": 2});
    let (mut session, log) = Session::fake("announced", json!({"capabilities": capabilities}));
    let result = session.initialize(&fixture());
    assert_eq!(result["capabilities"]["callHierarchyProvider"], json!({"workDoneProgress": false}));
    assert_eq!(result["capabilities"]["workspaceSymbolProvider"], json!(true));
    let path = fixture().join("models/marts/orders.sql");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(outline["result"]["echo"], "textDocument/documentSymbol");
    assert!(methods(&log).contains(&"textDocument/documentSymbol".to_owned()));
    let position = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 4, "character": 25}});
    let prepared = session.request("textDocument/prepareCallHierarchy", position);
    assert_eq!(prepared["result"]["echo"], "textDocument/prepareCallHierarchy");
    assert!(session.finish().success());
}

#[test]
fn workspace_symbols_list_every_resource_with_its_package_and_id() {
    let (mut session, _) = Session::fake("workspace-symbols", json!({}));
    session.initialize(&fixture());
    let all = session.request("workspace/symbol", json!({"query": ""}));
    assert_eq!(all["result"].as_array().unwrap().len(), 22);
    let found = session.request("workspace/symbol", json!({"query": "ctd"}));
    assert_eq!(names(&found["result"]), ["cents_to_dollars"]);
    assert_eq!(found["result"][0]["containerName"], "shop");
    assert_eq!(found["result"][0]["data"], json!({"uniqueId": "macro.shop.cents_to_dollars", "resourceType": "macro"}));
    assert_eq!(found["result"][0]["location"]["uri"], uri(&fixture().join("macros/money.sql")));
    assert_eq!(found["result"][0]["location"]["range"]["start"], json!({"line": 0, "character": 9}));
    assert!(session.finish().success());
}

#[test]
fn call_hierarchy_follows_the_server_graph_through_the_proxy_s_own_requests() {
    let all = json!({"error": null, "nodes": [
        node("model.shop.order_summary", "models/marts/order_summary.sql", &["model.shop.orders"]),
        // The scan also finds orders -> stg_customers.
        node("model.shop.orders", "models/marts/orders.sql", &["model.shop.stg_orders"]),
        node("model.shop.stg_customers", "models/staging/stg_customers.sql", &["source.shop.raw.customers"])]});
    let sources =
        json!({"error": null, "nodes": [node("source.shop.raw.customers", "models/staging/_sources.yml", &[])]});
    let (mut session, log) = Session::fake("call-hierarchy", json!({"answers": {ALL: all, SOURCES: sources}}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    let orders = item("models/marts/orders.sql", "model.shop.orders");
    // Only the server knows that order_summary's ref(var(...)) names orders.
    let summary =
        |calls: &Value| calls.as_array().unwrap().iter().any(|c| c["from"]["detail"] == "model.shop.order_summary");
    let incoming =
        session.poll(Duration::from_secs(10), ("callHierarchy/incomingCalls", json!({"item": orders})), summary);
    // The nodes the server did not list keep their scanned edges.
    let callers = [
        "analysis.shop.order_counts",
        "model.shop.customers",
        "model.shop.order_summary",
        "test.shop.assert_positive_amount",
    ];
    assert_eq!(details(&incoming, "from"), callers);
    // The server's dependencies of orders replace the scanned ones; its macros stay.
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": orders}));
    assert_eq!(details(&outgoing["result"], "to"), ["macro.shop_utils.is_positive", "model.shop.stg_orders"]);
    let stg_orders = call(&outgoing["result"], "to", "model.shop.stg_orders");
    assert_eq!(stg_orders["fromRanges"][0]["start"], json!({"line": 4, "character": 21}));
    assert_eq!(stg_orders["to"]["data"], json!({"uniqueId": "model.shop.stg_orders", "resourceType": "model"}));
    let stg_customers = uri(&fixture().join("models/staging/stg_customers.sql"));
    let on_source = json!({"textDocument": {"uri": stg_customers}, "position": {"line": 5, "character": 10}});
    let prepared = session.request("textDocument/prepareCallHierarchy", on_source);
    assert_eq!(prepared["result"][0]["data"]["uniqueId"], "source.shop.raw.customers");
    assert_eq!(prepared["result"][0]["uri"], uri(&fixture().join("models/staging/_sources.yml")));
    // The proxy's requests carry its own ids, and their answers never reach the client.
    assert_eq!((listings(&log, "*"), listings(&log, "source:*")), (1, 1));
    assert!(!session.received.iter().any(|m| m["id"].as_str().is_some_and(|id| id.starts_with("dbt-lsp-plus/"))));
    assert!(session.finish().success());
}

#[test]
fn a_client_id_with_the_proxy_s_prefix_is_still_the_client_s() {
    let (mut session, _) = Session::fake("id-collision", json!({}));
    session.initialize(&fixture());
    session.send(&json!({"jsonrpc": "2.0", "id": "dbt-lsp-plus/1", "method": "textDocument/hover", "params": {}}));
    let response = session.wait_for(|m| m["id"] == "dbt-lsp-plus/1");
    assert_eq!(response["result"]["echo"], "textDocument/hover");
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
fn without_dbt_lsp_it_serves_its_own_features_alone() {
    let project = fixture().to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    let result = session.initialize(&fixture());
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus");
    assert_eq!(result["capabilities"]["documentSymbolProvider"], json!(true));
    assert_eq!(result["capabilities"]["textDocumentSync"], json!({"openClose": true, "change": 2}));
    let warning = session.wait_for(|m| m["method"] == "window/showMessage");
    assert!(warning["params"]["message"].as_str().unwrap().contains("could not start"), "{warning}");
    let path = fixture().join("models/marts/orders.sql");
    let position = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 4, "character": 25}});
    let hover = session.request("textDocument/hover", position);
    assert_eq!(hover["error"]["code"], json!(-32803));
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "final"]);
    // A document that is no file holds no resource.
    let document = json!({"uri": "untitled:Untitled-1", "languageId": "sql", "version": 1, "text": "select 1"});
    session.notify("textDocument/didOpen", json!({"textDocument": document}));
    let untitled = json!({"textDocument": {"uri": "untitled:Untitled-1"}, "position": {"line": 0, "character": 0}});
    let prepared = session.request("textDocument/prepareCallHierarchy", untitled);
    assert_eq!(prepared.get("result"), Some(&Value::Null), "{prepared}");
    assert_eq!(show_messages(&session), 1, "the proxy warns once");
    assert!(session.finish().success());
}

#[test]
fn a_dbt_without_lsp_is_served_alone_and_said_once_in_the_log() {
    let (mut session, log) = Session::fake("standalone", json!({"no_lsp": true}));
    let result = session.initialize(&fixture());
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus");
    let logged = session.wait_for(|m| m["method"] == "window/logMessage");
    assert!(logged["params"]["message"].as_str().unwrap().contains("has no lsp"), "{logged}");
    // Past what the client would get first.
    session.request("workspace/symbol", json!({"query": "summary"}));
    assert_eq!(show_messages(&session), 0, "no warning");
    let registered = session.received.iter().any(|m| m["method"] == "client/registerCapability");
    assert!(!registered, "a watcher, for a client that does not let a server register one");
    assert_eq!(session.received.iter().filter(|m| m["method"] == "window/logMessage").count(), 1);
    assert!(lives(&log).is_empty(), "a server started");
    assert!(session.finish().success());
}

#[test]
fn alone_the_proxy_s_own_features_answer() {
    let (mut session, _) = Session::fake("standalone-features", json!({"no_lsp": true}));
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
fn alone_the_graph_takes_dbt_s_resolution_from_the_manifest() {
    let mut manifest = manifest();
    let summary_node = node("model.shop.order_summary", "models/marts/order_summary.sql", &["model.shop.orders"]);
    manifest["nodes"]["model.shop.order_summary"] = summary_node;
    manifest["nodes"]["model.shop.orders"]["depends_on"] = json!({"nodes": ["model.shop.stg_orders"]});
    let gate = scratch("standalone-graph-gate").join("open");
    let script = json!({"no_lsp": true, "manifest": manifest, "parse_gate": gate});
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
fn with_dbt_lsp_the_graph_stays_the_listing_s_after_a_parse() {
    let all = json!({"error": null, "nodes": [
        node("model.shop.orders", "models/marts/orders.sql", &["model.shop.stg_orders"])]});
    let sources = json!({"error": null, "nodes": []});
    let mut manifest = manifest();
    manifest["nodes"]["model.shop.orders"]["depends_on"] = json!({"nodes": ["model.shop.stg_customers"]});
    let script = json!({"answers": {ALL: all, SOURCES: sources}, "manifest": manifest});
    let (mut session, _) = Session::fake("server-graph-after-parse", script);
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let orders = json!({"item": item("models/marts/orders.sql", "model.shop.orders")});
    // The scan finds stg_customers too, until the listing comes.
    let listed = |calls: &Value| {
        calls.as_array().is_some_and(|c| c.iter().all(|c| c["to"]["detail"] != "model.shop.stg_customers"))
    };
    let outgoing = session.poll(Duration::from_secs(10), ("callHierarchy/outgoingCalls", orders), listed);
    assert_eq!(details(&outgoing, "to"), ["macro.shop_utils.is_positive", "model.shop.stg_orders"]);
    assert!(session.finish().success());
}

#[test]
fn alone_the_proxy_watches_the_project_for_a_client_that_lets_it() {
    let (mut session, log) = Session::fake("standalone-watch", json!({"no_lsp": true, "manifest": manifest()}));
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
fn a_crash_fails_each_pending_request_exactly_once() {
    // The restarted server never answers: the proxy stays alone.
    let silent = ["textDocument/definition", "textDocument/completion"];
    let script = json!({"silent": silent, "exit_on": "textDocument/hover", "hang_on": "initialize", "hang_life": 2});
    let (mut session, _) = Session::fake("crash", script);
    session.initialize(&fixture());
    let position = json!({"textDocument": {"uri": "file:///x.sql"}, "position": {"line": 0, "character": 0}});
    for (id, method) in
        [(1001, "textDocument/definition"), (1002, "textDocument/completion"), (1003, "textDocument/hover")]
    {
        session.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": position}));
    }
    let error = session.wait_for(|m| m["method"] == "window/showMessage");
    assert_eq!(error["params"]["type"], json!(1));
    assert!(error["params"]["message"].as_str().unwrap().contains("exit status: 3"), "{error}");
    let symbols = session.request("workspace/symbol", json!({"query": "summary"}));
    assert_eq!(names(&symbols["result"]), ["order_summary"]);
    for id in [1001, 1002, 1003] {
        let answers: Vec<&Value> = session.received.iter().filter(|m| m["id"] == json!(id)).collect();
        assert_eq!(answers.len(), 1, "request {id}: {answers:?}");
        assert_eq!(answers[0]["error"]["code"], json!(-32803));
        assert!(answers[0]["error"]["message"].as_str().unwrap().contains("exited"));
    }
    assert!(session.finish().success());
}

#[test]
fn a_server_that_exits_before_initialize_leaves_the_proxy_alone() {
    let (mut session, log) = Session::fake("exit-before-initialize", json!({"exit_on": "initialize"}));
    let result = session.initialize(&fixture());
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus");
    let warning = session.wait_for(|m| m["method"] == "window/showMessage");
    assert_eq!(warning["params"]["type"], json!(2));
    assert!(warning["params"]["message"].as_str().unwrap().contains("exited"), "{warning}");
    let capabilities = |m: &&Value| m.get("result").is_some_and(|r| r.get("capabilities").is_some());
    assert_eq!(session.received.iter().filter(capabilities).count(), 1);
    let path = fixture().join("models/marts/orders.sql");
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "final"]);
    // Past the first restart's delay: none comes before the client's initialize is answered.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lives(&log).len(), 1);
    assert!(session.finish().success());
}

#[test]
fn a_crash_during_shutdown_still_answers_shutdown_and_sends_nothing_more() {
    let script = json!({"exit_on": "shutdown", "raw_on_initialized": [ANALYZING]});
    let (mut session, log) = Session::fake("crash-on-shutdown", script);
    session.initialize(&fixture());
    session.wait_for(|m| m["method"] == "$/progress");
    let response = session.request("shutdown", Value::Null);
    assert_eq!(response["result"], Value::Null);
    assert!(response.get("error").is_none(), "{response}");
    // Past the first restart's delay: none comes after shutdown.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lives(&log).len(), 1);
    // After shutdown, only answers: no message, no progress ended.
    let (status, unread) = session.exit();
    assert!(status.success());
    assert_eq!(unread, Vec::<Value>::new());
}

#[test]
fn exit_without_shutdown_fails() {
    let (mut session, _) = Session::fake("exit-early", json!({}));
    session.initialize(&fixture());
    session.notify("exit", Value::Null);
    assert_eq!(session.wait().code(), Some(1));
}

/// A hover on the first line of PATH, a comment in each model it names.
fn hover_at_start(path: &Path) -> Value {
    json!({"textDocument": {"uri": uri(path)}, "position": {"line": 0, "character": 0}})
}

/// A server that exits on a hover, then one that never answers its initialize.
fn restart_hangs() -> Value {
    json!({"exit_on": "textDocument/hover", "exit_life": 1, "hang_on": "initialize", "hang_life": 2})
}

#[test]
fn a_crashed_server_restarts_with_the_session_as_the_client_left_it() {
    let (mut session, log) = Session::fake("restart", json!({"exit_on": "textDocument/hover", "exit_life": 1}));
    session.initialize(&fixture());
    let [customers, summary, orders, stg_orders, stg_customers] = [
        "marts/customers.sql",
        "marts/order_summary.sql",
        "marts/orders.sql",
        "staging/stg_orders.sql",
        "staging/stg_customers.sql",
    ]
    .map(|p| fixture().join("models").join(p));
    // Opened in another order than their paths'.
    session.open(&stg_orders, None);
    session.open(&orders, None);
    let text = std::fs::read_to_string(&summary).unwrap();
    let summary_document = json!({"uri": uri(&summary), "languageId": "jinja-sql", "version": 7, "text": text});
    session.notify("textDocument/didOpen", json!({"textDocument": summary_document}));
    session.open(&customers, None);
    assert_eq!(session.request("textDocument/hover", hover_at_start(&orders))["error"]["code"], json!(-32803));
    let shown = session.wait_for(|m| m["method"] == "window/showMessage");
    let message = shown["params"]["message"].as_str().unwrap();
    assert!(message.starts_with("dbt lsp exited") && message.ends_with("restarting it in 1 s."), "{shown}");
    // During the delay: an edit, a document opened then closed, a request.
    let edit = json!({"textDocument": {"uri": uri(&orders), "version": 3}, "contentChanges": [{"text": "select 3\n"}]});
    session.notify("textDocument/didChange", edit);
    session.open(&stg_customers, None);
    session.notify("textDocument/didClose", json!({"textDocument": {"uri": uri(&stg_customers)}}));
    assert_eq!(session.request("textDocument/hover", hover_at_start(&summary))["error"]["code"], json!(-32803));
    until_initialized(&log, 2);
    let listed = |m: &Value| m["params"]["command"] == "dbt.listNodes";
    until_logged(&log, "the restarted server listed", |_| life(&log, 2).iter().any(listed));
    let hover = session.request("textDocument/hover", hover_at_start(&summary));
    assert_eq!(hover["result"]["echo"], "textDocument/hover", "{hover}");
    let (first, second) = (life(&log, 1), life(&log, 2));
    assert_eq!(second[0]["params"], first[0]["params"], "the client's own parameters");
    assert!(second[0]["id"].as_str().is_some_and(|id| id.starts_with("dbt-lsp-plus/")), "{}", second[0]);
    // Everything the proxy sends before its listing, the closed document left out.
    let methods = second.iter().map(|m| m["method"].as_str().unwrap_or("(an answer)"));
    let replayed: Vec<&str> = methods.take_while(|method| *method != "workspace/executeCommand").collect();
    let open = "textDocument/didOpen";
    assert_eq!(replayed, ["initialize", "initialized", open, open, open, open]);
    let on_disk = |path: &Path| {
        let text = std::fs::read_to_string(path).unwrap();
        json!({"uri": uri(path), "languageId": "sql", "version": 1, "text": text})
    };
    let orders_document = json!({"uri": uri(&orders), "languageId": "sql", "version": 3, "text": "select 3\n"});
    let opened: Vec<&Value> =
        second.iter().filter(|m| m["method"] == open).map(|m| &m["params"]["textDocument"]).collect();
    // In the order of their paths.
    assert_eq!(opened, [&on_disk(&customers), &summary_document, &orders_document, &on_disk(&stg_orders)]);
    assert!(session.finish().success());
}

#[test]
fn three_crashes_restart_after_one_two_then_three_seconds_and_a_fourth_leaves_the_proxy_alone() {
    let (mut session, log) = Session::fake("restarts", json!({"exit_on": "textDocument/hover"}));
    session.initialize(&fixture());
    let summary = fixture().join("models/marts/order_summary.sql");
    for delay in 1..=3 {
        session.received.clear();
        session.request("textDocument/hover", hover_at_start(&summary));
        let shown = session.wait_for(|m| m["method"] == "window/showMessage");
        let crashed = SystemTime::now();
        let message = shown["params"]["message"].as_str().unwrap();
        assert!(message.ends_with(&format!("restarting it in {delay} s.")), "{shown}");
        until_initialized(&log, delay as usize + 1);
        let started = std::fs::metadata(log.with_extension("jsonl.lives")).unwrap().modified().unwrap();
        let waited = started.duration_since(crashed).expect("a restart after the crash");
        // The client heard of the crash a little after the proxy did.
        let early = Duration::from_millis(50);
        assert!(waited + early >= Duration::from_secs(delay) && waited < Duration::from_secs(delay + 1), "{waited:?}");
    }
    session.received.clear();
    session.request("textDocument/hover", hover_at_start(&summary));
    let shown = session.wait_for(|m| m["method"] == "window/showMessage");
    assert!(shown["params"]["message"].as_str().unwrap().contains("keeps its own features"), "{shown}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lives(&log).len(), 4);
    // One message on giving up: everything sent before this answer is read.
    session.request("workspace/symbol", json!({"query": "summary"}));
    assert_eq!(show_messages(&session), 1);
    assert!(session.finish().success());
}

const WATCHER: &str = r#"{"jsonrpc": "2.0", "id": 0, "method": "client/registerCapability", "params": {"registrations":
[{"id": "default-dbt-file-system-watcher", "method": "workspace/didChangeWatchedFiles",
"registerOptions": {"watchers": [{"globPattern": "**/*"}]}}]}}"#;
const ANALYZING: &str =
    r#"{"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "analyzing", "value": {"kind": "begin"}}}"#;
const COMPILED: [&str; 2] = [
    r#"{"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "compiled", "value": {"kind": "begin"}}}"#,
    r#"{"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "compiled", "value": {"kind": "end"}}}"#,
];

#[test]
fn across_a_restart_the_client_gets_one_initialize_answer_one_registration_and_a_begun_progress_ended() {
    let raw = [WATCHER, ANALYZING, COMPILED[0], COMPILED[1]];
    let script = json!({"raw_on_initialized": raw, "exit_on": "textDocument/hover", "exit_life": 1});
    let (mut session, log) = Session::fake("restart-registration", script);
    session.initialize(&fixture());
    session.wait_for(|m| m["method"] == "$/progress");
    let summary = fixture().join("models/marts/order_summary.sql");
    session.request("textDocument/hover", hover_at_start(&summary));
    session.wait_for(|m| m["method"] == "window/showMessage");
    until_initialized(&log, 2);
    // The restarted server registered and began before it answers this.
    let hover = session.request("textDocument/hover", hover_at_start(&summary));
    assert_eq!(hover["result"]["echo"], "textDocument/hover", "{hover}");
    // The answer to the proxy's initialize stays with the proxy.
    let capabilities = |m: &&Value| m.get("result").is_some_and(|r| r.get("capabilities").is_some());
    assert_eq!(session.received.iter().filter(capabilities).count(), 1);
    let registrations = session.received.iter().filter(|m| m["method"] == "client/registerCapability").count();
    assert_eq!(registrations, 1);
    let answer = json!({"jsonrpc": "2.0", "id": 0, "result": null});
    assert!(life(&log, 2).contains(&answer), "the proxy answers the second registration");
    let progress = |m: &&Value| m["method"] == "$/progress" && m["params"]["token"] == "analyzing";
    let kinds: Vec<&Value> = session.received.iter().filter(progress).map(|m| &m["params"]["value"]["kind"]).collect();
    assert_eq!(kinds, ["begin", "end", "begin"]);
    // A progress the server ended is not ended again: one end of each life's.
    let ended = |m: &&Value| m["params"]["token"] == "compiled" && m["params"]["value"]["kind"] == "end";
    assert_eq!(session.received.iter().filter(ended).count(), 2);
    assert!(session.finish().success());
}

#[test]
fn a_restarted_server_that_never_answers_its_initialize_is_killed_and_replaced() {
    let (mut session, log) = Session::fake("restart-hangs", restart_hangs());
    session.initialize(&fixture());
    let summary = fixture().join("models/marts/order_summary.sql");
    session.request("textDocument/hover", hover_at_start(&summary));
    session.wait_for(|m| m["method"] == "window/showMessage");
    until_logged(&log, "the second life", |_| lives(&log).len() == 2);
    let started = std::fs::metadata(log.with_extension("jsonl.lives")).unwrap().modified().unwrap();
    // Meanwhile the proxy answers alone.
    assert_eq!(session.request("textDocument/hover", hover_at_start(&summary))["error"]["code"], json!(-32803));
    session.received.clear();
    session.timeout = Duration::from_secs(20);
    let shown = session.wait_for(|m| m["method"] == "window/showMessage");
    assert_eq!(shown["params"]["message"], "dbt lsp did not answer its initialize: restarting it in 2 s.");
    // Ten seconds after the second life started: not sooner, nor half a second later.
    let waited = SystemTime::now().duration_since(started).unwrap();
    assert!(
        waited + Duration::from_millis(100) >= Duration::from_secs(10) && waited < Duration::from_millis(10_500),
        "{waited:?}"
    );
    assert!(!alive(lives(&log)[1]), "the hung server is killed");
    until_initialized(&log, 3);
    let hover = session.request("textDocument/hover", hover_at_start(&summary));
    assert_eq!(hover["result"]["echo"], "textDocument/hover", "{hover}");
    assert!(session.finish().success());
}

#[test]
fn a_restart_that_cannot_start_counts_as_a_crash() {
    let dir = scratch("restart-cannot-start");
    let (link, script) = (dir.join("dbt"), dir.join("script.json"));
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fake-dbt-lsp"), &link).unwrap();
    std::fs::write(&script, json!({"exit_on": "textDocument/hover"}).to_string()).unwrap();
    let project = fixture().to_string_lossy().into_owned();
    let command = [link.to_string_lossy().into_owned(), "lsp".into(), "--project-dir".into(), project];
    let cache = dir.join("cache");
    let mut session = Session::start(&command, &[("FAKE_DBT_LSP_SCRIPT", &script), ("XDG_CACHE_HOME", &cache)], None);
    session.initialize(&fixture());
    // The server's executable gone, as after an upgrade.
    std::fs::remove_file(&link).unwrap();
    session.request("textDocument/hover", hover_at_start(&fixture().join("models/marts/order_summary.sql")));
    let shown = std::cell::Cell::new(0);
    session.wait_for(|m| {
        shown.set(shown.get() + usize::from(m["method"] == "window/showMessage"));
        shown.get() == 4
    });
    let messages: Vec<&str> = session.received.iter().filter_map(|m| m["params"]["message"].as_str()).collect();
    assert!(messages[1].starts_with("dbt lsp could not start") && messages[1].ends_with("in 2 s."), "{messages:?}");
    assert!(
        messages[2].ends_with("in 3 s.")
            && messages[3].ends_with("keeps its own features: symbols, lineage, folding, renames."),
        "{messages:?}"
    );
    assert!(session.finish().success());
}

#[test]
fn a_restarted_server_that_dies_before_answering_counts_as_a_crash() {
    let (mut session, log) = Session::fake("restart-dies", json!({"exit_on": "initialize", "exit_life": 2}));
    session.initialize(&fixture());
    let killed = std::process::Command::new("kill").args(["-KILL", &lives(&log)[0].to_string()]).status().unwrap();
    assert!(killed.success());
    session.wait_for(|m| m["method"] == "window/showMessage");
    session.received.clear();
    let crashed = Instant::now();
    let shown = session.wait_for(|m| m["method"] == "window/showMessage");
    assert_eq!(shown["params"]["message"], "dbt lsp exited (exit status: 3): restarting it in 2 s.");
    // At once, not at the replay's deadline.
    assert!(crashed.elapsed() < Duration::from_secs(5), "{:?}", crashed.elapsed());
    until_initialized(&log, 3);
    assert!(session.finish().success());
}

#[test]
fn a_crash_leaves_the_next_server_s_listing_all_its_retries() {
    let cancelled = json!({"error": "Operation cancelled", "grain": "unknown", "nodes": []});
    let script = json!({"hold": [ALL], "answers": {ALL: cancelled}, "exit_on": "textDocument/hover", "exit_life": 1});
    let (mut session, log) = Session::fake("restart-retries", script);
    session.initialize(&fixture());
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    let listed_in = |n: usize| life(&log, n).iter().filter(|m| m["params"]["arguments"][0] == "*").count();
    until_logged(&log, "the first listing", |_| listed_in(1) == 1);
    // One retry spent, then the server dies with the retry pending.
    session.notify("test/release", Value::Null);
    until_logged(&log, "a retry", |_| listed_in(1) == 2);
    session.request("textDocument/hover", hover_at_start(&orders));
    until_initialized(&log, 2);
    for n in 1..=4 {
        until_logged(&log, "the new server's listing or one of its three retries", |_| listed_in(2) == n);
        session.notify("test/release", Value::Null);
    }
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!(listed_in(2), 4);
    assert!(session.finish().success());
}

#[test]
fn shutdown_during_the_delay_leaves_no_restart() {
    let (mut session, log) = Session::fake("restart-shutdown", json!({"exit_on": "textDocument/hover"}));
    session.initialize(&fixture());
    session.request("textDocument/hover", hover_at_start(&fixture().join("models/marts/order_summary.sql")));
    session.wait_for(|m| m["method"] == "window/showMessage");
    assert_eq!(session.request("shutdown", Value::Null)["result"], Value::Null);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lives(&log).len(), 1);
    // Nothing after shutdown: no restart, no message.
    let (status, unread) = session.exit();
    assert!(status.success());
    assert_eq!(unread, Vec::<Value>::new());
}

#[test]
fn exit_during_a_restart_kills_the_starting_server() {
    let (mut session, log) = Session::fake("restart-exit", restart_hangs());
    session.initialize(&fixture());
    session.request("textDocument/hover", hover_at_start(&fixture().join("models/marts/order_summary.sql")));
    until_logged(&log, "the second life", |_| lives(&log).len() == 2);
    assert_eq!(session.request("shutdown", Value::Null)["result"], Value::Null);
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
    assert!(!alive(lives(&log)[1]));
}

#[test]
fn a_refused_restart_s_late_end_leaves_the_next_server_running() {
    // The second life refuses its initialize and leaves a process that holds
    // its output three seconds more: its frame comes during the delay, its
    // end after the third answered.
    let script = json!({"exit_on": "textDocument/hover", "exit_life": 1, "refuse_on": "initialize", "refuse_life": 2,
                        "orphan_on": "initialize", "orphan_life": 2});
    let (mut session, log) = Session::fake("restart-orphan", script);
    session.initialize(&fixture());
    let summary = fixture().join("models/marts/order_summary.sql");
    session.request("textDocument/hover", hover_at_start(&summary));
    session.wait_for(|m| m["method"] == "window/showMessage");
    session.received.clear();
    let refused = session.wait_for(|m| m["method"] == "window/showMessage");
    assert_eq!(refused["params"]["message"], "dbt lsp refused its initialize: restarting it in 2 s.");
    until_initialized(&log, 3);
    // Well past the orphan's end, which comes about a second from now.
    std::thread::sleep(Duration::from_millis(2500));
    let hover = session.request("textDocument/hover", hover_at_start(&summary));
    assert_eq!(hover["result"]["echo"], "textDocument/hover", "{hover}");
    let orphan = |m: &&Value| m["params"]["message"] == "from the orphan";
    assert_eq!(session.received.iter().find(orphan), None, "the frame of a process let go of");
    assert_eq!(show_messages(&session), 1);
    assert!(session.finish().success());
}

#[test]
fn a_refused_restart_ends_the_progress_it_began() {
    let begin =
        r#"{"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "replaying", "value": {"kind": "begin"}}}"#;
    let script = json!({"exit_on": "textDocument/hover", "exit_life": 1, "send_on": "initialize", "send_life": 2,
                        "send": [begin], "refuse_on": "initialize", "refuse_life": 2});
    let (mut session, _) = Session::fake("restart-refused-progress", script);
    session.initialize(&fixture());
    session.request("textDocument/hover", hover_at_start(&fixture().join("models/marts/order_summary.sql")));
    session.wait_for(|m| m["params"]["message"].as_str().is_some_and(|text| text.starts_with("dbt lsp refused")));
    // Begun before the refusal, ended as the proxy lets that server go.
    let replaying = |m: &&Value| m["params"]["token"] == "replaying";
    let kinds: Vec<&Value> = session.received.iter().filter(replaying).map(|m| &m["params"]["value"]["kind"]).collect();
    assert_eq!(kinds, ["begin", "end"]);
    assert!(session.finish().success());
}

#[test]
fn lists_on_the_client_s_changes_and_never_on_its_own_compilations() {
    let all = json!({"error": null, "nodes": [node("model.shop.orders", "models/marts/orders.sql", &[])]});
    let (mut session, log) = Session::fake("no-feedback", json!({"answers": {ALL: all}}));
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    until_logged(&log, "the first listing", |_| listings(&log, "source:*") == 1);
    // Each listing makes the fake compile, as dbt lsp 2.0.9 does; two round trips let a loop show.
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!((listings(&log, "*"), listings(&log, "source:*")), (1, 1));
    session.open(&fixture().join("models/marts/customers.sql"), None);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    until_logged(&log, "the second listing", |_| listings(&log, "source:*") == 2);
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!((listings(&log, "*"), listings(&log, "source:*")), (2, 2));
    // The server gets the change before the listing it prompts.
    let logged = logged(&log);
    let saved = logged.iter().position(|m| m["method"] == "textDocument/didSave").unwrap();
    let second = logged.iter().rposition(|m| m["params"]["arguments"][0] == "*").unwrap();
    assert!(saved < second, "didSave at {saved}, its listing at {second}");
    session.notify("workspace/didChangeWatchedFiles", json!({"changes": [{"uri": uri(&path), "type": 2}]}));
    until_logged(&log, "a listing after a watched change", |_| listings(&log, "source:*") == 3);
    assert!(session.finish().success());
}

/// dbt lsp 2.0.9 writes `logs/` and `target/` under its project at each
/// compilation, and a client that honours its watcher of `**/*` sends an
/// event for each write: a listing on such an event would never end.
#[test]
fn ignores_watched_changes_of_dbt_s_own_output() {
    let link = scratch("dbt-output-link").join("shop");
    std::os::unix::fs::symlink(fixture(), &link).unwrap();
    let all = json!({"error": null, "nodes": [node("model.shop.orders", "models/marts/orders.sql", &[])]});
    let (mut session, log) = Session::fake("dbt-output-events", json!({"answers": {ALL: all}, "manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    until_logged(&log, "the first listing", |_| listings(&log, "source:*") == 1);
    let raw = |paths: &[PathBuf]| {
        let changes: Vec<String> = paths.iter().map(|path| format!(r#"{{"uri":"{}","type":2}}"#, uri(path))).collect();
        format!(
            r#"{{"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{{"changes":[{}]}}}}"#,
            changes.join(",")
        )
    };
    let written = ["target", "target/compiled/shop/x.sql", "logs/dbt-lsp.log"].map(|path| fixture().join(path));
    // Through another spelling of the root: the directory itself, as the fixture has none to resolve below.
    let output = raw(&[&written[..], &[link.join("logs")]].concat());
    session.send_raw(output.as_bytes());
    until_logged(&log, "the event, byte for byte", |lines| lines.contains(&output));
    // A hover after a change of the graph would list, or parse again.
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!((listings(&log, "*"), listings(&log, "source:*")), (1, 1));
    parses_stay(&log, 1);
    let both = raw(&[fixture().join("models/marts/orders.sql"), fixture().join("logs/dbt-lsp.log")]);
    session.send_raw(both.as_bytes());
    until_logged(&log, "the mixed event, byte for byte", |lines| lines.contains(&both));
    until_logged(&log, "a listing after a model's change", |_| listings(&log, "source:*") == 2);
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!((listings(&log, "*"), listings(&log, "source:*")), (2, 2));
    assert!(session.finish().success());
}

/// lsp-mode watches a linked directory at its resolved path, and reports its
/// files there.
#[test]
fn ignores_dbt_s_output_through_a_link_out_of_the_project() {
    let project = copy_fixture("dbt-output-out-link");
    let elsewhere = project.with_file_name("elsewhere-logs");
    std::fs::create_dir_all(&elsewhere).unwrap();
    // As lsp-mode reports it, whatever links the scratch directory sits under.
    let elsewhere = std::fs::canonicalize(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, project.join("logs")).unwrap();
    let all = json!({"error": null, "nodes": [node("model.shop.orders", "models/marts/orders.sql", &[])]});
    let arguments = [Path::new("--project-dir"), &project];
    let (mut session, log) =
        Session::fake_with("dbt-output-out-link-session", json!({"answers": {ALL: all}}), &arguments, None);
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    until_logged(&log, "the first listing", |_| listings(&log, "source:*") == 1);
    let written = json!({"changes": [{"uri": uri(&elsewhere.join("dbt-lsp.log")), "type": 2}]});
    session.notify("workspace/didChangeWatchedFiles", written);
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!((listings(&log, "*"), listings(&log, "source:*")), (1, 1));
    assert!(session.finish().success());
}

#[test]
fn retries_a_cancelled_listing_at_most_three_times() {
    let cancelled = json!({"error": "Operation cancelled", "grain": "unknown", "nodes": []});
    let (mut session, log) = Session::fake("always-cancelled", json!({"answers": {ALL: cancelled}}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    until_logged(&log, "four listings", |_| listings(&log, "*") == 4);
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!(listings(&log, "*"), 4);
    assert!(session.finish().success());
}

#[test]
fn lists_again_when_another_listing_cancelled_the_proxy_s() {
    let cancelled = json!({"error": "Operation cancelled", "grain": "unknown", "nodes": []});
    let summary_node = node("model.shop.order_summary", "models/marts/order_summary.sql", &["model.shop.orders"]);
    let script = json!({"answers_once": {ALL: cancelled}, "answers": {ALL: {"error": null, "nodes": [summary_node]}}});
    let (mut session, _) = Session::fake("cancelled-once", script);
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/order_summary.sql"), None);
    let summary = item("models/marts/order_summary.sql", "model.shop.order_summary");
    let lists_orders = |calls: &Value| calls[0]["to"]["detail"] == "model.shop.orders";
    session.poll(Duration::from_secs(10), ("callHierarchy/outgoingCalls", json!({"item": summary})), lists_orders);
    assert!(session.finish().success());
}

#[test]
fn never_lists_while_a_listing_of_the_client_s_is_pending() {
    let held = "workspace/executeCommand dbt.listNodes +orders+";
    let (mut session, log) = Session::fake("client-listing", json!({"hold": [held]}));
    session.initialize(&fixture());
    let params = json!({"command": "dbt.listNodes", "arguments": ["+orders+"]});
    session.send(&json!({"jsonrpc": "2.0", "id": 77, "method": "workspace/executeCommand", "params": params}));
    session.open(&fixture().join("models/marts/orders.sql"), None);
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!(listings(&log, "*"), 0, "the proxy's listing would cancel the client's");
    session.notify("test/release", json!({}));
    let answer = session.wait_for(|m| m["id"] == json!(77));
    assert_ne!(answer["result"]["error"], "Operation cancelled");
    until_logged(&log, "the deferred listing", |_| listings(&log, "*") == 1);
    assert!(session.finish().success());
}

/// The listing of `*` in which order_summary depends on orders, which only
/// the server can tell.
fn summary_listing() -> Value {
    let summary_node = node("model.shop.order_summary", "models/marts/order_summary.sql", &["model.shop.orders"]);
    json!({"error": null, "nodes": [summary_node]})
}

/// Lists once, then answers the listings after a save with LATER, by key;
/// the edge order_summary -> orders, which only the first listing gives,
/// must stay once the fake has received LISTED of them.
fn keeps_the_graph_after(name: &str, later: Value, listed: (&str, usize)) {
    let first = json!({ALL: summary_listing(), SOURCES: {"error": null, "nodes": []}});
    let (mut session, log) = Session::fake(name, json!({"answers_once": first, "answers": later}));
    session.initialize(&fixture());
    let path = fixture().join("models/marts/order_summary.sql");
    session.open(&path, None);
    until_logged(&log, "the first listing", |_| listings(&log, "source:*") == 1);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    until_logged(&log, "the later listings", |_| listings(&log, listed.0) == listed.1);
    barrier(&mut session);
    let summary = item("models/marts/order_summary.sql", "model.shop.order_summary");
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": summary}));
    assert_eq!(details(&outgoing["result"], "to"), ["model.shop.orders"]);
    assert!(session.finish().success());
}

const FAILED: &str = r#"{"error": "Compilation failed", "error_kind": "compilation_failed", "nodes": []}"#;

#[test]
fn a_failed_listing_keeps_the_graph_it_had() {
    let failed: Value = serde_json::from_str(FAILED).unwrap();
    keeps_the_graph_after("failed-all", json!({ALL: failed}), ("*", 2));
    keeps_the_graph_after("failed-sources", json!({ALL: summary_listing(), SOURCES: failed}), ("source:*", 2));
}

#[test]
fn a_listing_cancelled_too_often_keeps_the_graph_it_had() {
    let cancelled = json!({"error": "Operation cancelled", "grain": "unknown", "nodes": []});
    // One listing, then four tries after the save: the first and its three retries.
    keeps_the_graph_after("cancelled-later", json!({ALL: cancelled}), ("*", 5));
}

#[test]
fn an_empty_first_listing_counts_as_failed_and_leaves_the_scanned_graph() {
    let (mut session, log) = Session::fake("empty-first", json!({}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    until_logged(&log, "a listing", |_| listings(&log, "*") == 1);
    barrier(&mut session);
    assert_eq!(listings(&log, "source:*"), 0, "an empty `*` ends the listing");
    let orders = item("models/marts/orders.sql", "model.shop.orders");
    let incoming = session.request("callHierarchy/incomingCalls", json!({"item": orders}));
    let expected = ["analysis.shop.order_counts", "model.shop.customers", "test.shop.assert_positive_amount"];
    assert_eq!(details(&incoming["result"], "from"), expected);
    assert!(session.finish().success());
}

#[test]
fn only_the_first_document_opened_prompts_a_listing() {
    let (mut session, log) = Session::fake("first-open", json!({}));
    session.initialize(&fixture());
    for path in ["models/marts/orders.sql", "models/marts/customers.sql", "models/staging/stg_orders.sql"] {
        session.open(&fixture().join(path), None);
    }
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!(listings(&log, "*"), 1);
    assert!(session.finish().success());
}

#[test]
fn a_failed_first_listing_leaves_the_scanned_graph() {
    let failed = json!({"error": "Compilation failed", "error_kind": "compilation_failed", "nodes": []});
    let (mut session, log) = Session::fake("failed-first", json!({"answers": {ALL: failed}}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    until_logged(&log, "a listing", |_| listings(&log, "*") == 1);
    barrier(&mut session);
    let orders = item("models/marts/orders.sql", "model.shop.orders");
    let incoming = session.request("callHierarchy/incomingCalls", json!({"item": orders}));
    let expected = ["analysis.shop.order_counts", "model.shop.customers", "test.shop.assert_positive_amount"];
    assert_eq!(details(&incoming["result"], "from"), expected);
    assert!(session.finish().success());
}

/// The position of `page_views` in `stg_page_views.sql`, with that file's URI.
fn on_page_views() -> Value {
    let path = fixture().join("models/staging/stg_page_views.sql");
    json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 0, "character": 37}})
}

#[test]
fn answers_references_on_a_source_and_forwards_the_others() {
    let (mut session, log) = Session::fake("source-references", json!({}));
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
    assert!(!methods(&log).contains(&"textDocument/references".to_owned()));
    // On a ref, the server answers.
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}, "context": {"includeDeclaration": true}});
    let forwarded = session.request("textDocument/references", on_ref.clone());
    assert_eq!(forwarded["result"]["echo"], "textDocument/references");
    let mut rename_ref = on_ref;
    rename_ref["newName"] = json!("orders_v2");
    let forwarded = session.request("textDocument/rename", rename_ref);
    assert_eq!(forwarded["result"]["echo"], "textDocument/rename");
    assert!(methods(&log).contains(&"textDocument/rename".to_owned()));
    assert!(session.finish().success());
}

#[test]
fn renames_a_source_table_and_keeps_its_relation() {
    let (mut session, log) = Session::fake("source-rename", json!({}));
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
    assert!(!methods(&log).contains(&"textDocument/rename".to_owned()));
    let mut refused = on_page_views();
    refused["newName"] = json!("it's");
    let error = session.request("textDocument/rename", refused);
    assert_eq!(error["error"]["code"], json!(-32803));
    assert!(error["error"]["message"].as_str().unwrap().contains("cannot hold"), "{error}");
    assert!(session.finish().success());
}

#[test]
fn alone_it_still_answers_on_sources() {
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
fn after_a_crash_the_scan_gives_the_graph() {
    let summary_node = node("model.shop.order_summary", "models/marts/order_summary.sql", &["model.shop.orders"]);
    // The restarted server never answers: the proxy stays alone.
    let answers = json!({ALL: {"error": null, "nodes": [summary_node]}});
    let script = json!({"answers": answers, "exit_on": "textDocument/hover", "hang_on": "initialize", "hang_life": 2});
    let (mut session, _) = Session::fake("crash-then-scan", script);
    session.initialize(&fixture());
    let path = fixture().join("models/marts/order_summary.sql");
    session.open(&path, None);
    let summary = item("models/marts/order_summary.sql", "model.shop.order_summary");
    let lists_orders = |calls: &Value| calls.as_array().is_some_and(|calls| !calls.is_empty());
    session.poll(Duration::from_secs(10), ("callHierarchy/outgoingCalls", json!({"item": summary})), lists_orders);
    let position = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 0, "character": 0}});
    assert_eq!(session.request("textDocument/hover", position)["error"]["code"], json!(-32803));
    let change = json!({"textDocument": {"uri": uri(&path), "version": 2},
                        "contentChanges": [{"text": "select * from {{ ref('stg_customers') }}"}]});
    session.notify("textDocument/didChange", change);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": summary}));
    assert_eq!(details(&outgoing["result"], "to"), ["model.shop.stg_customers"]);
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

/// A copy of the fixture for test NAME, and a session on it in front of
/// the fake; the client watches no file, so that nothing tells the proxy
/// of a change on disk.
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
fn without_project_dir_nor_a_project_folder_the_working_directory_is_the_root() {
    let (mut session, _) = Session::fake_with("working-directory", json!({}), &[], Some(&fixture()));
    session.initialize(&fixture().join("models"));
    let found = session.request("workspace/symbol", json!({"query": "stg_orders"}));
    assert_eq!(found["result"][0]["location"]["uri"], uri(&fixture().join("models/staging/stg_orders.sql")));
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
fn a_server_that_exits_at_once_is_reported_once_and_after_initialize() {
    let project = fixture().to_string_lossy().into_owned();
    let command = ["sh".into(), "-c".into(), "exit 2".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    std::thread::sleep(Duration::from_millis(300));
    let result = session.initialize(&fixture());
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus");
    let path = fixture().join("models/marts/orders.sql");
    session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert!(session.received[0].get("result").is_some(), "first received: {}", session.received[0]);
    assert_eq!(show_messages(&session), 1);
    assert!(session.finish().success());
}

#[test]
fn no_listing_follows_shutdown() {
    let all = json!({"error": null, "nodes": [node("model.shop.orders", "models/marts/orders.sql", &[])]});
    let (mut session, log) = Session::fake("after-shutdown", json!({"answers": {ALL: all}, "hold": [ALL]}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    until_logged(&log, "the held listing", |_| listings(&log, "*") == 1);
    assert_eq!(session.request("shutdown", Value::Null)["result"], Value::Null);
    session.notify("test/release", json!({}));
    // The fake compiles for the first didOpen, then around its answer to the
    // listing: once the second compilation is through, so is that answer.
    let compiled = std::cell::Cell::new(0);
    session.wait_for(|m| {
        compiled.set(compiled.get() + usize::from(m["method"] == "dbt/lspCompileComplete"));
        compiled.get() == 2
    });
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
    assert_eq!(listings(&log, "source:*"), 0);
}

/// The id of the proxy's first listing of `*` that the fake received.
fn listing_id(log: &Path) -> Value {
    logged(log).into_iter().find(|m| m["params"]["arguments"][0] == "*").expect("a listing")["id"].clone()
}

#[test]
fn the_proxy_never_reuses_the_id_of_a_pending_request_of_the_client_s() {
    let (mut session, log) = Session::fake("id-skip", json!({"hold": ["textDocument/definition"]}));
    session.initialize(&fixture());
    let position = json!({"textDocument": {"uri": "file:///x.sql"}, "position": {"line": 0, "character": 0}});
    let request =
        json!({"jsonrpc": "2.0", "id": "dbt-lsp-plus/1", "method": "textDocument/definition", "params": position});
    session.send(&request);
    session.open(&fixture().join("models/marts/orders.sql"), None);
    until_logged(&log, "the listing", |_| listings(&log, "*") == 1);
    assert_ne!(listing_id(&log), "dbt-lsp-plus/1");
    session.notify("test/release", json!({}));
    let answer = session.wait_for(|m| m["id"] == "dbt-lsp-plus/1");
    assert_eq!(answer["result"]["echo"], "textDocument/definition");
    assert!(session.finish().success());
}

#[test]
fn a_request_under_the_id_of_the_proxy_s_listing_in_flight_is_refused() {
    let (mut session, log) = Session::fake("id-in-flight", json!({"hold": [ALL]}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    until_logged(&log, "the held listing", |_| listings(&log, "*") == 1);
    let id = listing_id(&log);
    session.send(&json!({"jsonrpc": "2.0", "id": id, "method": "textDocument/hover", "params": {}}));
    let answer = session.wait_for(|m| m["id"] == id);
    assert_eq!(answer["error"]["code"], json!(-32600), "{answer}");
    session.notify("test/release", json!({}));
    barrier(&mut session);
    assert_eq!(logged(&log).iter().filter(|m| m["id"] == id).count(), 1, "the server got the id once");
    assert_eq!(session.received.iter().filter(|m| m["id"] == id).count(), 1, "the listing's answer is the proxy's");
    assert!(session.finish().success());
}

#[test]
fn a_server_that_ignores_exit_is_killed_after_its_grace() {
    let (mut session, _) = Session::fake("hangs", json!({"hang_on": "exit"}));
    session.initialize(&fixture());
    let started = Instant::now();
    assert!(session.finish().success());
    assert!(started.elapsed() < Duration::from_secs(4), "the proxy took {:?} to exit", started.elapsed());
}

#[test]
fn a_source_rename_on_a_file_the_scan_does_not_hold_goes_to_the_server_but_not_its_references() {
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
        assert_eq!(answer["result"]["echo"], "textDocument/rename", "{answer}");
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
    let (mut session, log) =
        Session::fake_with("source-bom-session", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    // The first letter of page_views, as an editor that leaves the mark out counts it.
    let at = json!({"textDocument": {"uri": uri(&model)}, "position": {"line": 0, "character": 35},
                    "context": {"includeDeclaration": false}});
    let found = session.request("textDocument/references", at);
    assert_eq!(found["result"].as_array().map(Vec::len), Some(1), "{found}");
    assert!(!methods(&log).contains(&"textDocument/references".to_owned()));
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

/// What `experimental.dbtLspPlus` says the proxy answers itself, with a
/// server or ALONE.
fn answered_itself(alone: bool) -> Value {
    let everywhere = json!(["sql", "yaml", "python"]);
    let mut definition = json!({"source": everywhere, "ref": ["yaml"], "doc": ["yaml"], "entry": ["yaml"],
                                "test": ["yaml"]});
    let mut references = json!({"source": everywhere});
    if alone {
        (definition["ref"], definition["macro"]) = (json!(["sql", "yaml"]), json!(["sql", "yaml"]));
        (references["ref"], references["macro"]) = (json!(["sql", "yaml"]), json!(["sql", "yaml"]));
    }
    json!({"version": 2, "definition": definition, "references": references,
           "rename": {"source": everywhere, "cte": ["sql"]}})
}

#[test]
fn tells_which_requests_it_answers_itself() {
    let answered = answered_itself(false);
    let capabilities = json!({"textDocumentSync": 2, "experimental": {"server": 1}});
    let (mut session, _) = Session::fake("experimental", json!({"capabilities": capabilities}));
    let result = session.initialize(&fixture());
    assert_eq!(result["capabilities"]["experimental"], json!({"server": 1, "dbtLspPlus": answered}));
    assert!(session.finish().success());
    let project = fixture().to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    let result = session.initialize(&fixture());
    assert_eq!(result["capabilities"]["experimental"]["dbtLspPlus"], answered);
    assert_eq!(result["capabilities"]["definitionProvider"], true);
    assert!(session.finish().success());
}

#[test]
fn source_positions_follow_the_server_s_encoding() {
    let capabilities = json!({"textDocumentSync": {"change": 2, "openClose": true}, "positionEncoding": "utf-8"});
    let (mut session, _) = Session::fake("source-utf-8", json!({"capabilities": capabilities}));
    session.initialize(&fixture());
    let model = fixture().join("models/staging/stg_page_views.sql");
    // é is one character and two UTF-8 bytes: page_views starts at character 43, byte 44.
    session.open(&model, Some("/* é */ select * from {{ source('events', 'page_views') }}"));
    let at = json!({"textDocument": {"uri": uri(&model)}, "position": {"line": 0, "character": 45},
                    "context": {"includeDeclaration": false}});
    let found = session.request("textDocument/references", at);
    let range = json!({"start": {"line": 0, "character": 44}, "end": {"line": 0, "character": 54}});
    assert_eq!(found["result"], json!([{"uri": uri(&model), "range": range}]));
    assert!(session.finish().success());
}

/// The position of `orders` in `from orders` in `orders.sql`, with that file's URI.
fn on_from_orders() -> Value {
    let path = fixture().join("models/marts/orders.sql");
    json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 17, "character": 11}})
}

#[test]
fn folds_highlights_and_selects_within_a_document() {
    let (mut session, log) = Session::fake("document-features", json!({}));
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
    for method in ["textDocument/foldingRange", "textDocument/documentHighlight", "textDocument/selectionRange"] {
        assert!(!methods(&log).contains(&method.to_owned()), "{method} reached the server");
    }
    assert!(session.finish().success());
}

#[test]
fn renames_a_cte_and_forwards_other_renames() {
    let (mut session, log) = Session::fake("cte-rename", json!({}));
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
    assert!(!methods(&log).contains(&"textDocument/rename".to_owned()));
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}, "newName": "x"});
    let forwarded = session.request("textDocument/rename", on_ref);
    assert_eq!(forwarded["result"]["echo"], "textDocument/rename");
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
fn a_yaml_document_holds_no_cte() {
    let (mut session, log) = Session::fake("yaml-cte", json!({}));
    session.initialize(&fixture());
    let path = fixture().join("models/staging/notes.yml");
    session.open(&path, Some("query: with x as (select 1) select * from x\n"));
    let at = json!({"textDocument": {"uri": uri(&path)}, "position": {"line": 0, "character": 12}, "newName": "y"});
    let forwarded = session.request("textDocument/rename", at.clone());
    assert_eq!(forwarded["result"]["echo"], "textDocument/rename", "{forwarded}");
    assert!(methods(&log).contains(&"textDocument/rename".to_owned()));
    let highlight = session.request("textDocument/documentHighlight", at);
    assert_eq!(highlight.get("result"), Some(&Value::Null), "{highlight}");
    assert!(session.finish().success());
}

#[test]
fn without_dbt_lsp_the_features_within_a_document_remain() {
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

/// Opens orders.sql, which makes the fake compile, then asks for the node
/// orders until a parse has given it; returns its information.
fn parsed_orders(session: &mut Session) -> Value {
    session.open(&fixture().join("models/marts/orders.sql"), None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object())
}

#[test]
fn node_info_parses_with_the_server_s_dbt_once_it_compiled() {
    let (mut session, log) = Session::fake("node-info", json!({"manifest": manifest()}));
    let result = session.initialize(&fixture());
    let commands = &result["capabilities"]["executeCommandProvider"]["commands"];
    assert!(
        commands.as_array().unwrap().iter().any(|c| c == "dbt-lsp-plus.nodeInfo") && commands[0] == "dbt.listNodes"
    );
    // Nothing compiled yet: dbt may not have resolved the packages.
    let early = node_info(&mut session, json!("model.shop.orders"));
    assert_eq!(early["error"]["code"], json!(-32803), "{early}");
    assert!(early["error"]["message"].as_str().unwrap().contains("waiting for dbt lsp to compile"), "{early}");
    // The compilation its listing makes then starts the parse it wanted.
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    let orders = session.poll(Duration::from_secs(10), request, |result| result.is_object());
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
fn node_info_by_id_needs_no_document_opened() {
    let (mut session, _) = Session::fake("node-info-alone", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    // The proxy's listing makes the server compile, as the first document would.
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    let orders = session.poll(Duration::from_secs(10), request, |result| result.is_object());
    assert_eq!(orders["unique_id"], "model.shop.orders");
    assert!(session.finish().success());
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
fn a_parse_is_cut_off_the_network_and_its_project_where_it_can_be() {
    let project = copy_fixture("parse-cut-project");
    let flags = [Path::new("--project-dir"), &project];
    let script = json!({"manifest": manifest(), "parse_write": true});
    let (mut session, log) = Session::fake_with("parse-cut", script, &flags, None);
    session.initialize(&project);
    parsed_orders(&mut session);
    // Linux can, CI's runner once its AppArmor allows it; elsewhere, dbt's
    // flags and the guard on packages alone keep it offline and harmless.
    let cut = dbt_lsp_plus::config::can_cut();
    assert!(cut || !cfg!(target_os = "linux"), "unshare -rnm cannot cut on this Linux");
    let parse = &parses(&log)[0];
    let expected = if cut { (json!(["lo"]), json!(false)) } else { (parse["interfaces"].clone(), json!(true)) };
    assert_eq!((parse["interfaces"].clone(), parse["wrote"].clone()), expected);
    assert!(session.finish().success());
}

#[test]
fn node_info_gives_the_summary_a_hover_shows() {
    let nothing = r#"{"jsonrpc":"2.0","id":$ID,"result":null}"#;
    let script = json!({"manifest": manifest(), "raw_answers": {"textDocument/hover": nothing}});
    let (mut session, _) = Session::fake("node-info-summary", script);
    session.initialize(&fixture());
    let orders = parsed_orders(&mut session);
    assert_eq!(orders["summary"], "model table, \"shop\".\"main\".\"orders\"");
    let file = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"line": 4, "character": 28});
    let info = node_info(&mut session, json!({"uri": file, "position": on_ref}));
    let hover = session.request("textDocument/hover", json!({"textDocument": {"uri": file}, "position": on_ref}));
    assert_eq!(info["result"]["summary"], hover["result"]["contents"]["value"], "{hover}");
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
fn one_parse_at_a_time_and_one_more_after_a_change_during_it() {
    let gate = scratch("parse-gate-file").join("open");
    let script = json!({"manifest": manifest(), "parse_gate": gate});
    let (mut session, log) = Session::fake("parse-once", script);
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    node_info(&mut session, json!("model.shop.orders"));
    until_logged(&log, "the first parse", |_| parses(&log).len() == 1);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    barrier(&mut session);
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 1);
    std::fs::write(&gate, "").unwrap();
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    // The change during the first parse makes the next request parse again.
    node_info(&mut session, json!("model.shop.orders"));
    until_logged(&log, "the second parse", |_| parses(&log).len() == 2);
    assert!(session.finish().success());
}

#[test]
fn with_dbt_lsp_a_change_during_a_parse_waits_for_a_request() {
    let gate = scratch("server-lazy-gate-file").join("open");
    let (mut session, log) = Session::fake("server-lazy", json!({"manifest": manifest(), "parse_gate": gate}));
    session.initialize_as(&fixture(), json!({"workspace": {"inlayHint": {"refreshSupport": true}}}));
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    node_info(&mut session, json!("model.shop.orders"));
    until_logged(&log, "the parse", |_| parses(&log).len() == 1);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    std::fs::write(&gate, "").unwrap();
    // The parse has ended once the client is asked to refresh its hints.
    session.wait_for(|m| m["method"] == "workspace/inlayHint/refresh");
    parses_stay(&log, 1);
    assert!(session.finish().success());
}

#[test]
fn alone_each_change_parses_and_one_more_after_a_change_during_a_parse() {
    let gate = scratch("standalone-gate-file").join("open");
    let script = json!({"no_lsp": true, "manifest": manifest(), "parse_gate": gate});
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

/// A line of dbt parse's JSON log, as both builds write it.
fn dbt_event(level: &str, code: &str, msg: &str) -> String {
    json!({"data": {}, "info": {"code": code, "level": level, "msg": msg, "name": "Event"}}).to_string() + "\n"
}

#[test]
fn alone_the_parse_s_problems_are_diagnostics_until_a_parse_without_them() {
    let log_file = scratch("standalone-problems-log").join("parse.jsonl");
    let completed = dbt_event("info", "Q039", "Finished 'parse'");
    let error = "[error] [DependencyNotFound (dbt1048)]: Ref 'nowhere' not found\n  --> models/marts/orders.sql:1:21";
    let in_package = "[warning] [W (dbt1060)]: a warning\n  --> macros/is_positive.sql";
    let nowhere = "[warning] [UnusedResourceConfigPath (dbt1097)]: Configuration paths exist";
    let problems = [dbt_event("error", "1048", error), dbt_event("warn", "1060", in_package)].concat();
    std::fs::write(&log_file, problems + &dbt_event("warn", "1097", nowhere) + &completed).unwrap();
    let script = json!({"no_lsp": true, "manifest": manifest(), "parse_log": log_file});
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
fn alone_a_broken_project_file_is_shown_and_a_problem_in_no_file_logged_once() {
    let project = copy_fixture("standalone-broken-project");
    // dbt still parses, and says where; the scan can no longer read it.
    let config = std::fs::read_to_string(project.join("dbt_project.yml")).unwrap();
    std::fs::write(project.join("dbt_project.yml"), config + "models: [\n").unwrap();
    let log_file = scratch("standalone-broken-project-log").join("parse.jsonl");
    let nowhere = dbt_event("warn", "1097", "[warning] [UnusedResourceConfigPath (dbt1097)]: Configuration paths");
    let at =
        |line: u32| dbt_event("error", "1013", &format!("[error] [YAML (dbt1013)]\n  --> dbt_project.yml:{line}:1"));
    let completed = dbt_event("info", "Q039", "Finished 'parse'");
    std::fs::write(&log_file, at(2) + &nowhere + &completed).unwrap();
    let flags = [Path::new("--project-dir"), &project];
    let script = json!({"no_lsp": true, "parse_log": log_file});
    let (mut session, log) = Session::fake_with("standalone-broken-project-session", script, &flags, None);
    session.initialize(&project);
    let model = project.join("models/marts/orders.sql");
    session.open(&model, None);
    // Without the cut, the guard on packages refuses a project file it
    // cannot read, as dbt may read it and install packages.
    let cut = dbt_lsp_plus::config::can_cut();
    assert!(cut || !cfg!(target_os = "linux"), "unshare -rnm cannot cut on this Linux");
    if !cut {
        let refused =
            |m: &Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("which dbt may read"));
        session.wait_for(refused);
        parses_stay(&log, 0);
        assert!(session.finish().success());
        return;
    }
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
fn alone_a_yaml_error_named_from_dbt_s_directory_joins_the_last_run_s_failures() {
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
    let script = json!({"no_lsp": true, "parse_log": log_file});
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
    let alone =
        published(&mut session, &models, |params| params["diagnostics"].as_array().is_some_and(|d| d.len() == 1));
    assert_eq!(alone["diagnostics"][0]["source"], "dbt parse");
    assert!(session.finish().success());
}

/// A copy of the fixture for test NAME with `RUN_MODELS`, and a session
/// alone in front of the fake, whose parses log ERROR at PLACE.
fn alone_with_a_problem(name: &str, place: &str) -> (PathBuf, Session) {
    let project = copy_fixture(&format!("{name}-project"));
    std::fs::write(project.join("models/marts/_models.yml"), RUN_MODELS).unwrap();
    let error = format!("[warning] [W (dbt1060)]: x\n  --> {place}");
    let log_file = scratch(&format!("{name}-log")).join("parse.jsonl");
    std::fs::write(&log_file, dbt_event("warn", "1060", &error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"no_lsp": true, "parse_log": log_file, "manifest": manifest()});
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with(name, script, &flags, None);
    session.initialize_as(&project, json!({"workspace": {"inlayHint": {"refreshSupport": true}}}));
    (project, session)
}

#[test]
fn alone_a_crlf_file_opened_with_lf_counts_as_saved() {
    let (project, mut session) = alone_with_a_problem("standalone-crlf", "models/marts/customers.sql:3:1");
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
fn alone_a_new_run_leaves_an_unsaved_document_s_ranges_to_the_client() {
    let (project, mut session) = alone_with_a_problem("standalone-run-unsaved", "models/marts/_models.yml:3:5");
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
fn alone_a_parse_cut_short_keeps_the_last_problems() {
    let log_file = scratch("standalone-cut-short-log").join("parse.jsonl");
    let error = |what: &str| {
        let message = format!("[error] [DependencyNotFound (dbt1048)]: {what}\n  --> models/marts/orders.sql:1:1");
        dbt_event("error", "1048", &message)
    };
    std::fs::write(&log_file, error("first") + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"no_lsp": true, "parse_log": log_file, "parse_exit": 1});
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

/// The URI under which the proxy alone publishes dbt's error at PLACE,
/// named from the working directory that DIRECTORY gives of a copy of the
/// fixture, as test NAME.
fn published_from(name: &str, directory: impl Fn(&Path) -> PathBuf, place: &str) -> (PathBuf, Value) {
    let project = copy_fixture(&format!("{name}-project"));
    let cwd = directory(&project);
    std::fs::create_dir_all(&cwd).unwrap();
    let error = format!("[error] [DependencyNotFound (dbt1048)]\n  --> {place}:1:1");
    let log_file = scratch(&format!("{name}-log")).join("parse.jsonl");
    std::fs::write(&log_file, dbt_event("error", "1048", &error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"no_lsp": true, "parse_log": log_file});
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
fn alone_a_problem_named_through_dot_dot_is_published_as_the_root_spells_it() {
    // From a sibling, dbt's path reads from the root too.
    let (project, sibling) =
        published_from("standalone-sibling", |p| p.with_file_name("elsewhere"), "../shop/models/marts/orders.sql");
    assert_eq!(sibling, uri(&project.join("models/marts/orders.sql")));
    // From within the project, only from dbt's working directory.
    let (project, within) = published_from("standalone-within", |p| p.join("sub"), "../models/marts/orders.sql");
    assert_eq!(within, uri(&project.join("models/marts/orders.sql")));
}

#[test]
fn alone_a_parse_leaves_an_unsaved_document_s_ranges_to_the_client() {
    let error = "[error] [DependencyNotFound (dbt1048)]: x\n  --> models/marts/customers.sql:3:1";
    let log_file = scratch("standalone-unsaved-log").join("parse.jsonl");
    std::fs::write(&log_file, dbt_event("error", "1048", error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script = json!({"no_lsp": true, "parse_log": log_file, "manifest": manifest()});
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
fn with_dbt_lsp_the_parse_s_problems_stay_its_own() {
    let log_file = scratch("server-problems-log").join("parse.jsonl");
    let error = "[error] [DependencyNotFound (dbt1048)]: Ref 'nowhere' not found\n  --> models/marts/orders.sql:1:1";
    std::fs::write(&log_file, dbt_event("error", "1048", error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let (mut session, _) = Session::fake("server-problems", json!({"manifest": manifest(), "parse_log": log_file}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    barrier(&mut session);
    let published = session.received.iter().filter(|m| m["method"] == "textDocument/publishDiagnostics").count();
    assert_eq!(published, 0, "dbt lsp publishes its own");
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
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    let refused = node_info(&mut session, json!("model.shop.orders"));
    assert!(refused["error"]["message"].as_str().unwrap().contains("run dbt deps"), "{refused}");
    node_info(&mut session, json!("model.shop.orders"));
    let logged_once = session.received.iter().filter(|m| m["method"] == "window/logMessage").count();
    assert_eq!(logged_once, 1);
    assert!(parses(&log).is_empty());
    assert!(session.finish().success());
}

#[test]
fn exit_stops_a_parse_under_way() {
    let (mut session, log) = Session::fake("parse-hangs", json!({"manifest": manifest(), "parse_hang": true}));
    session.initialize(&fixture());
    session.open(&fixture().join("models/marts/orders.sql"), None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
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
    let answer = r#"{"jsonrpc":"2.0","id":$ID,"result":{"contents":{"kind":"markdown","value":"Server text"}}}"#;
    let script = json!({"manifest": manifest(), "raw_answers": {"textDocument/hover": answer}});
    let (mut session, _) = Session::fake("hover-summary", script);
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let orders = uri(&fixture().join("models/marts/orders.sql"));
    let on_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}});
    let hover = session.request("textDocument/hover", on_ref);
    let value = hover["result"]["contents"]["value"].as_str().unwrap();
    assert_eq!(value, "model view, \"shop\".\"main\".\"stg_orders\"\n\n---\n\nServer text");
    let elsewhere = json!({"textDocument": {"uri": orders}, "position": {"line": 0, "character": 0}});
    let plain = session.request("textDocument/hover", elsewhere);
    let expected = answer.replace("$ID", &plain["id"].to_string());
    assert!(session.received_raw.iter().any(|raw| raw == expected.as_bytes()), "the answer's bytes changed");
    assert!(session.finish().success());
}

#[test]
fn without_dbt_lsp_node_info_says_why_it_has_nothing() {
    let project = fixture().to_string_lossy().into_owned();
    let command = ["/nonexistent/dbt".into(), "lsp".into(), "--project-dir".into(), project];
    let mut session = Session::start(&command, &[], None);
    session.initialize(&fixture());
    let answer = node_info(&mut session, json!("model.shop.orders"));
    assert_eq!(answer["error"]["code"], json!(-32803), "{answer}");
    assert!(session.finish().success());
}

#[test]
fn no_parse_after_shutdown_nor_after_a_crash() {
    let (mut session, log) = Session::fake("parse-shutdown", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let path = fixture().join("models/marts/orders.sql");
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    // An answer of the proxy's own, which asks for no index, as a hover would.
    session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(session.request("shutdown", Value::Null)["result"], Value::Null);
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 1);
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
    // The restarted server never answers: the proxy stays alone.
    let script =
        json!({"manifest": manifest(), "exit_on": "textDocument/definition", "hang_on": "initialize", "hang_life": 2});
    let (mut session, log) = Session::fake("parse-crash", script);
    session.initialize(&fixture());
    session.open(&path, None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    session.send(&json!({"jsonrpc": "2.0", "id": 900, "method": "textDocument/definition", "params": {}}));
    session.wait_for(|m| m["method"] == "window/showMessage");
    let alone = node_info(&mut session, json!("model.shop.orders"));
    assert_eq!(alone["error"]["code"], json!(-32803), "{alone}");
    parses_stay(&log, 0);
    assert!(session.finish().success());
}

#[test]
fn a_parse_wanted_before_a_compilation_does_not_start_after_shutdown() {
    let (mut session, log) =
        Session::fake("parse-wanted-shutdown", json!({"manifest": manifest(), "hold_compile": true}));
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    session.notify("test/compile", json!({}));
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    // A change, then a hover that wants the parse its compilation allows.
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    barrier(&mut session);
    assert_eq!(session.request("shutdown", Value::Null)["result"], Value::Null);
    session.notify("test/compile", json!({}));
    parses_stay(&log, 1);
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
}

#[test]
fn a_parse_waits_for_the_server_to_compile_after_a_change() {
    let (mut session, log) = Session::fake("parse-held", json!({"manifest": manifest(), "hold_compile": true}));
    session.initialize(&fixture());
    let path = fixture().join("models/marts/orders.sql");
    session.open(&path, None);
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 0);
    session.notify("test/compile", json!({}));
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request.clone(), |result| result.is_object());
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    node_info(&mut session, json!("model.shop.orders"));
    parses_stay(&log, 1);
    session.notify("test/compile", json!({}));
    barrier(&mut session);
    node_info(&mut session, json!("model.shop.orders"));
    until_logged(&log, "the parse after the compilation", |_| parses(&log).len() == 2);
    assert!(session.finish().success());
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
        barrier(session);
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
        let script = json!({"manifest": manifest(), "raw_answers": {"textDocument/hover": r#"{"jsonrpc":"2.0","id":$ID,"result":null}"#}});
        let (mut session, _) = Session::fake(&format!("hover-format-{kind}"), script);
        let capabilities = json!({"textDocument": {"hover": {"contentFormat": formats}}});
        let params = json!({"processId": null, "rootUri": uri(&fixture()), "capabilities": capabilities});
        session.request("initialize", params);
        session.notify("initialized", json!({}));
        parsed_orders(&mut session);
        let customers = uri(&fixture().join("models/staging/stg_customers.sql"));
        let on_source = json!({"textDocument": {"uri": customers}, "position": {"line": 5, "character": 23}});
        let hover = session.request("textDocument/hover", on_source);
        assert_eq!(hover["result"]["contents"], json!({"kind": kind, "value": "source, raw_customers"}), "{formats}");
        assert!(session.finish().success());
    }
}

#[test]
fn alone_hovers_and_hints_come_from_the_parse_and_saves_are_asked_for() {
    let (mut session, log) = Session::fake("standalone-hover", json!({"no_lsp": true, "manifest": manifest()}));
    let capabilities = session.initialize(&fixture())["capabilities"].clone();
    assert_eq!(capabilities["textDocumentSync"], json!({"openClose": true, "change": 2, "save": {}}));
    assert_eq!([&capabilities["hoverProvider"], &capabilities["inlayHintProvider"]], [&json!(true), &json!(true)]);
    let orders = fixture().join("models/marts/orders.sql");
    let on_ref = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    let early = session.request("textDocument/hover", on_ref.clone());
    assert_eq!((early.get("error"), &early["result"]), (None, &Value::Null), "before a parse: {early}");
    session.open(&orders, None);
    let hover = session.poll(Duration::from_secs(10), ("textDocument/hover", on_ref), |result| !result.is_null());
    let summary = json!({"kind": "plaintext", "value": "model view, \"shop\".\"main\".\"stg_orders\""});
    assert_eq!(hover["contents"], summary);
    let hints = hints_of(&mut session, &orders)["result"].clone();
    assert!(hints.as_array().unwrap().iter().any(|hint| hint["tooltip"] == "model.shop.stg_orders"), "{hints}");
    let elsewhere = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 0, "character": 0}});
    assert_eq!(session.request("textDocument/hover", elsewhere)["result"], Value::Null);
    // `ref(var(...))` names no node the scan can see: no hint.
    assert_eq!(hints_of(&mut session, &fixture().join("models/marts/order_summary.sql"))["result"], json!([]));
    assert!(lives(&log).is_empty(), "a server started");
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
fn a_definition_in_yaml_leads_where_dbt_looks() {
    let project = navigable("definition-yaml");
    let (mut session, log) =
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
    assert!(!methods(&log).contains(&"textDocument/definition".to_owned()));
    assert!(session.finish().success());
}

#[test]
fn a_definition_without_a_target_reaches_the_server() {
    let project = navigable("definition-forwarded");
    let (mut session, log) =
        Session::fake_with("definition-forwarded", json!({}), &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let yml = project.join("models/marts/_models.yml");
    let answers = [
        position_in(&yml, "customer_id", 2),
        position_in(&yml, "weekly", 2),
        position_in(&project.join("dbt_project.yml"), "summary_upstream", 2),
        position_in(&project.join("models/marts/orders.sql"), "ref('stg_orders')", 6),
    ];
    for params in answers {
        let answer = session.request("textDocument/definition", params.clone());
        assert_eq!(answer["result"], json!({"echo": "textDocument/definition", "params": params}));
    }
    assert_eq!(methods(&log).iter().filter(|method| *method == "textDocument/definition").count(), 4);
    // A source in SQL is the proxy's.
    let on_source = position_in(&project.join("models/staging/stg_customers.sql"), "'customers'", 2);
    let answer = session.request("textDocument/definition", on_source);
    assert_eq!(answer["result"], location(&project, "models/staging/_sources.yml", 8, 14, 23));
    assert!(session.finish().success());
}

#[test]
fn a_hover_in_yaml_shows_a_doc_s_text_and_a_test_s_signature() {
    let project = navigable("hover-yaml");
    let null_answer = r#"{"jsonrpc":"2.0","id":$ID,"result":null}"#;
    let (mut session, _) = Session::fake_with(
        "hover-yaml",
        json!({"raw_answers": {"textDocument/hover": null_answer}}),
        &[Path::new("--project-dir"), &project],
        None,
    );
    session.initialize(&project);
    let yml = project.join("models/marts/_models.yml");
    let on_doc = session.request("textDocument/hover", position_in(&yml, "order_status", 2));
    // In the format of a client that names none.
    let text = "The status of an order: placed, shipped or returned.";
    assert_eq!(on_doc["result"]["contents"], json!({"kind": "plaintext", "value": text}));
    let on_test = session.request("textDocument/hover", position_in(&yml, "- is_even", 4));
    let signature = "test is_even(model, column_name)\n\nin package shop";
    assert_eq!(on_test["result"]["contents"], json!({"kind": "plaintext", "value": signature}));
    // Elsewhere, the server's null passes.
    assert_eq!(session.request("textDocument/hover", position_in(&yml, "customer_id", 2))["result"], Value::Null);
    assert!(session.finish().success());
}

#[test]
fn an_entry_s_name_is_its_node_for_hover_and_node_info() {
    let project = navigable("hover-entry");
    let null_answer = r#"{"jsonrpc":"2.0","id":$ID,"result":null}"#;
    let script = json!({"manifest": manifest(), "raw_answers": {"textDocument/hover": null_answer}});
    let (mut session, _) = Session::fake_with("hover-entry", script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    session.open(&project.join("models/marts/orders.sql"), None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    let on_name = position_in(&project.join("models/marts/_models.yml"), "name: orders", 8);
    let hover = session.request("textDocument/hover", on_name.clone());
    assert_eq!(hover["result"]["contents"]["value"], "model table, \"shop\".\"main\".\"orders\"");
    let argument = json!({"uri": on_name["textDocument"]["uri"], "position": on_name["position"]});
    assert_eq!(node_info(&mut session, argument)["result"]["unique_id"], "model.shop.orders");
    // A source table's `name:` too.
    let on_table = position_in(&project.join("models/staging/_sources.yml"), "name: customers", 8);
    assert_eq!(session.request("textDocument/hover", on_table)["result"]["contents"]["value"], "source, raw_customers");
    assert!(session.finish().success());
}

#[test]
fn a_markdown_hover_keeps_a_doc_s_line_breaks_and_fences_a_signature() {
    let project = navigable("hover-markdown");
    let docs = "{% docs order_status %}\nThe status of an order:\nplaced, shipped or returned.\n{% enddocs %}\n";
    std::fs::write(project.join("models/docs.md"), docs).unwrap();
    let null_answer = r#"{"jsonrpc":"2.0","id":$ID,"result":null}"#;
    let script = json!({"raw_answers": {"textDocument/hover": null_answer}});
    let (mut session, _) = Session::fake_with("hover-markdown", script, &[Path::new("--project-dir"), &project], None);
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
fn alone_a_ref_and_a_macro_call_in_sql_lead_where_dbt_resolves_them() {
    let project = copy_fixture("sql-definitions-project");
    let package_models = project.join("dbt_packages/shop_utils/models");
    std::fs::create_dir_all(&package_models).unwrap();
    std::fs::write(package_models.join("orders.sql"), "select 1 as order_id").unwrap();
    std::fs::write(package_models.join("uses.sql"), "select * from {{ ref('orders') }}").unwrap();
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) = Session::fake_with("sql-definitions", json!({"no_lsp": true}), &flags, None);
    let result = session.initialize(&project);
    assert_eq!(result["capabilities"]["experimental"]["dbtLspPlus"], answered_itself(true));
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
fn alone_the_references_of_a_ref_and_a_macro_span_the_project() {
    let (mut session, _) = Session::fake("references-alone", json!({"no_lsp": true}));
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
    assert_eq!(at(&declared).len(), 3, "its file's start too: {declared}");
    let cents = references("macros/money.sql", "cents_to_dollars", 2, false);
    assert_eq!(at(&cents), [("models/staging/stg_orders.sql".to_owned(), 10, 11, 27)]);
    assert!(session.finish().success());
}

#[test]
fn with_dbt_lsp_the_references_of_a_ref_reach_the_server() {
    let (mut session, log) = Session::fake("references-server", json!({}));
    session.initialize(&fixture());
    let mut params = position_in(&fixture().join("models/marts/orders.sql"), "ref('stg_orders')", 6);
    params["context"] = json!({"includeDeclaration": false});
    let answer = session.request("textDocument/references", params.clone());
    assert_eq!(answer["result"], json!({"echo": "textDocument/references", "params": params}));
    assert_eq!(methods(&log).iter().filter(|method| *method == "textDocument/references").count(), 1);
    assert!(session.finish().success());
}

#[test]
fn with_dbt_lsp_a_ref_in_sql_reaches_the_server() {
    let (mut session, log) = Session::fake("sql-ref-server", json!({}));
    session.initialize(&fixture());
    let on_ref = position_in(&fixture().join("models/marts/orders.sql"), "ref('stg_orders')", 6);
    let answer = session.request("textDocument/definition", on_ref.clone());
    assert_eq!(answer["result"], json!({"echo": "textDocument/definition", "params": on_ref}));
    assert_eq!(methods(&log).iter().filter(|method| *method == "textDocument/definition").count(), 1);
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
fn alone_a_versioned_ref_in_yaml_leads_to_its_version_once_parsed() {
    let (project, manifest) = versioned("versioned-alone");
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, _) =
        Session::fake_with("versioned-alone", json!({"no_lsp": true, "manifest": manifest}), &flags, None);
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
fn with_dbt_lsp_a_versioned_ref_in_yaml_is_the_proxy_s() {
    let (project, _) = versioned("versioned-server");
    let flags = [Path::new("--project-dir"), &project];
    let (mut session, log) = Session::fake_with("versioned-server", json!({}), &flags, None);
    session.initialize(&project);
    let on_ref = position_in(&project.join("models/marts/_dim.yml"), "ref('dim'", 5);
    let answer = session.request("textDocument/definition", on_ref);
    assert_eq!((answer.get("error"), &answer["result"]), (None, &Value::Null), "{answer}");
    barrier(&mut session);
    assert!(!methods(&log).iter().any(|method| method == "textDocument/definition"), "forwarded");
    assert!(session.finish().success());
}

#[test]
fn alone_a_definition_is_the_proxy_s_or_null() {
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
    let null = session.request("textDocument/definition", on_sql_ref);
    assert_eq!((null.get("error"), &null["result"]), (None, &Value::Null), "{null}");
    assert!(session.finish().success());
}

/// An inlay hint request on the whole of PATH.
fn hints_of(session: &mut Session, path: &Path) -> Value {
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 1000, "character": 0}});
    session.request("textDocument/inlayHint", json!({"textDocument": {"uri": uri(path)}, "range": range}))
}

#[test]
fn inlay_hints_follow_the_server_s_once_a_parse_has_finished() {
    let server = r#"{"jsonrpc":"2.0","id":$ID,"result":[{"position":{"line":0,"character":0},"label":"server"}]}"#;
    let script = json!({"manifest": manifest(), "raw_answers": {"textDocument/inlayHint": server}});
    let (mut session, log) = Session::fake("hints", script);
    session.initialize(&fixture());
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    // No index yet: the server's hints alone, and a parse asked for by the
    // hint request itself.
    assert_eq!(hints_of(&mut session, &orders)["result"].as_array().map(Vec::len), Some(1));
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 1000, "character": 0}});
    let request = ("textDocument/inlayHint", json!({"textDocument": {"uri": uri(&orders)}, "range": range}));
    let hints = session.poll(Duration::from_secs(10), request, |result| result.as_array().is_some_and(|h| h.len() > 1));
    let relation = json!({"position": {"line": 4, "character": 38}, "label": "\"shop\".\"main\".\"stg_orders\"",
                          "kind": 1, "paddingLeft": true, "tooltip": "model.shop.stg_orders"});
    assert_eq!(hints, json!([{"position": {"line": 0, "character": 0}, "label": "server"}, relation]));
    // With an index, a hint request after a change and its compilation
    // starts no parse.
    let parsed = parses(&log).len();
    session.received.clear();
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&orders)}}));
    session.wait_for(|m| m["method"] == "dbt/lspCompileComplete");
    hints_of(&mut session, &orders);
    parses_stay(&log, parsed);
    assert!(session.finish().success());
}

#[test]
fn the_server_s_null_or_error_for_hints_passes_unless_the_proxy_has_some() {
    let null_answer = r#"{"jsonrpc":"2.0","id":$ID,"result":null}"#;
    let script = json!({"manifest": manifest(), "raw_answers": {"textDocument/inlayHint": null_answer}});
    let (mut session, _) = Session::fake("hints-null", script);
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let alone = hints_of(&mut session, &fixture().join("models/marts/orders.sql"));
    assert_eq!(alone["result"].as_array().map(Vec::len), Some(1), "{alone}");
    // `ref(var(...))` names no node the scan can see: the server's null, byte for byte.
    let summary = hints_of(&mut session, &fixture().join("models/marts/order_summary.sql"));
    let expected = null_answer.replace("$ID", &summary["id"].to_string());
    assert!(session.received_raw.iter().any(|raw| raw == expected.as_bytes()), "{summary}");
    assert!(session.finish().success());
    let error = r#"{"jsonrpc":"2.0","id":$ID,"error":{"code":-32603,"message":"no"}}"#;
    let script = json!({"manifest": manifest(), "raw_answers": {"textDocument/inlayHint": error}});
    let (mut session, _) = Session::fake("hints-error", script);
    session.initialize(&fixture());
    parsed_orders(&mut session);
    let failed = hints_of(&mut session, &fixture().join("models/marts/orders.sql"));
    let expected = error.replace("$ID", &failed["id"].to_string());
    assert!(session.received_raw.iter().any(|raw| raw == expected.as_bytes()), "{failed}");
    assert!(session.finish().success());
}

#[test]
fn hints_asked_before_the_first_compilation_come_by_a_refresh() {
    let script = json!({"manifest": manifest(), "hold_compile": true});
    let (mut session, log) = Session::fake("hints-early", script);
    let capabilities = json!({"workspace": {"inlayHint": {"refreshSupport": true}}});
    session.request("initialize", json!({"processId": null, "rootUri": uri(&fixture()), "capabilities": capabilities}));
    session.notify("initialized", json!({}));
    let orders = fixture().join("models/marts/orders.sql");
    session.open(&orders, None);
    // On open, before dbt lsp has compiled, as clients ask.
    hints_of(&mut session, &orders);
    parses_stay(&log, 0);
    session.notify("test/compile", json!({}));
    session.wait_for(|message| message["method"] == "workspace/inlayHint/refresh");
    assert_eq!(parses(&log).len(), 1);
    let hints = hints_of(&mut session, &orders);
    assert!(hints["result"].as_array().is_some_and(|hints| hints.iter().any(|h| h["kind"] == 1)), "{hints}");
    assert!(session.finish().success());
}

#[test]
fn a_parse_asks_a_client_that_can_refresh_its_hints_and_keeps_its_answer() {
    let (mut session, log) = Session::fake("hints-refresh", json!({"manifest": manifest()}));
    let capabilities = json!({"workspace": {"inlayHint": {"refreshSupport": true}}});
    session.request("initialize", json!({"processId": null, "rootUri": uri(&fixture()), "capabilities": capabilities}));
    session.notify("initialized", json!({}));
    parsed_orders(&mut session);
    // The session answers the proxy's request by itself.
    let refresh = session.wait_for(|message| message["method"] == "workspace/inlayHint/refresh");
    let id = refresh["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("dbt-lsp-plus/"), "{refresh}");
    barrier(&mut session);
    let refreshes = session.received.iter().filter(|m| m["method"] == "workspace/inlayHint/refresh").count();
    assert_eq!(refreshes, 1, "one per parse");
    assert!(!logged(&log).iter().any(|message| message["id"] == id.as_str()), "the client's answer reached the server");
    assert!(session.finish().success());
    // A client that cannot refresh is not asked.
    let (mut session, _) = Session::fake("hints-no-refresh", json!({"manifest": manifest()}));
    session.initialize(&fixture());
    parsed_orders(&mut session);
    barrier(&mut session);
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
    // In YAML, a ref outside Jinja; the server's null becomes the proxy's hint.
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

/// A copy of the fixture for test NAME with `RUN_MODELS`, and a session in
/// front of the fake with SCRIPT.
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
fn the_last_run_s_failures_join_the_server_s_and_leave_its_others_as_they_are() {
    let project = copy_fixture("runs-merged-project");
    let models = project.join("models/marts/_models.yml");
    let staging = project.join("models/staging/stg_orders.sql");
    let on_models = json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
        "uri": uri(&models), "diagnostics": [{"range": {"start": {"line": 0, "character": 0},
        "end": {"line": 0, "character": 0}}, "severity": 2, "source": "dbt", "message": "from dbt"}]}});
    // Keys out of order and spaces, which no serialiser of the proxy's writes.
    let on_staging = format!(
        concat!(
            r#"{{"params": {{"diagnostics": [ ], "uri": "{}"}}, "#,
            r#""method": "textDocument/publishDiagnostics" , "jsonrpc": "2.0"}}"#
        ),
        uri(&staging)
    );
    let script = json!({"raw_on_compile": [on_models.to_string(), on_staging]});
    std::fs::write(&models, RUN_MODELS).unwrap();
    let (mut session, _) = Session::fake_with("runs-merged", script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    // The server publishes first, the run comes after.
    session.notify("test/compile", json!({}));
    published(&mut session, &models, |p| p["diagnostics"].as_array().unwrap().len() == 1);
    write_run(&project, &run_results("build", true), &run_manifest());
    let merged = published(&mut session, &models, |p| p["diagnostics"].as_array().unwrap().len() == 2);
    assert_eq!(merged["diagnostics"][0]["message"], "from dbt");
    assert_eq!(merged["diagnostics"][1]["code"], "last-run");
    // A file without failures gets the server's notification byte for byte.
    session.notify("test/compile", json!({}));
    published(&mut session, &staging, |_| true);
    assert!(session.received_raw.iter().any(|raw| raw == on_staging.as_bytes()), "the server's bytes changed");
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
    barrier(&mut session);
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
    barrier(&mut session);
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
    barrier(&mut session);
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
    barrier(&mut session);
    assert!(!session.received.iter().any(|m| m["method"] == "textDocument/publishDiagnostics"));
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
}

#[test]
fn a_change_of_uri_clears_the_one_published_before() {
    let project = copy_fixture("runs-spelling-project");
    std::fs::write(project.join("models/marts/_models.yml"), RUN_MODELS).unwrap();
    let link = project.with_file_name("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    // The server names the file by its resolved path, the proxy by the root's spelling.
    let resolved = project.join("models/marts/_models.yml");
    let spelt = link.join("models/marts/_models.yml");
    let server = json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                        "params": {"uri": uri(&resolved), "diagnostics": []}});
    let script = json!({"raw_on_compile": [server.to_string()]});
    let (mut session, _) = Session::fake_with("runs-spelling", script, &[Path::new("--project-dir"), &link], None);
    session.initialize(&link);
    write_run(&project, &run_results("build", true), &run_manifest());
    published(&mut session, &spelt, |p| !last_run(p).is_empty());
    session.notify("test/compile", json!({}));
    published(&mut session, &spelt, |p| p["diagnostics"] == json!([]));
    published(&mut session, &resolved, |p| !last_run(p).is_empty());
    assert!(session.finish().success());
}

#[test]
fn an_edit_clears_failures_over_an_empty_server_set_and_waits_over_a_full_one() {
    let project = copy_fixture("runs-server-sets-project");
    let (model, models) = (project.join("models/marts/orders.sql"), project.join("models/marts/_models.yml"));
    std::fs::write(&models, RUN_MODELS).unwrap();
    let publication = |path: &Path, diagnostics: Value| {
        json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
               "params": {"uri": uri(path), "diagnostics": diagnostics}})
        .to_string()
    };
    let warning = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                          "severity": 2, "source": "dbt", "message": "from dbt"}]);
    let script = json!({"raw_on_compile": [publication(&model, json!([])), publication(&models, warning)]});
    let (mut session, _) =
        Session::fake_with("runs-server-sets", script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    session.notify("test/compile", json!({}));
    published(&mut session, &models, |p| p["diagnostics"].as_array().unwrap().len() == 1);
    write_run(&project, &run_results("build", true), &run_manifest());
    published(&mut session, &model, |p| !last_run(p).is_empty());
    published(&mut session, &models, |p| !last_run(p).is_empty());
    // Over the server's empty set, an edit clears the failure at once.
    let edit = |path: &Path| {
        json!({"textDocument": {"uri": uri(path), "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "text": "# fixed\n"}]})
    };
    session.open(&model, None);
    session.received.clear();
    session.notify("textDocument/didChange", edit(&model));
    published(&mut session, &model, |p| last_run(p).is_empty());
    // Over its warning, whose range the edit moved, the proxy waits for the
    // server's next publication, which then goes out without the failure.
    session.open(&models, None);
    session.received.clear();
    session.notify("textDocument/didChange", edit(&models));
    barrier(&mut session);
    let on_models =
        |m: &Value| m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri(&models).as_str();
    assert!(!session.received.iter().any(on_models), "the server's old ranges published again");
    session.notify("test/compile", json!({}));
    let next = published(&mut session, &models, |_| true);
    assert_eq!((last_run(&next).len(), next["diagnostics"].as_array().map(Vec::len)), (0, Some(1)));
    assert!(session.finish().success());
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
    barrier(&mut session);
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
    barrier(&mut session);
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
