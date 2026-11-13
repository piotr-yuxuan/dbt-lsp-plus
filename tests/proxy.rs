// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy in front of a scripted fake of dbt lsp 2.0.9.

// Opt-in, ignored; in this crate so that one crate uses all of `support`.
#[path = "proxy/live.rs"]
mod live;
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

fn methods(log: &Path) -> Vec<String> {
    logged(log).iter().filter_map(|m| m["method"].as_str().map(str::to_owned)).collect()
}

/// The proxy's own listings that the fake received, by selector.
fn listings(log: &Path, selector: &str) -> usize {
    let own = |m: &Value| m["id"].as_str().is_some_and(|id| id.starts_with("dbt-lsp-plus/"));
    logged(log).iter().filter(|m| own(m) && m["params"]["arguments"][0] == selector).count()
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
fn a_crash_fails_each_pending_request_exactly_once() {
    let script =
        json!({"silent": ["textDocument/definition", "textDocument/completion"], "exit_on": "textDocument/hover"});
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
    let (mut session, _) = Session::fake("exit-before-initialize", json!({"exit_on": "initialize"}));
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
    assert!(session.finish().success());
}

#[test]
fn a_crash_during_shutdown_still_answers_shutdown() {
    let (mut session, _) = Session::fake("crash-on-shutdown", json!({"exit_on": "shutdown"}));
    session.initialize(&fixture());
    let response = session.request("shutdown", Value::Null);
    assert_eq!(response["result"], Value::Null);
    assert!(response.get("error").is_none(), "{response}");
    session.notify("exit", Value::Null);
    assert!(session.wait().success());
}

#[test]
fn exit_without_shutdown_fails() {
    let (mut session, _) = Session::fake("exit-early", json!({}));
    session.initialize(&fixture());
    session.notify("exit", Value::Null);
    assert_eq!(session.wait().code(), Some(1));
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
    let script = json!({"answers": {ALL: {"error": null, "nodes": [summary_node]}}, "exit_on": "textDocument/hover"});
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
fn a_source_request_on_a_file_the_scan_does_not_hold_goes_to_the_server() {
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
        let answer = session.request("textDocument/rename", at);
        assert_eq!(answer["result"]["echo"], "textDocument/rename", "{answer}");
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

#[test]
fn tells_which_requests_it_answers_itself() {
    let answered = json!({"version": 1, "references": ["source"], "rename": ["source", "cte"],
                          "definition": ["source", "ref", "doc", "entry", "test"]});
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
    let script = json!({"manifest": manifest(), "exit_on": "textDocument/definition"});
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
        position_in(&yml, "ref('orders', v=2)", 6),
        position_in(&project.join("dbt_project.yml"), "summary_upstream", 2),
        position_in(&project.join("models/marts/orders.sql"), "ref('stg_orders')", 6),
    ];
    for params in answers {
        let answer = session.request("textDocument/definition", params.clone());
        assert_eq!(answer["result"], json!({"echo": "textDocument/definition", "params": params}));
    }
    assert_eq!(methods(&log).iter().filter(|method| *method == "textDocument/definition").count(), 5);
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
