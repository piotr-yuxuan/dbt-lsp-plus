// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy's commands for clients: by dbt lsp's own commands in front of
//! the fake, and by the fake dbt's CLI without it.

use super::{alive, barrier, dbt_event, listings, logged, scratch, scratch_of, unindexed_then, with_options};
use crate::support::{Session, copy_fixture, fixture, until_logged, uri};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const COMPILE: &str = "dbt-lsp-plus.compile";
const SHOW: &str = "dbt-lsp-plus.show";
const NODES: &str = "dbt-lsp-plus.nodes";

fn orders() -> PathBuf {
    fixture().join("models/marts/orders.sql")
}

/// The commands that RESULT, an answer to `initialize`, announces.
fn announced(result: &Value) -> Vec<&str> {
    let commands = result["capabilities"]["executeCommandProvider"]["commands"].as_array();
    commands.into_iter().flatten().filter_map(Value::as_str).collect()
}

fn execute(session: &mut Session, command: &str, arguments: Value) -> Value {
    session.request("workspace/executeCommand", json!({"command": command, "arguments": arguments}))
}

/// The session NAME on the fixture, the fake dbt without `lsp` playing
/// SCRIPT, the proxy working outside the root, as dbt's paths then are;
/// its fake's log, and its answer to `initialize`.
fn alone(name: &str, script: Value) -> (Session, PathBuf, Value) {
    let mut script = script;
    script["no_lsp"] = json!(true);
    let arguments = [Path::new("--project-dir"), &fixture()];
    let (mut session, log) = Session::fake_options(name, script, &[], &arguments, Some(&scratch_of(name)));
    let result = session.initialize(&fixture());
    (session, log, result)
}

/// The runs of dbt's CLI that the fake logged beside LOG, each as it began.
fn runs(log: &Path) -> Vec<Value> {
    logged(&log.with_extension("jsonl.runs")).into_iter().filter(|line| line.get("run").is_some()).collect()
}

/// The requests of COMMAND, one of dbt lsp's, that the fake received.
fn received_command(log: &Path, command: &str) -> Vec<Value> {
    logged(log).into_iter().filter(|m| m["params"]["command"] == command).collect()
}

/// Writes SCRIPT as the fake's of the session NAME, which its next run reads.
fn rescript(name: &str, script: Value) {
    std::fs::write(scratch_of(name).join("script.json"), script.to_string()).unwrap();
}

/// The file that a compile's ANSWER names, and its text.
fn compiled_file(answer: &Value) -> (PathBuf, String) {
    let file_uri = answer["result"]["file_uri"].as_str().unwrap_or_else(|| panic!("no file in {answer}"));
    let path = url::Url::parse(file_uri).unwrap().to_file_path().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    (path, text)
}

#[test]
fn the_commands_are_announced_as_the_server_lists_their_own() {
    let (mut session, _) = with_options("commands-announced", json!({}), &[]);
    let result = session.initialize(&fixture());
    for command in [COMPILE, SHOW, NODES, "dbt.compileFile", "dbt.show", "dbt.listNodes"] {
        assert!(announced(&result).contains(&command), "{command}: {result}");
    }
    assert!(session.finish().success());
    let capabilities = json!({"textDocumentSync": {"openClose": true, "change": 2},
                              "executeCommandProvider": {"commands": ["dbt.compileFile", "dbt.listNodes"]}});
    let (mut session, _) = with_options("commands-no-show", json!({"capabilities": capabilities}), &[]);
    let result = session.initialize(&fixture());
    assert!(announced(&result).contains(&COMPILE) && !announced(&result).contains(&SHOW), "{result}");
    assert!(session.finish().success());
}

#[test]
fn alone_or_with_no_server_compile_and_nodes_are_announced_where_they_can_run() {
    let (session, _, result) = alone("commands-alone-announced", json!({}));
    let expected = [COMPILE, NODES];
    let clients = |result: &Value| -> Vec<String> {
        announced(result)
            .into_iter()
            .filter(|c| c.starts_with("dbt-lsp-plus.") && *c != "dbt-lsp-plus.nodeInfo")
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(clients(&result), expected);
    assert!(session.finish().success());
    let (mut session, _) = with_options("commands-no-start", json!({"exit_on": "initialize"}), &[]);
    let result = session.initialize(&fixture());
    assert_eq!(clients(&result), expected, "a server that could not start");
    assert!(session.finish().success());
}

#[test]
fn compile_goes_to_dbt_lsp_as_its_own_and_comes_back_as_it_came() {
    let key = format!("workspace/executeCommand dbt.compileFile {}", uri(&orders()));
    let raw = r#"{"jsonrpc":"2.0", "id":$ID , "result":{"error":null,"file_uri":"file:///x/orders.sql"}}"#;
    let (mut session, log) = with_options("commands-compile", json!({"raw_answers": {key: raw}}), &[]);
    session.initialize(&fixture());
    let answer = execute(&mut session, COMPILE, json!([uri(&orders())]));
    let at = session.received.iter().position(|m| *m == answer).unwrap();
    assert_eq!(String::from_utf8_lossy(&session.received_raw[at]), raw.replace("$ID", &answer["id"].to_string()));
    let sent = received_command(&log, "dbt.compileFile");
    assert_eq!((&sent[0]["id"], &sent[0]["params"]["arguments"]), (&answer["id"], &json!([uri(&orders())])));
    assert!(session.finish().success());
}

#[test]
fn show_goes_to_dbt_lsp_with_ten_rows_when_the_client_names_no_limit() {
    let (mut session, log) = with_options("commands-show", json!({}), &[]);
    session.initialize(&fixture());
    let by_uri = json!({"uri": uri(&orders()), "limit": 3});
    let answer = execute(&mut session, SHOW, json!([by_uri]));
    assert_eq!(answer["result"]["params"], json!({"command": "dbt.show", "arguments": [by_uri]}), "the fake's echo");
    execute(&mut session, SHOW, json!([{"inline": "select 1"}]));
    let sent: Vec<Value> =
        received_command(&log, "dbt.show").iter().map(|m| m["params"]["arguments"].clone()).collect();
    assert_eq!(sent, [json!([by_uri]), json!([{"inline": "select 1", "limit": 10}])]);
    assert!(session.finish().success());
}

#[test]
fn nodes_counts_as_the_client_s_listing_which_the_proxy_never_cancels() {
    let held = "workspace/executeCommand dbt.listNodes +orders+";
    let (mut session, log) = with_options("commands-nodes", json!({"hold": [held]}), &[]);
    session.initialize(&fixture());
    let params = json!({"command": NODES, "arguments": ["+orders+"]});
    session.send(&json!({"jsonrpc": "2.0", "id": 77, "method": "workspace/executeCommand", "params": params}));
    session.open(&orders(), None);
    barrier(&mut session);
    barrier(&mut session);
    assert_eq!(listings(&log, "*"), 0, "the proxy's listing would cancel the client's");
    session.notify("test/release", json!({}));
    let answer = session.wait_for(|m| m["id"] == json!(77) && m.get("method").is_none());
    assert_ne!(answer["result"]["error"], "Operation cancelled");
    let sent = received_command(&log, "dbt.listNodes").into_iter().find(|m| m["id"] == json!(77)).unwrap();
    assert_eq!(sent["params"]["arguments"], json!(["+orders+"]));
    until_logged(&log, "the deferred listing", |_| listings(&log, "*") == 1);
    assert!(session.finish().success());
}

#[test]
fn a_request_the_proxy_cannot_read_is_invalid_and_goes_nowhere() {
    let (mut session, log) = with_options("commands-invalid", json!({}), &[]);
    session.initialize(&fixture());
    for (command, arguments) in
        [(COMPILE, json!([])), (COMPILE, json!([7])), (SHOW, json!([{"limit": 3}])), (NODES, json!(null))]
    {
        let answer = execute(&mut session, command, arguments.clone());
        assert_eq!(answer["error"]["code"], -32602, "{command} {arguments}: {answer}");
    }
    barrier(&mut session);
    assert!(received_command(&log, "dbt.compileFile").is_empty() && received_command(&log, "dbt.show").is_empty());
    assert!(session.finish().success());
}

#[test]
fn dbt_lsp_s_error_and_its_lenses_reach_the_client_as_they_came() {
    let error = r#"{"jsonrpc":"2.0","id":$ID,"error":{"code":-32603,"message":"boom"}}"#;
    let lenses = r#"{"jsonrpc":"2.0","id":$ID,"result":[{"command":{"arguments":["/p/models/marts/orders.sql",{"name":"customers"}],"command":"dbt.previewCte","title":"Preview CTE"},"range":{"end":{"character":1,"line":7},"start":{"character":1,"line":7}}}]}"#;
    let script =
        json!({"raw_answers": {"workspace/executeCommand dbt.listNodes +x+": error, "textDocument/codeLens": lenses}});
    let (mut session, _) = with_options("commands-raw", script, &[]);
    session.initialize(&fixture());
    let refused = execute(&mut session, NODES, json!(["+x+"]));
    let lens = session.request("textDocument/codeLens", json!({"textDocument": {"uri": uri(&orders())}}));
    for (answer, raw) in [(refused, error), (lens, lenses)] {
        let at = session.received.iter().position(|m| *m == answer).unwrap();
        assert_eq!(String::from_utf8_lossy(&session.received_raw[at]), raw.replace("$ID", &answer["id"].to_string()));
    }
    assert!(session.finish().success());
}

#[test]
fn with_no_server_compile_runs_and_show_says_why() {
    let script = json!({"exit_on": "initialize", "compiled": "select 1 -- by the run"});
    let (mut session, _) = with_options("commands-no-server", script, &[]);
    session.initialize(&fixture());
    let compiled = execute(&mut session, COMPILE, json!([uri(&orders())]));
    let shown = execute(&mut session, SHOW, json!([{"inline": "select 1", "limit": 2}]));
    assert!(shown["result"]["error"].as_str().unwrap().contains("needs dbt lsp"), "{shown}");
    assert_eq!(compiled_file(&compiled).1, "select 1 -- by the run");
    assert!(session.finish().success());
}

#[test]
fn alone_compile_runs_dbt_compile_on_the_file_s_path() {
    let (mut session, log, _) = alone("commands-alone-compile", json!({"compiled": "select 1 -- compiled orders"}));
    let answer = execute(&mut session, COMPILE, json!([uri(&orders())]));
    assert_eq!(answer["result"]["error"], Value::Null, "{answer}");
    let (path, text) = compiled_file(&answer);
    assert_eq!(text, "select 1 -- compiled orders");
    assert!(path.ends_with("run/target/compiled/shop/models/marts/orders.sql"), "{path:?}");
    let run = &runs(&log)[0];
    let words: Vec<&str> = run["run"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let fixture = fixture().to_string_lossy().into_owned();
    for word in ["compile", "path:models/marts/orders.sql", &fixture, "--log-format", "--no-send-anonymous-usage-stats"]
    {
        assert!(words.contains(&word), "{word} in {words:?}");
    }
    assert!(words.windows(2).any(|pair| pair[0] == "--target-path" && pair[1].ends_with("run/target")), "{words:?}");
    assert!(session.finish().success());
}

#[test]
fn alone_a_snapshot_s_compiled_file_is_the_one_inside_its_directory() {
    let written = "compiled/shop/snapshots/orders_snapshot.sql/orders_snapshot.sql";
    let script = json!({"compiled": "select 2", "compile_writes": written});
    let (mut session, _, _) = alone("commands-alone-snapshot", script);
    let snapshot = fixture().join("snapshots/orders_snapshot.sql");
    let answer = execute(&mut session, COMPILE, json!([uri(&snapshot)]));
    assert!(compiled_file(&answer).0.ends_with(written), "{answer}");
    assert!(session.finish().success());
}

#[test]
fn alone_a_failing_compile_never_answers_an_earlier_file() {
    let name = "commands-alone-stale";
    let (mut session, _, _) = alone(name, json!({"compiled": "select 1"}));
    compiled_file(&execute(&mut session, COMPILE, json!([uri(&orders())])));
    let failed = dbt_event("error", "dbt1308", "[error] [DbDriverFailed (dbt1308)]: no driver");
    rescript(name, json!({"no_lsp": true, "run_log": failed, "run_exit": 1}));
    let answer = execute(&mut session, COMPILE, json!([uri(&orders())]));
    assert_eq!(answer["result"]["file_uri"], Value::Null, "{answer}");
    assert!(answer["result"]["error"].as_str().unwrap().contains("no driver"), "{answer}");
    assert!(session.finish().success());
}

#[test]
fn alone_compile_says_what_it_cannot_select() {
    let (mut session, log, _) = alone("commands-alone-refused", json!({}));
    let error = |session: &mut Session, path: &Path| {
        let answer = execute(session, COMPILE, json!([uri(path)]));
        assert_eq!(answer["result"]["file_uri"], Value::Null, "{answer}");
        answer["result"]["error"].as_str().unwrap().to_owned()
    };
    assert_eq!(error(&mut session, &orders()), "Compiled file path not found.", "no compiled file written");
    let outside = error(&mut session, Path::new("/elsewhere/x.sql"));
    assert!(outside.contains("outside the project at") && outside.contains(&*fixture().to_string_lossy()), "{outside}");
    let package = fixture().join("dbt_packages/shop_utils/macros/is_positive.sql");
    assert!(error(&mut session, &package).contains("package shop_utils"));
    assert!(error(&mut session, &fixture().join("models/my model.sql")).contains("space or a comma"));
    assert_eq!(runs(&log).len(), 1, "only the first ran");
    assert!(session.finish().success());
}

#[test]
fn alone_nodes_runs_dbt_list_and_answers_its_lines() {
    let name = "commands-alone-nodes";
    let node = json!({"unique_id": "model.shop.orders", "name": "orders", "resource_type": "model",
                      "package_name": "shop", "original_file_path": "models/marts/orders.sql",
                      "depends_on": {"nodes": ["model.shop.stg_orders"]}});
    let (mut session, log, _) = alone(name, json!({"list": [node]}));
    let answer = execute(&mut session, NODES, json!(["+stg_orders+"]));
    assert_eq!(answer["result"], json!({"nodes": [node], "error": null}));
    let run = runs(&log)[0]["run"].clone();
    let words: Vec<&str> = run.as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(words[..4], ["list", "--select=+stg_orders+", "--output", "json"]);
    assert!(!words.contains(&"--log-format"), "dbt's text log: {words:?}");
    rescript(name, json!({"no_lsp": true}));
    assert_eq!(
        execute(&mut session, NODES, json!(["nothing"]))["result"],
        json!({"nodes": [], "error": "No nodes found"})
    );
    rescript(
        name,
        json!({"no_lsp": true, "run_log": "[error] [IoError (dbt1001)]: Cannot open file\n", "run_exit": 1}),
    );
    let failed = execute(&mut session, NODES, json!(["x"]));
    assert_eq!(failed["result"]["error"], "[error] [IoError (dbt1001)]: Cannot open file");
    assert!(session.finish().success());
}

#[test]
fn alone_compile_takes_dbt_s_error_from_the_events_of_its_json_log_alone() {
    // As both builds write it under `--log-format json` with a target path they cannot create: no event.
    let bare = "[error] [IoError (dbt1001)]: Failed to create output directory: /p/target";
    let (mut session, _, _) =
        alone("commands-alone-bare-error", json!({"run_log": format!("{bare}\n"), "run_exit": 1}));
    let answer = execute(&mut session, COMPILE, json!([uri(&orders())]));
    let ended = format!("{bare} (dbt exit status: 1)");
    assert_eq!(answer["result"], json!({"file_uri": null, "error": ended}), "a launcher's line, with how dbt ended");
    assert!(session.finish().success());
}

#[test]
fn alone_nodes_takes_dbt_s_error_from_the_lines_of_its_text_log_alone() {
    // As both builds write it with `DBT_LOG_FORMAT=json` in their environment and no profiles.yml: events alone.
    let event = dbt_event("error", "dbt1005", "[error] [InvalidConfig (dbt1005)]: No profiles.yml found at /h");
    let (mut session, _, _) = alone("commands-alone-json-list", json!({"run_log": event, "run_exit": 1}));
    let answer = execute(&mut session, NODES, json!(["x"]));
    assert_eq!(answer["result"], json!({"nodes": [], "error": "dbt exit status: 1"}), "how dbt ended");
    assert!(session.finish().success());
}

#[test]
fn alone_show_is_refused_and_runs_nothing() {
    let (mut session, log, result) = alone("commands-alone-show", json!({}));
    assert!(!announced(&result).contains(&SHOW));
    let answer = execute(&mut session, SHOW, json!([{"inline": "select 1", "limit": 2}]));
    assert!(answer["result"]["error"].as_str().unwrap().contains("needs dbt lsp"), "{answer}");
    assert!(runs(&log).is_empty());
    assert!(session.finish().success());
}

#[test]
fn alone_no_run_while_packages_need_the_network() {
    let project = copy_fixture("commands-unsettled-project");
    std::fs::write(project.join("packages.yml"), "packages:\n  - package: dbt-labs/dbt_utils\n    version: 1.4.1\n")
        .unwrap();
    let script = json!({"no_lsp": true, "compiled": "select 1"});
    let (mut session, log) =
        crate::support::Session::fake_with("commands-unsettled", script, &[Path::new("--project-dir"), &project], None);
    session.initialize(&project);
    let answer = execute(&mut session, COMPILE, json!([uri(&project.join("models/marts/orders.sql"))]));
    assert!(answer["result"]["error"].as_str().unwrap().contains("dbt deps"), "{answer}");
    assert!(runs(&log).is_empty());
    assert!(session.finish().success());
}

#[test]
fn alone_runs_go_one_at_a_time_in_the_order_asked() {
    let name = "commands-alone-queue";
    let gate = scratch_of(name).with_extension("gate");
    let _ = std::fs::remove_file(&gate);
    let (mut session, log, _) = alone(name, json!({"compiled": "select 1", "run_gate": gate}));
    for id in [501, 502] {
        let params = json!({"command": COMPILE, "arguments": [uri(&orders())]});
        session.send(&json!({"jsonrpc": "2.0", "id": id, "method": "workspace/executeCommand", "params": params}));
    }
    until_logged(&log.with_extension("jsonl.runs"), "the first run", |lines| lines.len() == 1);
    std::fs::write(&gate, "").unwrap();
    let first = session.wait_for(|m| m["id"] == json!(501) && m.get("method").is_none());
    assert!(!session.received.iter().any(|m| m["id"] == json!(502)), "the first is answered first");
    session.wait_for(|m| m["id"] == json!(502) && m.get("method").is_none());
    assert_eq!(first["result"]["error"], Value::Null, "{first}");
    // Each run began after the one before it ended.
    let lines = logged(&log.with_extension("jsonl.runs"));
    let kinds: Vec<bool> = lines.iter().map(|line| line.get("run").is_some()).collect();
    assert_eq!(kinds, [true, false, true, false], "{lines:?}");
    assert_eq!((&lines[0]["pid"], &lines[2]["pid"]), (&lines[1]["ended"], &lines[3]["ended"]));
    std::fs::remove_file(&gate).unwrap();
    assert!(session.finish().success());
}

#[test]
fn alone_an_earlier_compiled_file_stays_and_only_this_run_s_manifest_counts() {
    let name = "commands-alone-earlier";
    let (mut session, _, _) = alone(name, json!({"compiled": "select 1"}));
    let (orders_file, _) = compiled_file(&execute(&mut session, COMPILE, json!([uri(&orders())])));
    let customers = fixture().join("models/marts/customers.sql");
    compiled_file(&execute(&mut session, COMPILE, json!([uri(&customers)])));
    nodes_then_check(&mut session, &orders_file);
    rescript(name, json!({"no_lsp": true}));
    let answer = execute(&mut session, COMPILE, json!([uri(&orders())]));
    assert_eq!(answer["result"]["error"], "Compiled file path not found.", "no manifest from this run: {answer}");
    assert!(orders_file.exists(), "the earlier file stays for the session");
    assert!(session.finish().success());
}

/// Asks for nodes, a run in the same directory, then checks that FILE, a
/// compiled file, still reads.
fn nodes_then_check(session: &mut Session, file: &Path) {
    execute(session, NODES, json!(["x"]));
    assert_eq!(std::fs::read_to_string(file).unwrap(), "select 1", "{file:?}");
}

#[test]
fn a_server_lost_after_it_listed_the_commands_leaves_compile_to_the_run_and_show_refused() {
    // The first life dies on a hover; the next never answers its initialize, which holds the restart.
    let script = json!({"exit_on": "textDocument/hover", "exit_life": 1, "hang_on": "initialize", "hang_life": 2,
                        "compiled": "select 1 -- by the run"});
    let (mut session, _) = with_options("commands-lost", script, &[]);
    let result = session.initialize(&fixture());
    assert!(announced(&result).contains(&SHOW), "the server listed dbt.show");
    let hover = session.request(
        "textDocument/hover",
        json!({"textDocument": {"uri": uri(&orders())},
                                                              "position": {"line": 0, "character": 0}}),
    );
    unindexed_then(&hover, "; dbt lsp exited (exit status: 3)");
    let compiled = execute(&mut session, COMPILE, json!([uri(&orders())]));
    let shown = execute(&mut session, SHOW, json!([{"inline": "select 1", "limit": 2}]));
    assert!(shown["result"]["error"].as_str().unwrap().contains("needs dbt lsp"), "{shown}");
    assert_eq!(compiled_file(&compiled).1, "select 1 -- by the run");
    session.exit();
}

#[test]
fn with_no_unshare_on_path_compile_and_nodes_are_announced_and_run() {
    let dir = scratch("commands-no-unshare");
    let (script, log) = (dir.join("script.json"), dir.join("log.jsonl"));
    let node = json!({"unique_id": "model.shop.orders", "name": "orders", "resource_type": "model"});
    std::fs::write(&script, json!({"no_lsp": true, "compiled": "select 1", "list": [node]}).to_string()).unwrap();
    let fixture_arg = fixture().to_string_lossy().into_owned();
    let server = [env!("CARGO_BIN_EXE_fake-dbt-lsp"), "lsp", "--project-dir", &fixture_arg].map(str::to_owned);
    // An empty PATH: nothing there, `unshare` included.
    let empty = dir.join("bin");
    std::fs::create_dir_all(&empty).unwrap();
    let env = [
        ("FAKE_DBT_LSP_SCRIPT", script.as_path()),
        ("FAKE_DBT_LSP_LOG", log.as_path()),
        ("PATH", empty.as_path()),
        ("XDG_CACHE_HOME", dir.as_path()),
    ];
    let mut session = Session::start_with(&[], &server, &env, Some(&dir));
    let result = session.initialize(&fixture());
    assert!([COMPILE, NODES].iter().all(|command| announced(&result).contains(command)), "{result}");
    assert_eq!(compiled_file(&execute(&mut session, COMPILE, json!([uri(&orders())]))).1, "select 1");
    assert_eq!(execute(&mut session, NODES, json!(["x"]))["result"], json!({"nodes": [node], "error": null}));
    assert_eq!(runs(&log).len(), 2);
    assert!(session.finish().success());
}

#[test]
fn alone_exit_kills_a_run_under_way() {
    let (mut session, log, _) = alone("commands-alone-exit", json!({"compiled": "select 1", "run_hang": true}));
    let params = json!({"command": COMPILE, "arguments": [uri(&orders())]});
    session.send(&json!({"jsonrpc": "2.0", "id": 600, "method": "workspace/executeCommand", "params": params}));
    until_logged(&log.with_extension("jsonl.runs"), "the run", |lines| lines.len() == 1);
    let pid = runs(&log)[0]["pid"].as_u64().unwrap() as u32;
    session.exit();
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while alive(pid) && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "the run's dbt outlived the proxy");
}
