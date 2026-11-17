// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The shadow in front of the fake: what it counts and logs, and that the
//! client gets dbt lsp's answers as they came.

use super::{location, logged, manifest, on_ref, parsed_orders, position_in, received, scratch_of, with_options};
use crate::support::{Session, fixture, scratch, uri};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const SUMMARY: &str = "dbt-lsp-plus.shadowSummary";

/// The shadow's log of the fake session NAME on the fixture.
fn log_of(name: &str) -> PathBuf {
    let root = fixture().to_string_lossy().into_owned();
    let file = format!("{}.jsonl", dbt_lsp_plus::config::hash([root]));
    scratch_of(name).join("cache/dbt-lsp-plus/shadow").join(file)
}

/// The session's counts, by the shadow's command.
fn summary(session: &mut Session) -> Value {
    session.request("workspace/executeCommand", json!({"command": SUMMARY}))["result"].clone()
}

/// Whether the result of `initialize`, RESULT, announces the shadow's command.
fn announces_summary(result: &Value) -> bool {
    result["capabilities"]["executeCommandProvider"]["commands"].as_array().unwrap().contains(&json!(SUMMARY))
}

fn orders() -> PathBuf {
    fixture().join("models/marts/orders.sql")
}

/// A session NAME with the shadow on, in front of the fake with SCRIPT,
/// initialized on the fixture.
fn shadowed(name: &str, script: Value) -> Session {
    let (mut session, _) = with_options(name, script, &["--shadow"]);
    assert!(announces_summary(&session.initialize(&fixture())));
    session
}

/// References on the `ref` of orders.sql, without their declaration.
fn references_on_ref() -> Value {
    let mut params = on_ref();
    params["context"] = json!({"includeDeclaration": false});
    params
}

#[test]
fn with_the_shadow_off_nothing_is_counted_nor_written() {
    let (mut session, _) = with_options("shadow-off", json!({}), &[]);
    assert!(!announces_summary(&session.initialize(&fixture())));
    session.request("textDocument/definition", on_ref());
    assert_eq!(summary(&mut session)["echo"], "workspace/executeCommand", "the server's to answer");
    assert!(session.finish().success());
    assert!(!scratch_of("shadow-off").join("cache/dbt-lsp-plus/shadow").exists());
}

#[test]
fn with_no_backend_the_shadow_is_said_to_be_off() {
    let (mut session, _) = with_options("shadow-alone", json!({}), &["--backend", "never", "--shadow"]);
    assert!(!announces_summary(&session.initialize(&fixture())));
    let off = |m: &Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("shadow"));
    session.wait_for(|m| m["method"] == "window/showMessage" && off(m));
    assert!(session.finish().success());
    assert!(!scratch_of("shadow-alone").join("cache/dbt-lsp-plus/shadow").exists());
}

#[test]
fn an_equal_definition_reaches_the_client_as_it_came_and_is_counted() {
    let stg_orders = uri(&fixture().join("models/staging/stg_orders.sql"));
    let raw = r#"{"jsonrpc":"2.0", "id":$ID ,"result":{"uri":"URI","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}}}}"#;
    let raw = raw.replace("URI", &stg_orders);
    let mut session = shadowed("shadow-equal", json!({"raw_answers": {"textDocument/definition": raw}}));
    let answer = session.request("textDocument/definition", on_ref());
    let at = session.received.iter().position(|m| *m == answer).unwrap();
    assert_eq!(String::from_utf8_lossy(&session.received_raw[at]), raw.replace("$ID", &answer["id"].to_string()));
    let counts = json!({"definition": {"ref": {"sql": {"equal": 1}}}});
    assert_eq!(summary(&mut session), json!({"requests": 1, "counts": counts}));
    assert!(session.finish().success());
}

#[test]
fn a_session_logs_its_start_each_followed_request_and_its_counts() {
    let stg_orders = location(&fixture(), "models/staging/stg_orders.sql", 0, 0, 0);
    let mut session = shadowed("shadow-logged", json!({"answers": {"textDocument/definition": stg_orders}}));
    session.request("textDocument/definition", on_ref());
    assert!(session.finish().success());
    let lines = logged(&log_of("shadow-logged"));
    let [start, request, end] = &lines[..] else { panic!("{lines:#?}") };
    assert_eq!((&start["type"], &start["proxy"]), (&json!("start"), &json!(env!("CARGO_PKG_VERSION"))));
    assert_eq!(start["server"], json!({"name": "dbt-lsp", "version": "2.0.9"}));
    assert_eq!(start["modes"], json!({"definition": "merge", "references": "merge"}));
    assert_eq!(request["target"], json!({"kind": "ref", "package": null, "name": "stg_orders", "version": null}));
    assert_eq!((&request["relation"], &request["file"]), (&json!("equal"), &json!("models/marts/orders.sql")));
    assert_eq!(request["counts"], json!({"oursOnly": 0, "dbtOnly": 0, "both": 1}));
    let counts = json!({"definition": {"ref": {"sql": {"equal": 1}}}});
    assert_eq!((&end["type"], &end["counts"]), (&json!("end"), &counts));
}

#[test]
fn a_definition_is_counted_by_how_the_two_answers_stand() {
    let mut whole = location(&fixture(), "models/staging/stg_orders.sql", 0, 0, 0);
    whole["range"]["end"] = json!({"line": 20, "character": 0});
    let script =
        json!({"answers_once": {"textDocument/definition": null}, "answers": {"textDocument/definition": whole}});
    let mut session = shadowed("shadow-relations", script);
    let stg_customers = position_in(&orders(), "ref('stg_customers')", 6);
    let no_target = position_in(&orders(), "select * from final", 0);
    for position in [on_ref(), on_ref(), stg_customers, no_target] {
        let answer = session.request("textDocument/definition", position);
        assert!(answer.get("result").is_some(), "{answer}");
    }
    let counts = &summary(&mut session)["counts"]["definition"];
    assert_eq!(counts["ref"]["sql"], json!({"dbtEmpty": 1, "equal": 1, "disjoint": 1}), "equal by containment");
    assert_eq!(counts["none"]["sql"], json!({"oursEmpty": 1}));
    assert!(session.finish().success());
}

#[test]
fn a_file_not_on_disk_and_an_error_are_counted_apart() {
    let gone = location(Path::new("/nonexistent/dbt-lsp-plus"), "macros/x.sql", 0, 0, 0);
    let script = json!({"answers": {"textDocument/definition": gone}, "refuse_on": "textDocument/references"});
    let mut session = shadowed("shadow-apart", script);
    session.request("textDocument/definition", on_ref());
    let refused = session.request("textDocument/references", references_on_ref());
    assert!(refused.get("error").is_some(), "dbt lsp's error, as it came: {refused}");
    let counts = &summary(&mut session)["counts"];
    assert_eq!(counts["definition"]["ref"]["sql"], json!({"dbtMissingFile": 1}));
    assert_eq!(counts["references"]["ref"]["sql"], json!({"dbtError": 1}));
    assert!(session.finish().success());
}

#[test]
fn references_count_what_the_proxy_adds_and_a_long_list_stays_one_short_line() {
    let whole = json!({"uri": uri(&orders()), "range": {"start": {"line": 0, "character": 0},
                                                       "end": {"line": 100, "character": 0}}});
    let many: Vec<Value> = (0..3000).map(|line| location(&fixture(), "models/marts/orders.sql", line, 0, 1)).collect();
    let script =
        json!({"answers_once": {"textDocument/references": [whole]}, "answers": {"textDocument/references": many}});
    let mut session = shadowed("shadow-references", script);
    session.request("textDocument/references", references_on_ref());
    let long = session.request("textDocument/references", references_on_ref());
    assert_eq!(long["result"].as_array().unwrap().len(), 3000, "the client gets them all");
    let counts = &summary(&mut session)["counts"]["references"]["ref"]["sql"];
    assert_eq!(*counts, json!({"oursAdds": 1, "disjoint": 1}));
    assert!(session.finish().success());
    let log = log_of("shadow-references");
    assert!(std::fs::read_to_string(&log).unwrap().lines().all(|line| line.len() < 8192));
    let lines = logged(&log);
    assert_eq!(lines[1]["counts"], json!({"oursOnly": 1, "dbtOnly": 0, "both": 1}));
    assert_eq!(lines[1]["oursOnly"][0]["path"], "snapshots/orders_snapshot.sql");
    assert_eq!(lines[2]["counts"], json!({"oursOnly": 2, "dbtOnly": 3000, "both": 0}));
    assert_eq!(lines[2]["dbtOnly"].as_array().unwrap().len(), 20, "as many as fit");
}

#[test]
fn in_dbt_mode_every_definition_is_followed() {
    let stg_orders = location(&fixture(), "models/staging/stg_orders.sql", 0, 0, 0);
    let script = json!({"answers_once": {"textDocument/definition": stg_orders},
                        "answers": {"textDocument/definition": null}});
    let options = ["--mode", "textDocument/definition=dbt", "--shadow"];
    let (mut session, log) = with_options("shadow-dbt", script, &options);
    session.initialize(&fixture());
    session.request("textDocument/definition", on_ref());
    let on_source = position_in(&fixture().join("models/staging/stg_customers.sql"), "source('raw', 'customers')", 8);
    assert_eq!(session.request("textDocument/definition", on_source)["result"], Value::Null, "dbt lsp's answer");
    assert_eq!(received(&log, "textDocument/definition"), 2);
    let counts = &summary(&mut session)["counts"]["definition"];
    assert_eq!((&counts["ref"]["sql"], &counts["source"]["sql"]), (&json!({"equal": 1}), &json!({"dbtEmpty": 1})));
    assert!(session.finish().success());
}

#[test]
fn a_definition_the_proxy_answers_is_not_followed() {
    let mut session = shadowed("shadow-listed", json!({}));
    let on_source = position_in(&fixture().join("models/staging/stg_customers.sql"), "source('raw', 'customers')", 8);
    let answer = session.request("textDocument/definition", on_source);
    assert!(answer["result"]["uri"].as_str().is_some_and(|uri| uri.ends_with("_sources.yml")), "{answer}");
    assert_eq!(summary(&mut session), json!({"requests": 0, "counts": {}}));
    assert!(session.finish().success());
    let options = ["--mode", "textDocument/definition=ours", "--shadow"];
    let (mut session, log) = with_options("shadow-ours", json!({}), &options);
    session.initialize(&fixture());
    session.request("textDocument/definition", on_ref());
    assert_eq!(summary(&mut session)["requests"], 0);
    assert_eq!(received(&log, "textDocument/definition"), 0);
    assert!(session.finish().success());
}

#[test]
fn an_edit_before_dbt_lsp_answers_is_counted_as_changed() {
    let mut session = shadowed("shadow-changed", json!({"hold": ["textDocument/definition"]}));
    session.open(&orders(), None);
    session.send(&json!({"jsonrpc": "2.0", "id": 900, "method": "textDocument/definition", "params": on_ref()}));
    // At the end, so that the position still reads the `ref`.
    let end = json!({"line": 22, "character": 0});
    let edit = json!({"textDocument": {"uri": uri(&orders()), "version": 2},
                      "contentChanges": [{"range": {"start": end, "end": end}, "text": "-- edited\n"}]});
    session.notify("textDocument/didChange", edit);
    session.notify("test/release", json!({}));
    session.wait_for(|m| m["id"] == 900 && m.get("method").is_none());
    assert_eq!(summary(&mut session)["counts"]["definition"]["ref"]["sql"], json!({"changed": 1}));
    assert!(session.finish().success());
}

#[test]
fn initialization_options_set_the_shadow_too() {
    let (mut session, _) = with_options("shadow-options", json!({"answers": {"textDocument/definition": null}}), &[]);
    assert!(announces_summary(&session.initialize_options(&fixture(), json!({"dbtLspPlus": {"shadow": true}}))));
    session.request("textDocument/definition", on_ref());
    assert_eq!(summary(&mut session)["counts"]["definition"]["ref"]["sql"], json!({"dbtEmpty": 1}));
    assert!(session.finish().success());
}

#[test]
fn a_full_log_is_renamed_and_a_new_one_begun() {
    let (mut session, _) = with_options("shadow-rotation", json!({}), &["--shadow"]);
    // Before `initialize`, when the proxy first writes.
    let log = log_of("shadow-rotation");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let megabyte = 1024 * 1024;
    std::fs::write(&log, format!("{}\n", "x".repeat(megabyte - 11))).unwrap();
    session.initialize(&fixture());
    session.request("textDocument/definition", on_ref());
    assert!(session.finish().success());
    let older = std::fs::read_to_string(log.with_extension("jsonl.1")).unwrap();
    assert!(older.len() <= megabyte + 8192, "{}", older.len());
    let last: Value = serde_json::from_str(older.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "start", "the line that took it past a megabyte");
    let types: Vec<Value> = logged(&log).iter().map(|line| line["type"].clone()).collect();
    assert_eq!(types, [json!("request"), json!("end")]);
}

#[test]
fn the_oracle_is_dbt_s_resolution_at_the_last_parse() {
    let mut parsed = manifest();
    parsed["nodes"]["model.shop.orders"]["depends_on"] = json!({"nodes": ["model.shop.stg_orders"]});
    let stg_orders = location(&fixture(), "models/staging/stg_orders.sql", 0, 0, 0);
    let script = json!({"manifest": parsed, "answers": {"textDocument/definition": stg_orders}});
    let (mut session, _) = with_options("shadow-oracle", script, &["--shadow"]);
    session.initialize(&fixture());
    parsed_orders(&mut session);
    session.request("textDocument/definition", on_ref());
    assert!(session.finish().success());
    let request = logged(&log_of("shadow-oracle")).into_iter().find(|line| line["type"] == "request").unwrap();
    assert_eq!(request["oracle"], json!([{"id": "model.shop.stg_orders", "file": "models/staging/stg_orders.sql"}]));
    assert_eq!(request["index"]["present"], true);
}

#[test]
fn with_no_cache_directory_the_shadow_counts_writes_nothing_and_says_so() {
    let dir = scratch("shadow-no-cache");
    let (script, tmp) = (dir.join("script.json"), dir.join("tmp"));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(&script, json!({"answers": {"textDocument/definition": null}}).to_string()).unwrap();
    let fixture_arg = fixture().to_string_lossy().into_owned();
    let server = [env!("CARGO_BIN_EXE_fake-dbt-lsp"), "lsp", "--project-dir", &fixture_arg].map(str::to_owned);
    // Neither XDG_CACHE_HOME nor HOME: the temporary directory is no place for it.
    let env = [("FAKE_DBT_LSP_SCRIPT", script.as_path()), ("TMPDIR", tmp.as_path())];
    let mut session = Session::start_with(&["--shadow"], &server, &env, Some(&dir));
    session.initialize(&fixture());
    session.request("textDocument/definition", on_ref());
    assert_eq!(summary(&mut session)["counts"]["definition"]["ref"]["sql"], json!({"dbtEmpty": 1}));
    let no_place = |m: &Value| m["params"]["message"].as_str().is_some_and(|text| text.contains("has no place"));
    session.wait_for(|m| m["method"] == "window/logMessage" && no_place(m));
    assert!(session.finish().success());
    let shadows = [tmp.join("dbt-lsp-plus/shadow"), dir.join("dbt-lsp-plus/shadow"), dir.join(".cache")];
    assert!(shadows.iter().all(|shadow| !shadow.exists()), "{shadows:?}");
}
