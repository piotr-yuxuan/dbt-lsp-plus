// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The three routes to the project: the server command's `--project-dir`,
//! the client's folder, and the first document in a project; and the quiet
//! of a session that has none.

use crate::support::{Session, copy_fixture, scratch, script, uri};
use crate::{dbt_event, logged, manifest, node_info, published};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What every process of the fake logged as it started, beside LOG.
pub(crate) fn starts(log: &Path) -> Vec<Value> {
    logged(&log.with_extension("jsonl.starts"))
}

/// The start of the fake whose arguments begin with FIRST, in STARTS.
fn started<'a>(starts: &'a [Value], first: &str) -> &'a Value {
    let found = starts.iter().find(|start| start["arguments"][0] == first);
    found.unwrap_or_else(|| panic!("no start of {first} in {starts:#?}"))
}

/// PATH resolved.
pub(crate) fn resolved(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

/// The path of a start's VALUE, resolved.
fn path_of(value: &Value) -> PathBuf {
    resolved(Path::new(value.as_str().unwrap()))
}

/// The fake's script, SCRIPT_VALUE written, its log and the server's cache,
/// as `Session::fake` places them, under DIR.
pub(crate) fn fake_env(dir: &Path, script_value: &Value) -> (PathBuf, PathBuf, PathBuf) {
    let (script_path, log, cache) = (dir.join("script.json"), dir.join("log.jsonl"), dir.join("cache"));
    fs::create_dir_all(dir).unwrap();
    fs::write(&script_path, script_value.to_string()).unwrap();
    (script_path, log, cache)
}

/// The fake linked as the `.venv/bin/dbt` of the project at PROJECT.
pub(crate) fn venv_fake(project: &Path) -> PathBuf {
    let dbt = project.join(".venv/bin/dbt");
    fs::create_dir_all(dbt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fake-dbt"), &dbt).unwrap();
    dbt
}

/// A PATH whose first directory, under DIR, holds a stub `dbt` that
/// appends its arguments to the returned log, then runs BODY.
pub(crate) fn path_with_stub(dir: &Path, body: &str) -> (std::ffi::OsString, PathBuf) {
    let (bin, log) = (dir.join("bin"), dir.join("stub.log"));
    script(&bin.join("dbt"), &format!("echo \"$*\" >> '{}'\n{body}", log.display()));
    let path = std::env::join_paths([bin].into_iter().chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())));
    (path.unwrap(), log)
}

/// The log messages that the session received.
fn log_messages(session: &Session) -> Vec<String> {
    let logs = session.received.iter().filter(|m| m["method"] == "window/logMessage");
    logs.map(|m| m["params"]["message"].as_str().unwrap_or_default().to_owned()).collect()
}

#[test]
fn without_a_project_the_proxy_answers_null_and_runs_nothing() {
    let dir = scratch("routes-quiet");
    let (bin, log, stderr) = (dir.join("bin"), dir.join("stubs.log"), dir.join("stderr"));
    let stub = format!("echo \"$0 $*\" >> '{}'\nexit 1", log.display());
    ["dbt", "git"].iter().for_each(|name| script(&bin.join(name), &stub));
    let folder = dir.join("repo");
    let (yml, sql) = (folder.join("notes/x.yml"), folder.join("notes/y.sql"));
    fs::create_dir_all(yml.parent().unwrap()).unwrap();
    fs::write(&yml, "models:\n  - name: x\n").unwrap();
    fs::write(&sql, "select * from {{ ref('x') }}\n").unwrap();
    let mut session = Session::start_logged(&[], &[("PATH", &bin)], Some(&folder), Some(&stderr));
    let capabilities = json!({"workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}});
    assert_eq!(session.initialize_as(&folder, capabilities)["serverInfo"]["name"], "dbt-lsp-plus");
    session.open(&yml, None);
    session.open(&sql, None);
    let at = json!({"textDocument": {"uri": uri(&sql)}, "position": {"line": 0, "character": 20}});
    let mut references = at.clone();
    references["context"] = json!({"includeDeclaration": true});
    let requests = [
        ("textDocument/hover", at.clone()),
        ("textDocument/definition", at.clone()),
        ("textDocument/references", references),
        ("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(&yml)}})),
        ("workspace/symbol", json!({"query": "x"})),
        ("textDocument/prepareCallHierarchy", at.clone()),
        ("textDocument/implementation", at),
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.p.x"]})),
        ("shutdown", Value::Null),
    ];
    let answers: Vec<Value> = requests.map(|(method, params)| session.request(method, params)).into();
    // Null to each of the server's methods and commands; an unknown method is not found.
    let results: Vec<Option<&Value>> = answers.iter().map(|answer| answer.get("result")).collect();
    let mut expected = vec![Some(&Value::Null); 9];
    expected[6] = None;
    assert_eq!(results, expected);
    let unknown = json!({"code": -32601, "message": "dbt-lsp-plus does not answer textDocument/implementation"});
    assert_eq!(answers[6]["error"], unknown);
    let received = session.received.len();
    let (status, unread) = session.exit();
    // The answers to initialize, the eight requests and shutdown: no other frame.
    assert_eq!((status.success(), received, unread), (true, 10, Vec::new()));
    assert!(!log.exists(), "{}", fs::read_to_string(&log).unwrap_or_default());
    assert_eq!(fs::read_to_string(&stderr).unwrap(), "");
}

/// A session NAME without `--project-dir`, the default `dbt lsp` as its
/// server command, in `repo` as Claude Code 2.1.317 starts it at a repository's
/// root, the fixture copied to `repo/dbt`, whose `.venv/bin/dbt` is the
/// fake, and a stub `dbt` first on PATH; `repo/ci.yml`, then `orders.sql`
/// opened.  Returns the session, the project, its dbt, the fake's log and
/// the stub's.
fn from_a_document(name: &str) -> (Session, PathBuf, PathBuf, PathBuf, PathBuf) {
    let copy = copy_fixture(name);
    let (dir, repo) = (copy.parent().unwrap().to_owned(), copy.with_file_name("repo"));
    let project = repo.join("dbt");
    fs::create_dir_all(&repo).unwrap();
    fs::rename(&copy, &project).unwrap();
    let dbt = venv_fake(&project);
    let (script_path, log, cache) = fake_env(&dir, &json!({"manifest": manifest()}));
    let (path, stub_log) = path_with_stub(&dir, "exit 1");
    let env = [
        ("FAKE_DBT_SCRIPT", script_path.as_path()),
        ("FAKE_DBT_LOG", log.as_path()),
        ("XDG_CACHE_HOME", cache.as_path()),
        ("PATH", Path::new(&path)),
        // No repository but the scratch one: the checkout of the tests is no one's.
        ("GIT_CEILING_DIRECTORIES", dir.as_path()),
        ("GIT_CONFIG_NOSYSTEM", Path::new("1")),
    ];
    let mut session = Session::start(&[], &env, Some(&repo));
    let capabilities = json!({
        "general": {"positionEncodings": ["utf-16"]},
        "textDocument": {"synchronization": {"didSave": true}, "hover": {"contentFormat": ["markdown", "plaintext"]},
                         "definition": {"linkSupport": true}, "callHierarchy": {},
                         "documentSymbol": {"hierarchicalDocumentSymbolSupport": true}},
        "workspace": {"workspaceFolders": false, "didChangeWatchedFiles": {"dynamicRegistration": false}}});
    let result = session.initialize_as(&repo, capabilities);
    assert_eq!(result["capabilities"]["hoverProvider"], json!(true), "{result}");
    fs::write(repo.join("ci.yml"), "on: push\n").unwrap();
    session.open(&repo.join("ci.yml"), None);
    session.open(&project.join("models/marts/orders.sql"), None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    (session, project, dbt, log, stub_log)
}

#[test]
fn without_project_dir_the_first_document_in_a_project_names_it_and_its_dbt() {
    let (mut session, project, dbt, log, stub_log) = from_a_document("routes-document");
    session.request("workspace/symbol", json!({"query": "orders"}));
    let said = log_messages(&session);
    let line = format!("dbt-lsp-plus: project {}, dbt {} (the project's .venv).", project.display(), dbt.display());
    assert_eq!(said, [line]);
    assert!(!session.received.iter().any(|m| m["method"] == "window/showMessage"));
    assert!(session.finish().success());
    assert!(!stub_log.exists(), "PATH's dbt ran");
    let parse = started(&starts(&log), "parse").clone();
    assert_eq!((path_of(&parse["argv0"]), path_of(&parse["cwd"])), (resolved(&dbt), resolved(&project)), "{parse}");
}

#[test]
fn on_a_project_from_a_document_the_proxy_answers_alone() {
    let (mut session, project, ..) = from_a_document("routes-document-answers");
    let orders = project.join("models/marts/orders.sql");
    let on_ref = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    let hover = session.request("textDocument/hover", on_ref.clone());
    let summary = hover["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(summary.starts_with("model view, \"shop\".\"main\".\"stg_orders\""), "{hover}");
    let definition = session.request("textDocument/definition", on_ref.clone());
    assert_eq!(definition["result"]["uri"], uri(&project.join("models/staging/stg_orders.sql")), "{definition}");
    let mut references = on_ref;
    references["context"] = json!({"includeDeclaration": false});
    let references = session.request("textDocument/references", references);
    let files: Vec<&Value> = references["result"].as_array().unwrap().iter().map(|found| &found["uri"]).collect();
    assert!(files.contains(&&json!(uri(&orders))), "{references}");
    let symbols = session.request("workspace/symbol", json!({"query": "orders"}));
    let model = symbols["result"].as_array().unwrap().iter().find(|symbol| symbol["name"] == "orders");
    assert_eq!(model.map(|model| &model["location"]["uri"]), Some(&json!(uri(&orders))), "{symbols}");
    assert!(session.finish().success());
}

#[test]
fn a_document_of_another_project_is_told_where_it_lies() {
    let (mut session, project, ..) = from_a_document("routes-elsewhere");
    let repo = project.parent().unwrap().to_owned();
    let other = repo.join("other");
    let m = other.join("models/m.sql");
    fs::create_dir_all(m.parent().unwrap()).unwrap();
    fs::write(other.join("dbt_project.yml"), "name: other\n").unwrap();
    fs::write(&m, "select * from {{ ref('orders') }}\n").unwrap();
    session.open(&m, None);
    let on_ref = json!({"textDocument": {"uri": uri(&m)}, "position": {"line": 0, "character": 24},
                        "context": {"includeDeclaration": false}});
    let message = format!(
        "{} lies in the dbt project at {}, and this session serves only the one at {}",
        m.display(),
        other.display(),
        project.display()
    );
    let elsewhere = json!({"code": -32803, "message": message});
    // nodeInfo at the same place too, which has no mode.
    let at = json!({"uri": uri(&m), "position": {"line": 0, "character": 24}});
    let info = json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": [at]});
    let reading = [
        ("textDocument/hover", &on_ref),
        ("textDocument/definition", &on_ref),
        ("textDocument/references", &on_ref),
        ("textDocument/prepareCallHierarchy", &on_ref),
        ("workspace/executeCommand", &info),
    ];
    for (method, params) in reading {
        let answer = session.request(method, params.clone());
        assert_eq!((answer.get("result"), &answer["error"]), (None, &elsewhere), "{method}: {answer}");
    }
    let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}});
    let hints = session.request("textDocument/inlayHint", json!({"textDocument": {"uri": uri(&m)}, "range": range}));
    assert_eq!((hints.get("error"), &hints["result"]), (None, &json!([])), "{hints}");
    // The methods of the document's own text, a CTE's rename among them, a document in no project, and the
    // project's own, as before.
    for path in [&m, &repo.join("ci.yml")] {
        let outline = session.request("textDocument/documentSymbol", json!({"textDocument": {"uri": uri(path)}}));
        assert!(outline.get("error").is_none() && outline["result"].is_array(), "{outline}");
    }
    let cte = other.join("models/c.sql");
    fs::write(&cte, "with a as (select 1) select * from a\n").unwrap();
    session.open(&cte, None);
    let on_a = json!({"textDocument": {"uri": uri(&cte)}, "position": {"line": 0, "character": 35}, "newName": "b"});
    let rename = session.request("textDocument/rename", on_a);
    let edits = rename["result"]["changes"][uri(&cte)].as_array().map_or(0, Vec::len);
    assert_eq!((rename.get("error"), edits), (None, 2), "the CTE and its use: {rename}");
    let at_start = json!({"textDocument": {"uri": uri(&repo.join("ci.yml"))}, "position": {"line": 0, "character": 0}});
    let hover = session.request("textDocument/hover", at_start);
    assert!(hover.get("error").is_none(), "{hover}");
    let orders = project.join("models/marts/orders.sql");
    let on_orders = json!({"textDocument": {"uri": uri(&orders)}, "position": {"line": 4, "character": 28}});
    let hover = session.request("textDocument/hover", on_orders);
    assert!(hover.get("error").is_none() && hover["result"]["contents"].is_object(), "{hover}");
    assert!(session.finish().success());
}

/// A session NAME without `--project-dir`, the default `dbt lsp` as its
/// server command, run outside the project, the client's folder a copy of the
/// fixture whose `.venv/bin/dbt` is the fake.  Returns the session, its
/// answer to `initialize` and the project's dbt.
fn from_the_folder(name: &str) -> (Session, Value, PathBuf) {
    let project = copy_fixture(name);
    let dir = project.parent().unwrap().to_owned();
    let dbt = venv_fake(&project);
    let (script_path, log, cache) = fake_env(&dir, &json!({}));
    let env = [
        ("FAKE_DBT_SCRIPT", script_path.as_path()),
        ("FAKE_DBT_LOG", log.as_path()),
        ("XDG_CACHE_HOME", cache.as_path()),
        ("GIT_CEILING_DIRECTORIES", dir.as_path()),
        ("GIT_CONFIG_NOSYSTEM", Path::new("1")),
    ];
    let mut session = Session::start(&[], &env, Some(&dir));
    let result = session.initialize(&project);
    // Once answered, the line that came before it.
    session.request("workspace/symbol", json!({"query": "orders"}));
    (session, result, dbt)
}

#[test]
fn without_project_dir_the_client_s_folder_s_dbt_without_lsp_leaves_the_proxy_alone() {
    let (session, result, dbt) = from_the_folder("routes-folder-alone");
    let said = log_messages(&session);
    assert_eq!(said.len(), 1, "{said:?}");
    let named = format!("dbt {} (the project's .venv).", dbt.display());
    assert!(said[0].ends_with(&named), "{said:?}");
    assert!(session.finish().success());
    let answer = (&result["serverInfo"]["name"], &result["capabilities"]["hoverProvider"]);
    assert_eq!(answer, (&json!("dbt-lsp-plus"), &json!(true)), "{result}");
}

#[test]
fn given_project_dir_the_command_runs_as_written_and_one_line_says_so() {
    let project = copy_fixture("routes-given");
    let dir = project.parent().unwrap().to_owned();
    let venv_log = dir.join("venv.log");
    script(&project.join(".venv/bin/dbt"), &format!("echo \"$0 $*\" >> '{}'\nexit 0", venv_log.display()));
    let (path, stub_log) = path_with_stub(&dir, &format!("exec '{}' \"$@\"", env!("CARGO_BIN_EXE_fake-dbt")));
    let (script_path, log, cache) = fake_env(&dir, &json!({"manifest": manifest()}));
    let env = [
        ("FAKE_DBT_SCRIPT", script_path.as_path()),
        ("FAKE_DBT_LOG", log.as_path()),
        ("XDG_CACHE_HOME", cache.as_path()),
        ("PATH", Path::new(&path)),
    ];
    let command = ["dbt".to_owned(), "lsp".into(), "--project-dir".into(), project.to_string_lossy().into_owned()];
    let stderr = dir.join("stderr");
    let mut session = Session::start_logged(&command, &env, None, Some(&stderr));
    let result = session.initialize(&project);
    assert_eq!(result["serverInfo"]["name"], "dbt-lsp-plus", "{result}");
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    let line = format!("dbt-lsp-plus: project {}, dbt dbt.", project.display());
    assert_eq!(log_messages(&session), [line.as_str()]);
    assert!(session.finish().success());
    // The same line on the standard error, which Claude Code's debug log shows.
    assert_eq!(fs::read_to_string(&stderr).unwrap().lines().next(), Some(line.as_str()));
    // PATH's dbt, as written, ran the parse alone: no probe.
    let ran = fs::read_to_string(&stub_log).unwrap();
    let ran: Vec<&str> = ran.lines().collect();
    assert!(ran.len() == 1 && ran[0].starts_with("parse "), "{ran:?}");
    assert!(!venv_log.exists(), "the project's .venv ran");
    // In the server's working directory, this process's: dbt-mode's profiles.yml stays found.
    let here = resolved(&std::env::current_dir().unwrap());
    let dirs: Vec<PathBuf> = starts(&log).iter().map(|start| path_of(&start["cwd"])).collect();
    assert_eq!(dirs, [here], "the parse");
}

#[test]
fn a_dbt_skipped_in_the_project_s_venv_is_named_in_the_line_before_path_s() {
    let project = copy_fixture("routes-skipped");
    let dir = project.parent().unwrap().to_owned();
    let venv = project.join(".venv/bin/dbt");
    script(&venv, "exit 0");
    std::fs::set_permissions(&venv, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let (path, _) = path_with_stub(&dir, &format!("exec '{}' \"$@\"", env!("CARGO_BIN_EXE_fake-dbt")));
    let (script_path, log, cache) = fake_env(&dir, &json!({}));
    let env = [
        ("FAKE_DBT_SCRIPT", script_path.as_path()),
        ("FAKE_DBT_LOG", log.as_path()),
        ("XDG_CACHE_HOME", cache.as_path()),
        ("PATH", Path::new(&path)),
        ("GIT_CEILING_DIRECTORIES", dir.as_path()),
        ("GIT_CONFIG_NOSYSTEM", Path::new("1")),
    ];
    let mut session = Session::start(&[], &env, Some(&dir));
    assert_eq!(session.initialize(&project)["serverInfo"]["name"], "dbt-lsp-plus");
    session.request("workspace/symbol", json!({"query": "orders"}));
    let said = log_messages(&session);
    let named = format!("dbt {} (PATH); skipped {} (cannot run: ", dir.join("bin/dbt").display(), venv.display());
    assert!(said.len() == 1 && said[0].contains(&named), "{said:?}");
    assert!(session.finish().success());
}

#[test]
fn a_project_from_a_document_runs_dbt_in_its_directory() {
    let copy = copy_fixture("routes-dbt-dir");
    let repo = copy.with_file_name("repo");
    let project = repo.join("dbt");
    fs::create_dir_all(&repo).unwrap();
    fs::rename(&copy, &project).unwrap();
    assert!(project.join("profiles.yml").is_file() && !repo.join("profiles.yml").exists());
    let script_value = json!({"manifest": manifest(), "compiled": "select 1"});
    let fake = [env!("CARGO_BIN_EXE_fake-dbt").to_owned(), "lsp".into()];
    let (script_path, log, cache) = fake_env(&repo.with_file_name("session"), &script_value);
    let env = [
        ("FAKE_DBT_SCRIPT", script_path.as_path()),
        ("FAKE_DBT_LOG", log.as_path()),
        ("XDG_CACHE_HOME", cache.as_path()),
    ];
    let mut session = Session::start(&fake, &env, Some(&repo));
    session.initialize(&repo);
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    let info = session.poll(Duration::from_secs(10), request, |result| result.is_object());
    assert_eq!(info["unique_id"], "model.shop.orders");
    assert_eq!(path_of(&started(&starts(&log), "parse")["cwd"]), resolved(&project), "the parse");
    let compiled = session
        .request("workspace/executeCommand", json!({"command": "dbt-lsp-plus.compile", "arguments": [uri(&orders)]}));
    let file = compiled["result"]["file_uri"].as_str().unwrap_or_else(|| panic!("{compiled}"));
    assert!(url::Url::parse(file).unwrap().to_file_path().unwrap().is_file(), "{compiled}");
    assert_eq!(path_of(&started(&starts(&log), "compile")["cwd"]), resolved(&project), "the compile");
    assert_eq!(node_info(&mut session, json!("model.shop.orders"))["result"]["unique_id"], "model.shop.orders");
    assert!(session.finish().success());
}

#[test]
fn a_project_from_a_document_reads_dbt_s_paths_from_its_directory() {
    let copy = copy_fixture("routes-dbt-paths");
    let (dir, repo) = (copy.parent().unwrap().to_owned(), copy.with_file_name("repo"));
    let project = repo.join("dbt");
    fs::create_dir_all(&repo).unwrap();
    fs::rename(&copy, &project).unwrap();
    // dbt names a YAML error's file from its working directory, here the project.
    let error = "[error] [YAML (dbt1013)]\n  --> models/staging/_sources.yml:3:5";
    let parse_log = dir.join("parse.jsonl");
    fs::write(&parse_log, dbt_event("error", "1013", error) + &dbt_event("info", "Q039", "Finished")).unwrap();
    let script_value = json!({"manifest": manifest(), "parse_log": parse_log, "compiled": "select 1",
                              "compiled_relative": true});
    let (script_path, log, cache) = fake_env(&dir.join("session"), &script_value);
    let env = [
        ("FAKE_DBT_SCRIPT", script_path.as_path()),
        ("FAKE_DBT_LOG", log.as_path()),
        ("XDG_CACHE_HOME", cache.as_path()),
    ];
    let fake = [env!("CARGO_BIN_EXE_fake-dbt").to_owned(), "lsp".into()];
    let mut session = Session::start(&fake, &env, Some(&repo));
    session.initialize(&repo);
    let orders = project.join("models/marts/orders.sql");
    session.open(&orders, None);
    let found = published(&mut session, &project.join("models/staging/_sources.yml"), |params| {
        params["diagnostics"] != json!([])
    });
    assert_eq!(found["diagnostics"][0]["code"], "1013", "{found}");
    // The compiled file, named from the run's working directory through `..`.
    let compiled = session
        .request("workspace/executeCommand", json!({"command": "dbt-lsp-plus.compile", "arguments": [uri(&orders)]}));
    let file = compiled["result"]["file_uri"].as_str().unwrap_or_else(|| panic!("{compiled}"));
    let text = fs::read_to_string(url::Url::parse(file).unwrap().to_file_path().unwrap()).unwrap();
    assert_eq!(text, "select 1");
    assert!(session.finish().success());
}

#[test]
fn a_relative_dbt_and_profiles_dir_name_what_they_named_from_the_proxy_s_directory() {
    let copy = copy_fixture("routes-relative");
    let (dir, repo) = (copy.parent().unwrap().to_owned(), copy.with_file_name("repo"));
    let project = repo.join("dbt");
    fs::create_dir_all(&repo).unwrap();
    fs::rename(&copy, &project).unwrap();
    // The dbt and the profiles of the repository, not of the project.
    let dbt = venv_fake(&repo);
    let (script_path, log, cache) = fake_env(&dir.join("session"), &json!({"manifest": manifest()}));
    let env = [
        ("FAKE_DBT_SCRIPT", script_path.as_path()),
        ("FAKE_DBT_LOG", log.as_path()),
        ("XDG_CACHE_HOME", cache.as_path()),
    ];
    let server = [".venv/bin/dbt", "lsp", "--profiles-dir", "config"].map(str::to_owned);
    let mut session = Session::start(&server, &env, Some(&repo));
    session.initialize(&repo);
    session.open(&project.join("models/marts/orders.sql"), None);
    let request =
        ("workspace/executeCommand", json!({"command": "dbt-lsp-plus.nodeInfo", "arguments": ["model.shop.orders"]}));
    session.poll(Duration::from_secs(10), request, |result| result.is_object());
    assert!(session.finish().success());
    let parse = started(&starts(&log), "parse").clone();
    let arguments: Vec<&str> = parse["arguments"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let profiles = arguments.iter().position(|word| *word == "--profiles-dir").map(|at| arguments[at + 1]);
    assert_eq!(parse["argv0"], json!(dbt.to_str().unwrap()), "{parse}");
    assert_eq!(profiles, Some(repo.join("config").to_str().unwrap()), "{parse}");
}
