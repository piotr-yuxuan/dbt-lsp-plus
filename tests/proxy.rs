// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy in front of a scripted fake of dbt lsp 2.0.9.

mod support;

use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use support::{Session, fixture, received_by_server, scratch, until_logged, uri};

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
    for added in ["documentSymbolProvider", "workspaceSymbolProvider", "callHierarchyProvider"] {
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
    session.send_raw(body.as_bytes());
    session.send_raw(b"not json at all");
    let response = session.wait_for(|m| m["id"] == json!(7));
    assert_eq!(response["result"]["echo"], "textDocument/hover");
    until_logged(&log, "the bodies as sent", |lines| {
        lines.contains(&body) && lines.iter().any(|l| l == "not json at all")
    });
    assert!(session.finish().success());
}

#[test]
fn forwards_the_server_s_messages_byte_for_byte() {
    let answer = r#"{"result": {"z": 1, "a": 1.50},  "id": $ID, "jsonrpc": "2.0"}"#;
    let notification = r#"{"params": {"b": 2, "a": 1.50}, "method": "dbt/custom" , "jsonrpc": "2.0"}"#;
    let script = json!({"raw_answers": {"textDocument/hover": answer}, "raw_on_initialized": [notification]});
    let (mut session, _) = Session::fake("server-bytes", script);
    session.initialize(&fixture());
    session.wait_for(|m| m["method"] == "dbt/custom");
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
    // Renames the CTE final to résultat in the buffer only, then edits a line before it.
    let edits = json!({"textDocument": {"uri": uri(&path), "version": 2}, "contentChanges": [
        {"range": {"start": {"line": 11, "character": 0}, "end": {"line": 11, "character": 5}}, "text": "résultat"},
        {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 0}}, "text": "-- é𝄞\n"}]});
    session.notify("textDocument/didChange", edits);
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&path)}}));
    assert_eq!(names(&outline["result"]), ["orders", "customers", "résultat"]);
    assert_eq!(outline["result"][2]["selectionRange"]["end"], json!({"line": 12, "character": 8}));
    assert!(!methods(&log).contains(&"textDocument/documentSymbol".to_owned()));
    until_logged(&log, "didChange", |lines| lines.iter().any(|line| line.contains("textDocument/didChange")));
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
        node("model.shop.orders", "models/marts/orders.sql", &["model.shop.stg_orders", "model.shop.stg_customers"]),
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
    assert_eq!(
        details(&incoming, "from"),
        ["model.shop.order_summary"],
        "the server's graph replaces the scanned node edges"
    );
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": orders}));
    let callees = ["macro.shop_utils.is_positive", "model.shop.stg_customers", "model.shop.stg_orders"];
    assert_eq!(details(&outgoing["result"], "to"), callees);
    assert_eq!(outgoing["result"][2]["fromRanges"][0]["start"], json!({"line": 4, "character": 21}));
    assert_eq!(
        outgoing["result"][2]["to"]["data"],
        json!({"uniqueId": "model.shop.stg_orders", "resourceType": "model"})
    );
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

#[test]
fn a_failed_listing_keeps_the_graph_it_had() {
    let failed = json!({"error": "Compilation failed", "error_kind": "compilation_failed", "nodes": []});
    let stg_node = node("model.shop.stg_customers", "models/staging/stg_customers.sql", &["source.shop.raw.customers"]);
    let sources =
        json!({"error": null, "nodes": [node("source.shop.raw.customers", "models/staging/_sources.yml", &[])]});
    let all = json!({"error": null, "nodes": [stg_node]});
    let script = json!({"answers_once": {SOURCES: sources}, "answers": {ALL: all, SOURCES: failed}});
    let (mut session, log) = Session::fake("failed-listing", script);
    session.initialize(&fixture());
    let path = fixture().join("models/staging/stg_customers.sql");
    session.open(&path, None);
    until_logged(&log, "the first listing", |_| listings(&log, "source:*") == 1);
    session.notify("textDocument/didSave", json!({"textDocument": {"uri": uri(&path)}}));
    until_logged(&log, "the failed second listing", |_| listings(&log, "source:*") == 2);
    barrier(&mut session);
    let stg = item("models/staging/stg_customers.sql", "model.shop.stg_customers");
    let outgoing = session.request("callHierarchy/outgoingCalls", json!({"item": stg}));
    assert_eq!(details(&outgoing["result"], "to"), ["source.shop.raw.customers"]);
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
