// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The subcommands `doctor` and `select`, the binary run as an agent runs it
//! in a shell, in front of the fake dbt, in scratch directories.

use crate::live::paths;
use crate::logged;
use crate::routes::{fake_env, path_with_stub, resolved, starts, venv_fake};
use crate::support::{copy_fixture, script};
use dbt_lsp_plus::config::can_cut;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A copy of the fixture for one test, the fake's script and log beside it.
struct Case {
    /// The test's scratch directory.
    dir: PathBuf,
    copy: PathBuf,
    script: PathBuf,
    log: PathBuf,
    /// The PATH of the runs, the system's unless a test sets it.
    path: OsString,
}

impl Case {
    /// The fixture copied for the test NAME, the fake playing SCRIPT.
    fn new(name: &str, script_value: &Value) -> Case {
        let copy = copy_fixture(name);
        let dir = copy.parent().unwrap().to_owned();
        let (script, log, _) = fake_env(&dir, script_value);
        Case { dir, copy, script, log, path: std::env::var_os("PATH").unwrap() }
    }

    /// The fake's script made SCRIPT_VALUE.
    fn rescript(&self, script_value: &Value) {
        fs::write(&self.script, script_value.to_string()).unwrap();
    }

    /// `dbt-lsp-plus WORDS` to run in AT from an empty environment but for
    /// the case's PATH, the fake's, a scratch home and cache, and git kept to
    /// the scratch directory.
    fn command(&self, at: &Path, words: &[&str]) -> Command {
        let home = self.dir.join("home");
        fs::create_dir_all(&home).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_dbt-lsp-plus"));
        command.args(words).current_dir(at).env_clear().env("PATH", &self.path);
        command.env("HOME", &home).env("XDG_CACHE_HOME", self.dir.join("cache"));
        command.env("FAKE_DBT_LSP_SCRIPT", &self.script).env("FAKE_DBT_LSP_LOG", &self.log);
        command.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null");
        command.env("GIT_CEILING_DIRECTORIES", &self.dir);
        command
    }

    /// `dbt-lsp-plus WORDS` run in AT, as `command` makes it: its exit
    /// status and its answer, after checking that the standard output is one
    /// line of one JSON object and the standard error empty.
    fn run(&self, at: &Path, words: &[&str]) -> (Option<i32>, Value) {
        let output = self.command(at, words).output().expect("run dbt-lsp-plus");
        let (stdout, stderr) = (String::from_utf8(output.stdout).unwrap(), String::from_utf8(output.stderr).unwrap());
        let lines: Vec<&str> = stdout.lines().collect();
        assert!(lines.len() == 1 && stdout.ends_with('\n'), "one line: {stdout:?}, {stderr}");
        let answer: Value = serde_json::from_str(lines[0]).unwrap();
        assert!(answer.is_object() && stderr.is_empty(), "{answer} {stderr}");
        (output.status.code(), answer)
    }

    /// The runs of dbt's CLI that the fake logged, each as it began.
    fn runs(&self) -> Vec<Value> {
        logged(&self.log.with_extension("jsonl.runs")).into_iter().filter(|line| line.get("run").is_some()).collect()
    }
}

/// Whether this system can cut; when not, says that the test NAME checks
/// the refusal in place of the run.
fn cut(name: &str) -> bool {
    let cut = can_cut();
    if !cut {
        let _ = writeln!(std::io::stderr(), "{name}: this system cannot cut: the refusal checked instead");
    }
    cut
}

/// A path's text, resolved, from an answer's VALUE.
fn path_of(value: &Value) -> PathBuf {
    resolved(Path::new(value.as_str().unwrap_or_else(|| panic!("no path: {value}"))))
}

/// PATH with the links of its directory resolved, its last component as
/// written: a link that the answer names, as a `.venv`'s dbt, stays itself.
fn spelt(path: &Path) -> PathBuf {
    resolved(path.parent().unwrap()).join(path.file_name().unwrap())
}

/// The packages' file of the guard's tests: the fixture's local package and
/// a hub package that no lock names.
const HUB: &str =
    "packages:\n  - local: dbt_packages/shop_utils\n  - package: dbt-labs/dbt_utils\n    version: 1.3.0\n";

/// `packages::refused`'s reason for HUB, on any system, when the session
/// is not alone.
const NOT_LOCKED: &str = "a package from the network is not in a lock as recent as the files naming it: dbt parse would install it, \
     rewriting the install path; run dbt deps";

const REFUSED: &str = "dbt-lsp-plus runs dbt only cut off the network, which this system cannot";

#[test]
fn a_subcommand_s_usage_error_says_its_usage_on_the_standard_error_alone() {
    let case = Case::new("cli-usage", &json!({}));
    for (words, usage) in
        [(&["doctor", "--bogus"][..], "usage: dbt-lsp-plus doctor"), (&["select"], "usage: dbt-lsp-plus select")]
    {
        let output = case.command(&case.copy, words).output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!((output.status.code(), output.stdout), (Some(2), Vec::new()), "{words:?}");
        assert!(stderr.contains(usage), "{words:?}: {stderr}");
    }
    let output = case.command(&case.copy, &["doctor", "-h"]).output().unwrap();
    assert!(
        output.status.success() && String::from_utf8(output.stdout).unwrap().starts_with("usage: dbt-lsp-plus doctor")
    );
}

#[test]
fn doctor_finds_a_document_s_project_and_runs_only_its_dbt_s_probe() {
    let mut case = Case::new("cli-doctor-document", &json!({}));
    let repo = case.dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::rename(&case.copy, repo.join("dbt")).unwrap();
    case.copy = repo.join("dbt");
    let venv = venv_fake(&case.copy);
    let (path, stub_log) = path_with_stub(&case.dir, "exit 1");
    case.path = path;
    let before = paths(&repo);
    let (status, mut answer) = case.run(&repo, &["doctor", "dbt/models/marts/orders.sql"]);
    // The paths, as the binary spells them from its working directory, compared with links resolved.
    let taken = [&["path"][..], &["project"], &["dbt", "named"], &["dbt", "command", "0"]].map(|pointer| {
        let value = answer.pointer_mut(&format!("/{}", pointer.join("/"))).map(Value::take).unwrap_or_default();
        spelt(Path::new(value.as_str().unwrap_or_else(|| panic!("no path at {pointer:?}"))))
    });
    let orders = case.copy.join("models/marts/orders.sql");
    assert_eq!(taken, [&orders, &case.copy, &venv, &venv].map(|path| spelt(path)));
    let dbt = json!({"command": [null, "lsp"], "named": null, "source": "the project's .venv", "skipped": [],
                     "lsp": true, "why": null});
    let expected = json!({"version": env!("CARGO_PKG_VERSION"), "path": null, "project": null, "route": "document",
        "dbt": dbt, "alone": "dbt lsp is not started for a project found from a file", "cut": can_cut(),
        "packages": null});
    assert_eq!((status, answer), (Some(0), expected));
    assert!(!stub_log.exists(), "PATH's dbt ran: {}", fs::read_to_string(&stub_log).unwrap_or_default());
    let starts = starts(&case.log);
    assert_eq!(starts.len(), 1, "{starts:#?}");
    assert_eq!(
        (&starts[0]["arguments"], path_of(&starts[0]["cwd"])),
        (&json!(["lsp", "--help"]), resolved(&case.copy))
    );
    assert_eq!(paths(&repo), before, "doctor wrote in the repository");
}

#[test]
fn doctor_on_the_folder_s_route() {
    let case = Case::new("cli-doctor-folder", &json!({}));
    venv_fake(&case.copy);
    let (status, answer) = case.run(&case.copy, &["doctor", "."]);
    assert_eq!((status, &answer["route"], &answer["alone"]), (Some(0), &json!("folder"), &Value::Null), "{answer}");
    assert_eq!(path_of(&answer["project"]), resolved(&case.copy));
    case.rescript(&json!({"no_lsp": true}));
    let (status, answer) = case.run(&case.copy, &["doctor", "."]);
    assert_eq!((status, &answer["dbt"]["lsp"], &answer["alone"]), (Some(0), &json!(false), &json!("it has no lsp")));
}

/// The case NAME for the given route, the fake playing SCRIPT: the copy's
/// `.venv/bin/dbt` a stub that logs to the returned file, which nothing on
/// that route should run, and a directory outside the copy to run in.
fn given(name: &str, script_value: &Value) -> (Case, PathBuf, PathBuf) {
    let case = Case::new(name, script_value);
    let stub_log = case.dir.join("venv.log");
    script(&case.copy.join(".venv/bin/dbt"), &format!("echo \"$*\" >> '{}'", stub_log.display()));
    let elsewhere = case.dir.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    (case, stub_log, elsewhere)
}

#[test]
fn doctor_on_the_given_route_probes_the_command_as_written() {
    let (case, stub_log, elsewhere) = given("cli-doctor-given", &json!({}));
    let (fake, copy) = (env!("CARGO_BIN_EXE_fake-dbt-lsp"), case.copy.to_str().unwrap());
    let never = ["doctor", ".", "--", "--backend", "never", "--", fake, "lsp", "--project-dir", copy];
    let (status, answer) = case.run(&elsewhere, &never);
    let route = (&answer["route"], &answer["alone"], path_of(&answer["project"]));
    assert_eq!((status, route), (Some(0), (&json!("given"), &json!("the backend is never"), resolved(&case.copy))));
    let dbt = &answer["dbt"];
    let words = json!([fake, "lsp", "--project-dir", copy]);
    assert_eq!((&dbt["command"], &dbt["source"]), (&words, &json!("the server command")));
    // Under never the server probes nothing; doctor probes once, for itself.
    let probes: Vec<Value> = starts(&case.log).iter().map(|start| start["arguments"].clone()).collect();
    assert_eq!((&dbt["lsp"], probes), (&json!(true), vec![json!(["lsp", "--help"])]), "{answer}");
    case.rescript(&json!({"no_lsp": true}));
    let auto = ["doctor", ".", "--", "--backend", "auto", "--", fake, "lsp", "--project-dir", copy];
    let (_, answer) = case.run(&elsewhere, &auto);
    assert_eq!((&answer["dbt"]["lsp"], &answer["alone"]), (&json!(false), &json!("it has no lsp")), "{answer}");
    assert!(!stub_log.exists(), "the .venv's dbt ran on the given route");
}

#[test]
fn doctor_finds_no_project_in_a_given_directory_without_one() {
    let (case, _, elsewhere) = given("cli-doctor-given-none", &json!({}));
    let (fake, empty) = (env!("CARGO_BIN_EXE_fake-dbt-lsp"), elsewhere.to_str().unwrap());
    let (status, answer) = case.run(&elsewhere, &["doctor", "--", fake, "lsp", "--project-dir", empty]);
    let error = format!("no dbt_project.yml in {empty}, the server command's --project-dir");
    assert_eq!((status, &answer["project"], &answer["error"]), (Some(1), &Value::Null, &json!(error)), "{answer}");
    assert_eq!(answer.as_object().unwrap().len(), 4, "only version, path, project and error: {answer}");
    assert!(starts(&case.log).is_empty(), "{:#?}", starts(&case.log));
}

#[test]
fn doctor_probes_what_the_lookup_left_unprobed() {
    let mut case = Case::new("cli-doctor-probes", &json!({}));
    let venv = case.copy.join(".venv/bin/dbt");
    script(&venv, "echo \"Error: No such command 'lsp'.\" >&2; exit 2");
    let bin = case.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fake-dbt-lsp"), bin.join("dbt")).unwrap();
    let system = case.path.clone();
    case.path = std::env::join_paths([bin.clone()].into_iter().chain(std::env::split_paths(&system))).unwrap();
    let (status, answer) = case.run(&case.copy, &["doctor"]);
    let dbt = &answer["dbt"];
    assert_eq!(status, Some(0), "{answer}");
    assert_eq!(dbt["skipped"], json!([{"path": venv.to_str().unwrap(), "why": "lsp --help gives exit status: 2"}]));
    let named = bin.join("dbt");
    assert_eq!(
        (&dbt["source"], &dbt["lsp"], spelt(Path::new(dbt["named"].as_str().unwrap()))),
        (&json!("PATH"), &json!(true), spelt(&named))
    );
    assert_eq!(dbt["command"], json!(["dbt", "lsp"]), "a bare dbt stays for PATH");

    // Nothing named dbt on PATH, as an empty directory gives.
    let empty = case.dir.join("empty");
    fs::create_dir_all(&empty).unwrap();
    case.path = empty.into_os_string();
    let (status, answer) = case.run(&case.copy, &["doctor"]);
    assert_eq!((status, &answer["dbt"]["lsp"]), (Some(1), &Value::Null), "{answer}");
    assert!(answer["dbt"]["why"].as_str().unwrap().starts_with("cannot run"), "{answer}");

    // The user's own first word, which the lookup keeps unprobed.
    case.path = system;
    let (own, own_log) = (case.dir.join("own-dbt"), case.dir.join("own.log"));
    script(&own, &format!("echo \"$*\" >> '{}'; echo 'Start the dbt language server'", own_log.display()));
    let (status, answer) = case.run(&case.copy, &["doctor", ".", "--", own.to_str().unwrap(), "lsp"]);
    assert_eq!(
        (status, &answer["dbt"]["source"], &answer["dbt"]["lsp"]),
        (Some(0), &json!("the server command"), &json!(true))
    );
    assert_eq!(fs::read_to_string(&own_log).unwrap(), "lsp --help\n", "probed once");
}

#[test]
fn doctor_without_a_project_runs_nothing() {
    let mut case = Case::new("cli-doctor-quiet", &json!({}));
    let (bin, stubs) = (case.dir.join("stubs"), case.dir.join("stubs.log"));
    let stub = format!("echo \"$0 $*\" >> '{}'\nexit 1", stubs.display());
    ["dbt", "unshare", "git"].iter().for_each(|name| script(&bin.join(name), &stub));
    case.path = bin.into_os_string();
    let folder = case.dir.join("notes");
    fs::create_dir_all(&folder).unwrap();
    let (status, answer) = case.run(&case.dir, &["doctor", "notes/x.sql"]);
    assert_eq!((status, &answer["project"]), (Some(1), &Value::Null), "{answer}");
    assert!(answer["error"].as_str().unwrap().contains("notes/x.sql"), "{answer}");
    let mut keys: Vec<&String> = answer.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["error", "path", "project", "version"]);
    assert!(!stubs.exists(), "{}", fs::read_to_string(&stubs).unwrap_or_default());
}

#[test]
fn doctor_walks_up_from_the_path_as_written_without_its_dots() {
    let case = Case::new("cli-doctor-dots", &json!({}));
    venv_fake(&case.copy);
    let outside = case.dir.join("x/models/a.sql");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, "select 1\n").unwrap();
    // From inside the copy, a file beside it, in no project.
    let (status, answer) = case.run(&case.copy, &["doctor", "../x/models/a.sql"]);
    assert_eq!((status, &answer["project"]), (Some(1), &Value::Null), "{answer}");
    assert_eq!(path_of(&answer["path"]), resolved(&outside), "{answer}");
    assert!(!answer["path"].as_str().unwrap().contains(".."), "{answer}");
    // And into the project from beside it.
    let (status, answer) = case.run(outside.parent().unwrap(), &["doctor", "../../shop/models/marts/orders.sql"]);
    assert_eq!((status, path_of(&answer["project"])), (Some(0), resolved(&case.copy)), "{answer}");
}

#[test]
fn doctor_and_select_give_the_guard_s_reason() {
    let case = Case::new("cli-guard", &json!({}));
    venv_fake(&case.copy);
    fs::write(case.copy.join("packages.yml"), HUB).unwrap();
    // dbt lsp would start behind the proxy: its first parse is guarded as not alone.
    let (status, answer) = case.run(&case.copy, &["doctor"]);
    assert_eq!((status, &answer["packages"]), (Some(0), &json!(NOT_LOCKED)), "{answer}");
    let probes = starts(&case.log).len();
    let (status, answer) = case.run(&case.copy, &["select", "x"]);
    let expected = if cut("doctor_and_select_give_the_guard_s_reason") { NOT_LOCKED } else { REFUSED };
    assert_eq!((status, answer), (Some(1), json!({"nodes": [], "error": expected})));
    assert!(case.runs().is_empty(), "{:?}", case.runs());
    assert_eq!(starts(&case.log).len(), probes, "select probed a dbt it would not run");
}

#[test]
fn doctor_asks_the_guard_as_the_session_s_first_parse_would() {
    let mut case = Case::new("cli-guard-alone", &json!({}));
    venv_fake(&case.copy);
    fs::write(case.copy.join("packages.yml"), HUB).unwrap();
    // Without the cut on any system, where whether the session is alone decides.
    let (bin, _) = path_with_stub(&case.dir, "exit 1");
    script(&case.dir.join("bin/unshare"), "exit 1");
    case.path = bin;
    let (_, document) = case.run(&case.copy, &["doctor", "models/marts/orders.sql"]);
    let alone = "a package from the network is named, and nothing cuts dbt parse off the network here";
    assert_eq!((&document["cut"], &document["packages"]), (&json!(false), &json!(alone)), "{document}");
    let (_, folder) = case.run(&case.copy, &["doctor", "."]);
    assert_eq!((&folder["alone"], &folder["packages"]), (&Value::Null, &json!(NOT_LOCKED)), "{folder}");
}

#[test]
fn select_on_the_given_route_runs_dbt_where_it_is_run() {
    let (case, stub_log, elsewhere) = given("cli-select-given", &json!({"list": listed()}));
    if !cut("select_on_the_given_route_runs_dbt_where_it_is_run") {
        return;
    }
    let (fake, copy) = (env!("CARGO_BIN_EXE_fake-dbt-lsp"), case.copy.to_str().unwrap());
    let (status, answer) = case.run(&elsewhere, &["select", "+stg_orders+", "--", fake, "lsp", "--project-dir", copy]);
    assert_eq!((status, answer), (Some(0), json!({"nodes": listed(), "error": null})));
    let starts = starts(&case.log);
    let list = starts.iter().find(|start| start["arguments"][0] == "list").unwrap();
    assert_eq!(path_of(&list["cwd"]), resolved(&elsewhere), "in the current directory");
    assert!(!stub_log.exists(), "the .venv's dbt ran on the given route");
}

/// The fake's lines for `orders` and `stg_orders`, as `dbt list` prints them.
fn listed() -> [Value; 2] {
    let node = |name: &str, depends: &[&str]| {
        json!({"unique_id": format!("model.shop.{name}"), "name": name, "resource_type": "model",
               "package_name": "shop", "depends_on": {"nodes": depends}})
    };
    [node("orders", &["model.shop.stg_orders"]), node("stg_orders", &[])]
}

#[test]
fn select_lists_the_nodes_by_dbt_list_under_the_cut_and_leaves_no_run() {
    let case = Case::new("cli-select", &json!({"list": listed()}));
    venv_fake(&case.copy);
    if !cut("select_lists_the_nodes_by_dbt_list_under_the_cut_and_leaves_no_run") {
        return;
    }
    let (status, answer) = case.run(&case.copy, &["select", "+stg_orders+"]);
    assert_eq!((status, answer), (Some(0), json!({"nodes": listed(), "error": null})));
    let runs = case.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let words: Vec<&str> = runs[0]["run"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(words[..4], ["list", "--select=+stg_orders+", "--output", "json"]);
    for quiet in ["--no-version-check", "--no-send-anonymous-usage-stats", "--no-manage-state", "--quiet"] {
        assert!(words.contains(&quiet), "{quiet} in {words:?}");
    }
    assert_eq!(runs[0]["interfaces"], json!(["lo"]), "cut off the network");
    let starts = starts(&case.log);
    let list = starts.iter().find(|start| start["arguments"][0] == "list").unwrap();
    assert_eq!(path_of(&list["cwd"]), resolved(&case.copy));
    let kept = fs::read_dir(case.dir.join("cache/dbt-lsp-plus")).unwrap().flatten();
    let left: Vec<PathBuf> =
        kept.flat_map(|hash| fs::read_dir(hash.path()).unwrap().flatten()).map(|e| e.path()).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn select_sweeps_a_run_directory_left_for_a_day() {
    let case = Case::new("cli-select-stale", &json!({"list": listed()}));
    venv_fake(&case.copy);
    if !cut("select_sweeps_a_run_directory_left_for_a_day") {
        return;
    }
    case.run(&case.copy, &["select", "+stg_orders+"]);
    let hashes: Vec<PathBuf> =
        fs::read_dir(case.dir.join("cache/dbt-lsp-plus")).unwrap().flatten().map(|e| e.path()).collect();
    // As a select killed by a signal leaves it, two days ago.
    let stale = hashes[0].join("1");
    fs::create_dir_all(&stale).unwrap();
    let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 3600);
    fs::File::create(stale.join("parse.log")).unwrap().set_modified(two_days).unwrap();
    let fresh = hashes[0].join("2");
    fs::create_dir_all(&fresh).unwrap();
    case.run(&case.copy, &["select", "+stg_orders+"]);
    assert!(!stale.exists() && fresh.exists(), "{:?}", fs::read_dir(&hashes[0]).unwrap().collect::<Vec<_>>());
}

#[test]
fn select_without_the_cut_is_refused_and_runs_nothing() {
    let mut case = Case::new("cli-select-no-cut", &json!({"list": listed()}));
    venv_fake(&case.copy);
    let (bin, _) = path_with_stub(&case.dir, "exit 1");
    script(&case.dir.join("bin/unshare"), "exit 1");
    case.path = bin;
    let (status, answer) = case.run(&case.copy, &["select", "+stg_orders+"]);
    assert_eq!((status, answer), (Some(1), json!({"nodes": [], "error": REFUSED})));
    assert!(case.runs().is_empty(), "{:?}", case.runs());
    assert!(starts(&case.log).is_empty(), "not even a probe: {:#?}", starts(&case.log));
}

#[test]
fn select_fails_with_dbt_s_error_or_with_no_nodes() {
    let error = "[error] [IoError (dbt1001)]: Cannot open file";
    let case = Case::new("cli-select-failures", &json!({"run_log": format!("{error}\n"), "run_exit": 1}));
    venv_fake(&case.copy);
    if !cut("select_fails_with_dbt_s_error_or_with_no_nodes") {
        let (status, answer) = case.run(&case.copy, &["select", "x"]);
        return assert_eq!((status, answer), (Some(1), json!({"nodes": [], "error": REFUSED})));
    }
    let (status, answer) = case.run(&case.copy, &["select", "x"]);
    assert_eq!((status, answer), (Some(1), json!({"nodes": [], "error": error})));
    case.rescript(&json!({}));
    let (status, answer) = case.run(&case.copy, &["select", "nothing"]);
    assert_eq!((status, answer), (Some(1), json!({"nodes": [], "error": "No nodes found"})));
    // Outside every project, the advice is select's own: it takes no path.
    let (status, answer) = case.run(&case.dir, &["select", "x"]);
    let why = answer["error"].as_str().unwrap();
    assert!(
        status == Some(1) && why.contains("run it within the project, or name it with -- dbt lsp --project-dir"),
        "{why}"
    );
}
