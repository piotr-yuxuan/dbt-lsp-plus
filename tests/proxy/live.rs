// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The server with the real dbt, whichever build: dbt-oss, or the
//! proprietary one, whose `dbt lsp` never runs.  Opt-in, since it needs
//! dbt 2; built with the network, then run with none, as CI runs it:
//!
//!     cargo test --locked -j 4 --test proxy --no-run
//!     unshare -rn make live DBT_LSP_PLUS_TEST_DBT=/path/to/dbt
//!
//! It runs on a copy of the fixture, from an empty environment with a
//! scratch home, so that dbt writes nothing into the repository nor the
//! user's home.

use crate::support::{Session, copy_fixture, uri};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// With the dbt given: the server's own features, its node information,
/// hover and graph from the user's own `dbt parse`, that parse's errors and
/// the last run's failures as diagnostics, and the commands by dbt's CLI.
#[test]
#[ignore = "needs DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable"]
fn with_the_real_dbt() {
    let dbt = std::env::var("DBT_LSP_PLUS_TEST_DBT").expect("DBT_LSP_PLUS_TEST_DBT names a dbt 2 executable");
    let project = copy_fixture("live");
    let home = project.with_file_name("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(project.join("models/marts/_models.yml"), LIVE_MODELS).unwrap();
    // The files of a build whose relationships test failed, as dbt 2.0.9 writes them.
    std::fs::create_dir_all(project.join("target")).unwrap();
    std::fs::write(project.join("target/run_results.json"), LIVE_RESULTS).unwrap();
    std::fs::write(project.join("target/manifest.json"), LIVE_MANIFEST).unwrap();
    std::fs::write(project.join("target/sources.json"), LIVE_FRESHNESS).unwrap();
    let project_arg = project.to_string_lossy().into_owned();
    let server = [dbt, "lsp".into(), "--project-dir".into(), project_arg.clone(), "--profiles-dir".into(), project_arg];
    // DO_NOT_TRACK does not stop the telemetry of dbt 2.0.9; this flag does.
    let server = [&server[..], &["--no-send-anonymous-usage-stats".to_owned()]].concat();
    let before = paths(&project);
    // An empty environment but for PATH and these, run in the project: dbt writes nowhere else.
    let env = [("HOME", home.as_path()), ("DO_NOT_TRACK", Path::new("1"))];
    let mut session = Session::start(&server, &env, Some(&project));
    session.timeout = Duration::from_secs(180);
    // As an editor with a hierarchical outline that lets the server register its file watcher.
    let capabilities = json!({"textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": true}},
                              "workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}});
    let result = session.initialize_as(&project, capabilities);
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus", "{result}");
    session.wait_for(|m| m["method"] == "client/registerCapability");
    document_features(&mut session, &project);
    navigation(&mut session, &project);
    last_run(&mut session, &project);
    from_the_parse(&mut session, &project);
    lineage(&mut session, &project);
    node_information(&mut session, &project);
    facts_in_the_hover(&mut session, &project);
    navigation_in_sql(&mut session, &project);
    the_parse_s_errors(&mut session, &project);
    commands_by_the_cli(&mut session, &project);
    rename_a_source_table_and_compile(&mut session, &project);
    let shown = session.received.iter().filter(|m| m["method"] == "window/showMessage").count();
    assert_eq!(shown, 0, "no warning");
    assert!(session.finish().success());
    let added: Vec<PathBuf> = paths(&project).difference(&before).cloned().collect();
    assert!(added.is_empty(), "the project gained {added:?}");
}

/// Without `--project-dir`, the project found from a document, then from
/// the client's folder, its dbt the `.venv`'s, with either build: no
/// `--profiles-dir`, since dbt runs in the project; nothing written there
/// but dbt's own output.
#[test]
#[ignore = "needs DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable"]
fn the_project_and_its_dbt_found_without_project_dir() {
    let dbt = std::env::var("DBT_LSP_PLUS_TEST_DBT").expect("DBT_LSP_PLUS_TEST_DBT names a dbt 2 executable");
    let project = copy_fixture("live-found");
    let dir = project.parent().unwrap().to_owned();
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let venv = project.join(".venv/bin/dbt");
    std::fs::create_dir_all(venv.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&dbt, &venv).unwrap();
    let before = paths(&project);
    found_from_a_document(&project, &venv);
    found_from_the_folder(&project);
    from_a_shell(&project);
    let added: Vec<PathBuf> = paths(&project).difference(&before).cloned().collect();
    assert!(added.is_empty(), "the project gained {added:?}");
}

/// A session of the default command without `--project-dir`, in the parent
/// of the copy PROJECT, as an agent starts one at a repository's root.
fn without_project_dir(project: &Path) -> Session {
    let dir = project.parent().unwrap();
    let (home, ceiling) = (dir.join("home"), dir.to_owned());
    let server = ["dbt", "lsp", "--no-send-anonymous-usage-stats"].map(str::to_owned);
    // No repository: the checkout of the tests is no one's.
    let env = [
        ("HOME", home.as_path()),
        ("DO_NOT_TRACK", Path::new("1")),
        ("GIT_CEILING_DIRECTORIES", &ceiling),
        ("GIT_CONFIG_NOSYSTEM", Path::new("1")),
        ("GIT_CONFIG_GLOBAL", Path::new("/dev/null")),
    ];
    let mut session = Session::start(&server, &env, Some(dir));
    session.timeout = Duration::from_secs(180);
    session
}

/// The client's folder above the project: the first model opened names it,
/// and the server answers from the parse of the `.venv`'s dbt, VENV.
fn found_from_a_document(project: &Path, venv: &Path) {
    let mut session = without_project_dir(project);
    session.initialize(project.parent().unwrap());
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    let info = session.poll(Duration::from_secs(120), request, |result| result.is_object());
    assert_eq!(info["relation"], "\"shop\".\"main\".\"orders\"", "{info}");
    let at = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    let definition = session.request("textDocument/definition", at);
    assert_eq!(definition["result"]["uri"], uri(&project.join("models/staging/stg_orders.sql")), "{definition}");
    let line = format!("dbt {} (the project's .venv)", venv.display());
    let said = |m: &Value| m["params"]["message"].as_str().is_some_and(|text| text.contains(&line));
    assert!(session.received.iter().any(|m| m["method"] == "window/logMessage" && said(m)), "{line}");
    assert!(session.finish().success());
}

/// The client's folder the project: the server answers the definition of a
/// ref, once the parse has run.
fn found_from_the_folder(project: &Path) {
    let mut session = without_project_dir(project);
    let result = session.initialize(project);
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus", "{result}");
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let at = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    let found =
        session.poll(Duration::from_secs(120), ("textDocument/definition", at), |result| result["uri"].is_string());
    assert_eq!(found["uri"], uri(&project.join("models/staging/stg_orders.sql")), "{found}");
    assert!(session.finish().success());
}

/// doctor from the parent of the copy PROJECT, on a model, and node and
/// select in PROJECT, as an agent runs them in a shell, with the default
/// server command.
fn from_a_shell(project: &Path) {
    let dir = project.parent().unwrap();
    let run = |at: &Path, words: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dbt-lsp-plus"));
        command.args(words).args(["--", "dbt", "lsp", "--no-send-anonymous-usage-stats"]).current_dir(at);
        command.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("HOME", dir.join("home")).env("XDG_CACHE_HOME", dir.join("cache")).env("DO_NOT_TRACK", "1");
        command
            .env("GIT_CEILING_DIRECTORIES", dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        let output = command.output().expect("run dbt-lsp-plus");
        let (stdout, stderr) = (String::from_utf8(output.stdout).unwrap(), String::from_utf8(output.stderr).unwrap());
        let answer = serde_json::from_str::<Value>(&stdout).unwrap_or_default();
        let one_line = stdout.lines().count() == 1 && stdout.ends_with('\n');
        assert!(one_line && answer.is_object() && stderr.is_empty(), "{words:?}: {stdout} {stderr}");
        (output.status.code(), answer)
    };
    let (status, doctor) = run(dir, &["doctor", "shop/models/marts/orders.sql"]);
    let found = (&doctor["route"], &doctor["dbt"]["source"], &doctor["dbt"]["why"]);
    assert_eq!(
        (status, found),
        (Some(0), (&json!("document"), &json!("the project's .venv"), &Value::Null)),
        "{doctor}"
    );
    assert_eq!(doctor["packages"], Value::Null, "{doctor}");
    // node by id, then by a place, from one parse of its own, whose manifest is kept.
    let (status, by_id) = run(project, &["node", "model.shop.stg_orders"]);
    let (_, by_place) = run(project, &["node", "models/marts/orders.sql:5:29"]);
    let children = by_id["node"]["children"].as_array().cloned().unwrap_or_default();
    let parents = std::fs::read_dir(dir.join("cache/dbt-lsp-plus")).into_iter().flatten().flatten();
    let kept = parents.filter_map(|parent| std::fs::read_dir(parent.path().join("kept")).ok()).flatten().count();
    let found = (status, &by_id["error"], children.contains(&json!("model.shop.orders")));
    assert_eq!(
        (found, &by_place["node"]["unique_id"], kept),
        ((Some(0), &Value::Null, true), &json!("model.shop.stg_orders"), 1),
        "{by_id} {by_place}"
    );
    let (status, selected) = run(project, &["select", "+stg_orders+"]);
    let nodes = selected["nodes"].as_array().into_iter().flatten();
    let orders = nodes.clone().find(|node| node["unique_id"] == "model.shop.orders");
    let upstream = orders.and_then(|orders| orders["depends_on"]["nodes"].as_array());
    let named = upstream.is_some_and(|nodes| nodes.contains(&json!("model.shop.stg_orders")));
    assert!(status == Some(0) && named, "{selected}");
}

/// The information of a node, a hover's summary and the graph, as dbt
/// resolves them: `order_summary`'s upstream is a variable.
fn from_the_parse(session: &mut Session, project: &Path) {
    session.open(&project.join("models/marts/orders.sql"), None);
    let on_ref = json!({"textDocument": {"uri": uri(&project.join("models/marts/orders.sql"))}, "position": {"line": 4, "character": 28}});
    // At once, before the parse has ended: the summary, or why there is none, never a null.
    let early = session.request("textDocument/hover", on_ref.clone());
    let value = early["result"]["contents"]["value"].as_str().unwrap_or_default();
    let reason = early["error"]["message"].as_str().unwrap_or_default();
    let unindexed = early["error"]["code"] == -32803 && reason.starts_with("dbt parse has not finished: ");
    assert!(value.starts_with("model view, \"shop\".\"main\".\"stg_orders\"") || unindexed, "{early}");
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    let orders = session.poll(Duration::from_secs(60), request, |result| result.is_object());
    assert_eq!(orders["relation"], "\"shop\".\"main\".\"orders\"", "{orders}");
    let hover = session.request("textDocument/hover", on_ref);
    let value = hover["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.starts_with("model view, \"shop\".\"main\".\"stg_orders\""), "{hover}");
    let summary = json!({"item": item(project, "models/marts/order_summary.sql", "model.shop.order_summary")});
    let outgoing = session.request("callHierarchy/outgoingCalls", summary);
    let callees: Vec<&str> =
        outgoing["result"].as_array().unwrap().iter().map(|c| c["to"]["detail"].as_str().unwrap()).collect();
    assert_eq!(callees, ["model.shop.orders"]);
}

/// Navigation in SQL once the parse has run: a `ref` to its model's file, a
/// package's macro call to its block, and the references of a model across
/// files.
fn navigation_in_sql(session: &mut Session, project: &Path) {
    let orders = project.join("models/marts/orders.sql");
    let at = |line: u64, character: u64| json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": line, "character": character}});
    let on_ref = session.request("textDocument/definition", at(4, 28));
    assert_eq!(on_ref["result"]["uri"], uri(&project.join("models/staging/stg_orders.sql")), "{on_ref}");
    let on_macro = session.request("textDocument/definition", at(16, 24));
    let block = uri(&project.join("dbt_packages/shop_utils/macros/is_positive.sql"));
    assert_eq!(on_macro["result"]["uri"], block, "{on_macro}");
    let mut params = at(4, 28);
    params["context"] = json!({"includeDeclaration": false});
    let found = session.request("textDocument/references", params);
    let uris: Vec<&str> = found["result"].as_array().into_iter().flatten().filter_map(|l| l["uri"].as_str()).collect();
    let snapshot = uri(&project.join("snapshots/orders_snapshot.sql"));
    assert!(uris.contains(&uri(&orders).as_str()) && uris.contains(&snapshot.as_str()), "{found}");
}

/// A model written outside the editor with an unknown `ref`: the watcher's
/// event parses, and dbt's error shows on it, until the model goes.
fn the_parse_s_errors(session: &mut Session, project: &Path) {
    let broken = project.join("models/marts/broken.sql");
    std::fs::write(&broken, "select * from {{ ref('nowhere') }}\n").unwrap();
    let event = |kind: u8| json!({"changes": [{"uri": uri(&broken), "type": kind}]});
    let on_broken = |m: &Value| m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == uri(&broken);
    session.notify("workspace/didChangeWatchedFiles", event(1));
    let published = session.wait_for(on_broken);
    let diagnostics = published["params"]["diagnostics"].as_array().unwrap();
    let unknown = diagnostics.iter().find(|d| d["code"] == "1048").unwrap_or_else(|| panic!("{published}"));
    assert_eq!((&unknown["source"], &unknown["severity"]), (&json!("dbt parse"), &json!(1)), "{unknown}");
    assert_eq!(unknown["range"]["start"]["line"], 0, "{unknown}");
    std::fs::remove_file(&broken).unwrap();
    session.received.clear();
    session.notify("workspace/didChangeWatchedFiles", event(3));
    assert_eq!(session.wait_for(on_broken)["params"]["diagnostics"], json!([]));
}

/// The commands for clients by dbt's CLI: a model's compiled file in the
/// server's cache, a snapshot's in the directory named after its file, and
/// the nodes of a selector.
fn commands_by_the_cli(session: &mut Session, project: &Path) {
    let compile =
        |session: &mut Session, file: &str| execute(session, "dbt-lsp-plus.compile", json!([uri(&project.join(file))]));
    let (file, text) = compiled_file(&compile(session, "models/marts/orders.sql"));
    assert!(
        file.ends_with("run/target/compiled/shop/models/marts/orders.sql") && !text.contains("{{"),
        "{file:?}: {text}"
    );
    let (snapshot, _) = compiled_file(&compile(session, "snapshots/orders_snapshot.sql"));
    assert!(snapshot.ends_with("snapshots/orders_snapshot.sql/orders_snapshot.sql"), "{snapshot:?}");
    let nodes = execute(session, "dbt-lsp-plus.nodes", json!(["+stg_orders+"]));
    assert!(ids(&nodes).contains(&"model.shop.orders".to_owned()), "{nodes}");
    let orders =
        nodes["result"]["nodes"].as_array().unwrap().iter().find(|n| n["unique_id"] == "model.shop.orders").unwrap();
    assert!(orders["depends_on"]["nodes"].as_array().unwrap().contains(&json!("model.shop.stg_orders")), "{orders}");
}

fn execute(session: &mut Session, command: &str, arguments: Value) -> Value {
    session.request("workspace/executeCommand", json!({"command": command, "arguments": arguments}))
}

/// The file that a compile's ANSWER names, and its text.
fn compiled_file(answer: &Value) -> (PathBuf, String) {
    let file_uri = answer["result"]["file_uri"].as_str().unwrap_or_else(|| panic!("no file in {answer}"));
    let path = url::Url::parse(file_uri).unwrap().to_file_path().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    (path, text)
}

/// The ids of the nodes in a `nodes` ANSWER.
fn ids(answer: &Value) -> Vec<String> {
    let nodes = answer["result"]["nodes"].as_array().into_iter().flatten();
    nodes.filter_map(|node| node["unique_id"].as_str()).map(str::to_owned).collect()
}

/// Every path under DIR, relative to it, but dbt's `target/` and `logs/`.
pub(crate) fn paths(dir: &Path) -> BTreeSet<PathBuf> {
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

/// The server's own `dbt parse`: a node's information, the summary alone on
/// a hover of its ref, and the relations as hints.
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
    assert!(!value.contains("**dbt lsp**"), "no section of another server's: {hover}");
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 100, "character": 0}});
    let orders = uri(&project.join("models/marts/orders.sql"));
    let hints = session.request("textDocument/inlayHint", json!({"textDocument": {"uri": orders}, "range": range}));
    let labels: Vec<&str> = hints["result"].as_array().unwrap().iter().filter_map(|h| h["label"].as_str()).collect();
    let expected = ["\"shop\".\"main\".\"stg_orders\"", "\"shop\".\"main\".\"stg_customers\""];
    assert_eq!(labels, expected, "{hints}");
    assert_eq!(hints["result"][0]["position"], json!({"line": 4, "character": 38}), "{hints}");
    // None on a macro file and on a properties file without a ref.
    for file in ["macros/money.sql", "models/staging/_sources.yml"] {
        let params = json!({"textDocument": {"uri": uri(&project.join(file))}, "range": range});
        let hints = session.request("textDocument/inlayHint", params);
        assert_eq!(hints["result"], json!([]), "{file}: {hints}");
    }
}

/// Once the parse has given the nodes, the facts after a summary, each on a
/// line of its own, hard in markdown: on a `ref`, the model's id; at line 1,
/// column 1 of a model's file, the model's own, from its properties file,
/// `LIVE_MODELS`.
fn facts_in_the_hover(session: &mut Session, project: &Path) {
    let orders = uri(&project.join("models/marts/orders.sql"));
    let at = |line: u64, character: u64| json!({"textDocument": {"uri": orders}, "position": {"line": line, "character": character}});
    let on_ref = session.request("textDocument/hover", at(4, 28));
    let value = on_ref["result"]["contents"]["value"].as_str().unwrap_or_default();
    let summary = value.starts_with("model view, \"shop\".\"main\".\"stg_orders\"");
    assert!(summary && value.lines().any(|line| line.trim_end() == "`model.shop.stg_orders`"), "{on_ref}");
    let own = session.request("textDocument/hover", at(0, 0));
    let value = own["result"]["contents"]["value"].as_str().unwrap_or_default();
    for fact in ["`model.shop.orders`, properties `models/marts/_models.yml`", "columns `customer_id`"] {
        assert!(value.lines().any(|line| line.trim_end() == fact), "{fact}: {own}");
    }
}

/// The outline and folding of a model, the outline of a properties file,
/// the highlights of a CTE, and a hover at the model's start before any
/// index.
fn document_features(session: &mut Session, project: &Path) {
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&orders)}}));
    let names: Vec<&str> = outline["result"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["orders", "customers", "final"]);
    // A properties file's: its sources and their tables.
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
    // The CTE's definition and its three uses.
    let on_orders = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 17, "character": 10}});
    let highlights = session.request("textDocument/documentHighlight", on_orders);
    assert_eq!(highlights["result"].as_array().map(Vec::len), Some(4), "{highlights}");
    // The CTE's use, then outward.
    let at = json!({"textDocument": {"uri": uri(&orders)}, "positions": [{"line": 17, "character": 11}]});
    let selection = session.request("textDocument/selectionRange", at);
    let used = json!({"start": {"line": 17, "character": 9}, "end": {"line": 17, "character": 15}});
    assert_eq!(selection["result"][0]["range"], used, "{selection}");
    // At the model's start, the summary, or why there is no index before the parse the open started has
    // ended: never null.
    let start = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 0, "character": 0}});
    let first = session.request("textDocument/hover", start);
    let message = first["error"]["message"].as_str().unwrap_or_default();
    let unindexed = first["error"]["code"] == -32803 && message.starts_with("dbt parse has not finished: ");
    let summary = first["result"]["contents"]["value"].as_str().is_some_and(|value| value.starts_with("model table"));
    assert!(unindexed || summary, "{first}");
    // On ref('stg_orders'), line 4, once parsed.
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
        description: \"{{ doc('no_such_doc') }}\"
        data_tests:
          - relationships:
              arguments:
                to: ref('customers')
                field: customer_id
";

const LIVE_RESULTS: &str = r#"{"args": {"command": "build"}, "results": [
  {"unique_id": "test.shop.relationships_orders_customer_id.1", "status": "fail", "message": "Error", "failures": 3}]}"#;

const LIVE_MANIFEST: &str = r#"{"nodes": {"test.shop.relationships_orders_customer_id.1": {
  "unique_id": "test.shop.relationships_orders_customer_id.1", "resource_type": "test", "package_name": "shop",
  "name": "relationships_orders_customer_id", "original_file_path": "models/marts/_models.yml",
  "attached_node": "model.shop.orders", "column_name": "customer_id", "test_metadata": {"name": "relationships"}},
  "model.shop.orders": {"resource_type": "model", "name": "orders", "package_name": "shop"}},
  "sources": {"source.shop.raw.orders": {"unique_id": "source.shop.raw.orders", "resource_type": "source",
    "source_name": "raw", "name": "orders", "package_name": "shop",
    "original_file_path": "models/staging/_sources.yml"}}}"#;

const LIVE_FRESHNESS: &str = r#"{"results": [{"unique_id": "source.shop.raw.orders", "status": "Warn",
  "max_loaded_at": "2018-01-02T00:00:00Z", "max_loaded_at_time_ago_in_s": 279704850.0,
  "criteria": {"warn_after": {"count": 1, "period": "day"}, "error_after": {"count": null, "period": null}}}]}"#;

/// The last run's failure at its entry in the properties file, a warning of
/// `dbt parse` possibly beside it, of the unknown doc there.
fn last_run(session: &mut Session, project: &Path) {
    let models = uri(&project.join("models/marts/_models.yml"));
    let failed = |message: &Value| {
        let diagnostics = message["params"]["diagnostics"].as_array().map_or(&[][..], Vec::as_slice);
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == models.as_str()
            && diagnostics.iter().any(|d| d["code"] == "last-run")
    };
    let published = session.wait_for(failed);
    let diagnostics = published["params"]["diagnostics"].as_array().unwrap();
    let ours = |d: &&Value| d["source"] == "dbt-lsp-plus" || d["source"] == "dbt parse";
    assert!(diagnostics.iter().all(|d| ours(&d)), "{published}");
    let failure = diagnostics.iter().find(|d| d["code"] == "last-run").unwrap();
    assert_eq!(
        (&failure["message"], &failure["source"]),
        (&json!("dbt build: relationships failed: 3 rows"), &json!("dbt-lsp-plus")),
        "{published}"
    );
    // The last freshness check's stale table, at its name in the sources' file.
    let sources = uri(&project.join("models/staging/_sources.yml"));
    let stale = |message: &Value| {
        let diagnostics = message["params"]["diagnostics"].as_array().map_or(&[][..], Vec::as_slice);
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == sources.as_str()
            && diagnostics.iter().any(|d| d["code"] == "stale-source")
    };
    let published = session.wait_for(stale);
    let diagnostics = published["params"]["diagnostics"].as_array().unwrap();
    let warning = diagnostics.iter().find(|d| d["code"] == "stale-source").cloned();
    assert_eq!(warning.unwrap()["range"]["start"], json!({"line": 6, "character": 14}), "{published}");
}

/// Definitions and a hover in a properties file, then of a ref in SQL.
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
    // A doc's text, and null for a definition at a position without a target.
    let hover = session.request("textDocument/hover", at("order_status", 2));
    let text = "The status of an order: placed, shipped or returned.";
    assert_eq!(hover["result"]["contents"]["value"], text, "{hover}");
    let on_column = session.request("textDocument/definition", at("customer_id", 2));
    assert_eq!(on_column["result"], Value::Null, "{on_column}");
    // A ref in SQL: one location, and its references, the snapshot's among them.
    let orders = uri(&project.join("models/marts/orders.sql"));
    let on_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}});
    let found = session.request("textDocument/definition", on_ref.clone());
    let start = json!({"line": 0, "character": 0});
    let stg_orders =
        json!({"uri": uri(&project.join("models/staging/stg_orders.sql")), "range": {"start": start, "end": start}});
    assert_eq!(found["result"], stg_orders, "one location");
    let mut references = on_ref;
    references["context"] = json!({"includeDeclaration": false});
    let used = session.request("textDocument/references", references);
    let uses = [json!(orders), json!(uri(&project.join("snapshots/orders_snapshot.sql")))];
    let files: Vec<&Value> = used["result"].as_array().into_iter().flatten().map(|l| &l["uri"]).collect();
    assert_eq!(files, uses.iter().collect::<Vec<_>>(), "{used}");
    // Workspace symbols, each with its id as its container.
    let symbols = session.request("workspace/symbol", json!({"query": "orders"}));
    let model = |s: &Value| s["name"] == "orders" && s["containerName"] == "model.shop.orders";
    assert!(symbols["result"].as_array().is_some_and(|found| found.iter().any(model)), "{symbols}");
}

/// A call hierarchy item for the node ID at PATH of PROJECT.
fn item(project: &Path, path: &str, id: &str) -> Value {
    let zero = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}});
    json!({"name": id.rsplit('.').next().unwrap(), "kind": 5, "uri": uri(&project.join(path)),
           "range": zero, "selectionRange": zero, "data": {"uniqueId": id}})
}

/// The call hierarchy on the manifest's graph: only dbt resolves
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
/// server; once applied, dbt lists the new table, and the model that
/// selects from it still compiles to the relation `page_views`.
fn rename_a_source_table_and_compile(session: &mut Session, project: &Path) {
    let model = project.join("models/staging/stg_page_views.sql");
    session.open(&model, None);
    let compiled =
        |session: &mut Session| compiled_file(&execute(session, "dbt-lsp-plus.compile", json!([uri(&model)]))).0;
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
    let list = json!({"command": "dbt-lsp-plus.nodes", "arguments": ["source:*"]});
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
