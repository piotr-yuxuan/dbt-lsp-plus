// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy's commands for clients, `dbt-lsp-plus.compile`, `.show` and
//! `.nodes`: done by dbt lsp's own commands when it runs, else, for two of
//! them, by dbt's CLI cut off the network.  How each request goes, the
//! words of a run, and what its output answers, in dbt lsp 2.0.9's shapes.

use crate::config::{self, Running};
use crate::parse_log;
use crate::project::Project;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;
use url::Url;

pub const COMPILE: &str = "dbt-lsp-plus.compile";
pub const SHOW: &str = "dbt-lsp-plus.show";
pub const NODES: &str = "dbt-lsp-plus.nodes";

/// Each command, the command of dbt lsp's that does its work, and whether
/// dbt's CLI does it without dbt lsp: `dbt show` would reach the network.
const COMMANDS: [(&str, &str, bool); 3] =
    [(COMPILE, "dbt.compileFile", true), (SHOW, "dbt.show", false), (NODES, "dbt.listNodes", true)];

/// The rows of a show whose client names no limit: `dbt show`'s default.
const LIMIT: u64 = 10;

/// Why a server command with no `lsp` word runs no command: dbt's words
/// are those before it.
pub const NO_LSP_WORD: &str = "the server command has no lsp, so dbt-lsp-plus runs no dbt";

/// What dbt lsp 2.0.9 answers a compile that finds no compiled file.
pub const NOT_FOUND: &str = "Compiled file path not found.";

/// How a request of a command goes.
#[derive(Debug, PartialEq)]
pub enum Via {
    /// To dbt lsp, as its command of this name.
    Server(&'static str),
    /// By dbt's CLI.
    Run,
    /// Answered with this failure.
    Refused(Value),
}

/// The proxy's command named NAME, if it is one.
pub fn own(name: &str) -> Option<&'static str> {
    COMMANDS.iter().map(|(command, ..)| *command).find(|command| *command == name)
}

/// The commands that `initialize` announces: each whose command of dbt
/// lsp's the server LISTS, or that dbt's CLI does, when the system can CUT.
pub fn announced(lists: &dyn Fn(&str) -> bool, cut: bool) -> Vec<&'static str> {
    let done = |(_, dbt, run): &&(&str, &str, bool)| lists(dbt) || (*run && cut);
    COMMANDS.iter().filter(done).map(|(command, ..)| *command).collect()
}

/// How a request of COMMAND goes: to dbt lsp when SERVES says that a server
/// runs that listed its command; else by dbt's CLI, for a command that it
/// does, when the system can CUT; else refused.
pub fn route(command: &str, serves: &dyn Fn(&str) -> bool, cut: bool) -> Via {
    let (_, dbt, run) = COMMANDS.iter().find(|(own, ..)| *own == command).copied().unwrap_or((command, "", false));
    match (serves(dbt), run, cut) {
        (true, ..) => Via::Server(dbt),
        (false, true, true) => Via::Run,
        (false, true, false) => {
            Via::Refused(failure(command, "dbt-lsp-plus runs dbt only cut off the network, which this system cannot"))
        }
        (false, false, _) => {
            Via::Refused(failure(command, "dbt-lsp-plus.show needs dbt lsp: dbt show alone would reach the network"))
        }
    }
}

/// COMMAND's answer that fails for WHY: `error` set, the rest null, or `[]`
/// for nodes.
pub fn failure(command: &str, why: &str) -> Value {
    match command {
        COMPILE => json!({"file_uri": null, "error": why}),
        SHOW => json!({"columns": null, "data": null, "error": why}),
        _ => json!({"nodes": [], "error": why}),
    }
}

/// The arguments of a request of COMMAND, from its PARAMS, as dbt lsp takes
/// them: a URI for a compile; for a show, an object with a `uri` or an
/// `inline` text, and a positive `limit`, `LIMIT` when it has none; a
/// selector for nodes.  None when they cannot be read.
pub fn arguments(command: &str, params: &Value) -> Option<Vec<Value>> {
    let first = params.pointer("/arguments/0")?;
    let shown = |object: &serde_json::Map<String, Value>| {
        let mut object = object.clone();
        let limit = object.entry("limit").or_insert(json!(LIMIT));
        limit.as_u64().filter(|limit| *limit > 0)?;
        Some(Value::Object(object))
    };
    let named = |object: &&serde_json::Map<String, Value>| {
        ["uri", "inline"].iter().any(|key| object.get(*key).is_some_and(Value::is_string))
    };
    let argument = match command {
        SHOW => first.as_object().filter(named).and_then(shown),
        _ => first.as_str().map(|_| first.clone()),
    };
    argument.map(|argument| vec![argument])
}

/// What a run of dbt gave: how it ended, none when it was stopped or ran
/// past its deadline, a reason when it could not start; both its outputs;
/// the manifest it wrote, if it wrote one.
struct Output {
    status: Result<Option<ExitStatus>, String>,
    log: String,
    manifest: Option<Value>,
}

/// A run of dbt for a request of a command: the command, the words after
/// dbt's own, and how its output answers.
pub struct Job {
    command: &'static str,
    pub words: Vec<String>,
    read: Box<dyn FnOnce(&Output) -> Value + Send>,
}

/// Why a run failed, if it did: dbt's first error, from its JSON log when
/// JSON, else from its first `[error]` line; else how it ended, after the
/// first line that is none of its events, as a launcher's own.
fn first_error(output: &Output, json: bool) -> Option<String> {
    let status = match &output.status {
        Err(why) => return Some(why.clone()),
        Ok(None) => return Some("dbt was stopped, or ran past its deadline".into()),
        Ok(Some(status)) => status,
    };
    let logged = match json {
        true => {
            parse_log::read(&output.log).problems.into_iter().find(|problem| problem.severity == 1).map(|p| p.message)
        }
        false => output.log.lines().map(str::trim).find(|line| line.starts_with("[error]")).map(str::to_owned),
    };
    let other = || output.log.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('{'));
    let ended = || other().map_or_else(|| format!("dbt {status}"), |line| format!("{line} (dbt {status})"));
    logged.or_else(|| (!status.success()).then(ended))
}

/// The compile of the file at PATH, of the project at ROOT that PROJECT
/// holds: `dbt compile --select path:R`, R the file's path from the root;
/// its answer, `compiled`'s, from the working directory CWD.  Refused for a
/// file outside the root, in a package, or whose R holds a space or a
/// comma, which dbt's selector would split.
pub fn compile_job(path: Option<&Path>, root: &Path, project: Option<&Project>, cwd: PathBuf) -> Result<Job, Value> {
    let refused = |why: String| failure(COMPILE, &why);
    let path = path.ok_or_else(|| refused("the document is no file".into()))?;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| refused(format!("{} is outside the project at {}", path.display(), root.display())))?;
    let package = project.map_or("", |project| project.root_package.as_str()).to_owned();
    let other = |(name, dir): &(&String, &PathBuf)| **name != package && path.starts_with(dir);
    let in_package = project.and_then(|project| project.packages.iter().find(other));
    if let Some((name, _)) = in_package {
        return Err(refused(format!(
            "{} is a file of the package {name}: only the project's own compile",
            path.display()
        )));
    }
    let relative = relative.to_string_lossy().into_owned();
    if relative.contains(|c: char| c == ',' || c.is_whitespace()) {
        return Err(refused(format!("{relative} holds a space or a comma, which dbt's selector would split")));
    }
    let words = ["compile", "--select", &format!("path:{relative}"), "--log-format", "json"].map(str::to_owned);
    let read = move |output: &Output| match first_error(output, true) {
        Some(why) => failure(COMPILE, &why),
        None => compiled(output.manifest.as_ref(), &package, &relative, &cwd),
    };
    Ok(Job { command: COMPILE, words: words.into(), read: Box::new(read) })
}

/// The answer of a compile of the file at RELATIVE, a path from the root,
/// from the MANIFEST it wrote: the `compiled_path` of the node of the root
/// PACKAGE whose file is RELATIVE, a test's never, as of a properties
/// file, which dbt writes from its working directory CWD; `NOT_FOUND` for
/// none, as for a seed, a macro's file or a disabled model.
pub fn compiled(manifest: Option<&Value>, package: &str, relative: &str, cwd: &Path) -> Value {
    let nodes =
        manifest.and_then(|manifest| manifest["nodes"].as_object()).into_iter().flat_map(|nodes| nodes.values());
    let in_file = |node: &&Value| node["original_file_path"] == relative && node["package_name"] == package;
    let mut own = nodes.filter(in_file).filter(|node| node["resource_type"] != "test");
    let written =
        own.find_map(|node| node["compiled_path"].as_str()).and_then(|path| fs::canonicalize(cwd.join(path)).ok());
    let uri = written.and_then(|path| Url::from_file_path(path).ok());
    uri.map_or_else(|| failure(COMPILE, NOT_FOUND), |uri| json!({"file_uri": uri.as_str(), "error": null}))
}

/// The nodes that SELECTOR selects: `dbt list --select=SELECTOR --output
/// json`, with dbt's text log, since with a JSON one each node would be a
/// log event; its answer, `listed`'s.
pub fn nodes_job(selector: &str) -> Job {
    let words = ["list".into(), format!("--select={selector}"), "--output".into(), "json".into()];
    let read = |output: &Output| match first_error(output, false) {
        Some(why) => failure(NODES, &why),
        None => listed(&output.log),
    };
    Job { command: NODES, words: words.into(), read: Box::new(read) }
}

/// The nodes in TEXT, `dbt list --output json`'s output: each line that is
/// a JSON object with a `unique_id`; "No nodes found" for none, as dbt lsp
/// 2.0.9 answers.
pub fn listed(text: &str) -> Value {
    let nodes = text.lines().filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok());
    let nodes: Vec<Value> = nodes.filter(|node| node["unique_id"].is_string()).collect();
    match nodes.is_empty() {
        true => failure(NODES, "No nodes found"),
        false => json!({"nodes": nodes, "error": null}),
    }
}

/// Hands a run's answer over once: the one the thread sends, else a failure
/// of the command, should the thread end without one, as on a panic.
struct Report(Option<Box<dyn FnOnce(Value) + Send>>, &'static str);

impl Drop for Report {
    fn drop(&mut self) {
        if let Some(report) = self.0.take() {
            report(failure(self.1, "the run ended without an answer"));
        }
    }
}

/// Runs COMMAND, JOB's dbt command writing under DIR, in CWD when given, in
/// a thread of its own, with no input and both outputs in `DIR/run.log`, DIR's manifest
/// removed first so that only this run's counts, the compiled files of
/// earlier runs kept; hands REPORT the answer.  dbt is killed past
/// DEADLINE, or when the run is stopped.
pub fn start(
    job: Job,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    dir: PathBuf,
    deadline: Duration,
    report: Box<dyn FnOnce(Value) + Send>,
) -> Running {
    Running::spawn(move |stopped| {
        let mut report = Report(Some(report), job.command);
        let _ = fs::remove_file(dir.join("target/manifest.json"));
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("run.log");
        let status =
            config::spawn_logged(&command, cwd.as_deref(), &log).map(|child| config::finish(child, deadline, stopped));
        let status = status.map_err(|error| format!("cannot run {}: {error}", command[0]));
        let manifest = fs::read_to_string(dir.join("target/manifest.json")).ok();
        let manifest = manifest.and_then(|text| serde_json::from_str(&text).ok());
        let output = Output { status, log: fs::read_to_string(&log).unwrap_or_default(), manifest };
        let read = (job.read)(&output);
        if let Some(report) = report.0.take() {
            report(read);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::scratch_project;
    use std::collections::HashMap;
    use std::process::Command;
    use std::sync::mpsc;

    fn served(listed: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |dbt: &str| listed.contains(&dbt)
    }

    #[test]
    fn a_request_goes_to_dbt_lsp_when_it_listed_the_command_else_to_the_cli_under_the_cut() {
        let all = served(&["dbt.compileFile", "dbt.show", "dbt.listNodes"]);
        assert_eq!(route(COMPILE, &all, false), Via::Server("dbt.compileFile"));
        assert_eq!(route(SHOW, &all, true), Via::Server("dbt.show"));
        assert_eq!(route(NODES, &all, true), Via::Server("dbt.listNodes"));
        let none = served(&[]);
        assert_eq!(route(COMPILE, &none, true), Via::Run);
        assert_eq!(route(NODES, &none, true), Via::Run);
        let refused = |via: Via| match via {
            Via::Refused(failure) => failure["error"].as_str().unwrap().to_owned(),
            other => panic!("{other:?}"),
        };
        assert!(refused(route(SHOW, &none, true)).contains("needs dbt lsp"), "never alone");
        assert!(refused(route(COMPILE, &none, false)).contains("cut off the network"));
        assert!(refused(route(NODES, &served(&["dbt.compileFile"]), false)).contains("cut off the network"));
    }

    #[test]
    fn initialize_announces_what_the_server_lists_and_what_the_cli_can_do() {
        assert_eq!(
            announced(&served(&["dbt.compileFile", "dbt.show", "dbt.listNodes"]), false),
            [COMPILE, SHOW, NODES]
        );
        assert_eq!(announced(&served(&[]), true), [COMPILE, NODES]);
        assert_eq!(announced(&served(&["dbt.show"]), false), [SHOW]);
        assert_eq!(announced(&served(&[]), false), Vec::<&str>::new());
        assert_eq!((own(COMPILE), own("dbt.compileFile"), own(config::NODE_INFO)), (Some(COMPILE), None, None));
    }

    #[test]
    fn a_failure_has_its_command_s_shape() {
        assert_eq!(failure(COMPILE, "no"), json!({"file_uri": null, "error": "no"}));
        assert_eq!(failure(SHOW, "no"), json!({"columns": null, "data": null, "error": "no"}));
        assert_eq!(failure(NODES, "no"), json!({"nodes": [], "error": "no"}));
    }

    #[test]
    fn arguments_are_read_as_dbt_lsp_takes_them_a_show_s_limit_ten_when_none() {
        let read = |command: &str, arguments: Value| super::arguments(command, &json!({"arguments": arguments}));
        assert_eq!(read(COMPILE, json!(["file:///p/a.sql"])), Some(vec![json!("file:///p/a.sql")]));
        assert_eq!(read(NODES, json!(["+orders+"])), Some(vec![json!("+orders+")]));
        let uri = json!({"uri": "file:///p/a.sql", "limit": 3});
        assert_eq!(read(SHOW, json!([uri])), Some(vec![uri]));
        assert_eq!(read(SHOW, json!([{"inline": "select 1"}])), Some(vec![json!({"inline": "select 1", "limit": 10})]));
        for bad in [json!([]), json!([3]), json!([{"limit": 3}]), json!([{"inline": "x", "limit": 0}]), json!("x")] {
            assert_eq!(read(SHOW, bad.clone()), None, "{bad}");
        }
        assert_eq!(read(COMPILE, json!([7])), None);
        assert_eq!(super::arguments(NODES, &Value::Null), None);
    }

    fn project(root: &Path) -> Project {
        let packages = [("shop".to_owned(), root.to_owned()), ("utils".to_owned(), root.join("dbt_packages/utils"))];
        Project {
            root_package: "shop".into(),
            packages: HashMap::from(packages),
            entities: Vec::new(),
            sources: Vec::new(),
            files: HashMap::new(),
        }
    }

    fn refusal(job: Result<Job, Value>) -> String {
        job.err().map(|failure| failure["error"].as_str().unwrap().to_owned()).unwrap_or_default()
    }

    #[test]
    fn a_compile_selects_its_file_by_its_path_from_the_root_of_the_project_only() {
        let root = Path::new("/a dir/p");
        let project = project(root);
        let job = |path: &str| compile_job(Some(Path::new(path)), root, Some(&project), PathBuf::from("/cwd"));
        let words = job("/a dir/p/models/orders.sql").map(|job| job.words).unwrap_or_default();
        assert_eq!(
            words,
            ["compile", "--select", "path:models/orders.sql", "--log-format", "json"],
            "the root's space is no matter"
        );
        assert!(refusal(job("/elsewhere/a.sql")).contains("outside the project"));
        assert!(refusal(job("/a dir/p/dbt_packages/utils/models/u.sql")).contains("package utils"));
        assert!(refusal(job("/a dir/p/models/my model.sql")).contains("space or a comma"));
        assert!(refusal(job("/a dir/p/models/a,b.sql")).contains("space or a comma"));
        assert!(refusal(compile_job(None, root, Some(&project), PathBuf::new())).contains("no file"));
    }

    #[test]
    fn a_compile_answers_the_compiled_path_of_its_node_in_the_root_package() {
        let dir = scratch_project("compiled-path", &[("target/compiled/shop/models/orders.sql", "select 1")]);
        let node = |package: &str, path: &str| json!({"original_file_path": "models/orders.sql", "package_name": package, "compiled_path": path});
        let manifest = json!({"nodes": {"a": node("utils", "target/elsewhere.sql"), "b": node("shop", "target/compiled/shop/models/orders.sql")}});
        let answer = compiled(Some(&manifest), "shop", "models/orders.sql", &dir);
        let file = fs::canonicalize(dir.join("target/compiled/shop/models/orders.sql")).unwrap();
        assert_eq!(answer, json!({"file_uri": Url::from_file_path(file).unwrap().as_str(), "error": null}));
        assert_eq!(compiled(Some(&manifest), "shop", "models/other.sql", &dir), failure(COMPILE, NOT_FOUND));
        assert_eq!(compiled(None, "shop", "models/orders.sql", &dir), failure(COMPILE, NOT_FOUND));
        let test = json!({"nodes": {"t": {"original_file_path": "models/orders.sql", "package_name": "shop",
                                          "resource_type": "test", "compiled_path": "target/compiled/shop/models/orders.sql"}}});
        assert_eq!(
            compiled(Some(&test), "shop", "models/orders.sql", &dir),
            failure(COMPILE, NOT_FOUND),
            "a test's never"
        );
        let seed = json!({"nodes": {"s": {"original_file_path": "models/orders.sql", "package_name": "shop"}}});
        assert_eq!(
            compiled(Some(&seed), "shop", "models/orders.sql", &dir),
            failure(COMPILE, NOT_FOUND),
            "no compiled_path"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_list_answers_its_json_lines_with_an_id() {
        let text = "{\"unique_id\": \"model.shop.a\", \"name\": \"a\"}\nnoise\n{\"other\": 1}\n{\"unique_id\": \"seed.shop.b\"}\n";
        let answer = listed(text);
        assert_eq!(answer["error"], Value::Null);
        assert_eq!(answer["nodes"].as_array().map(Vec::len), Some(2));
        assert_eq!(listed("noise\n"), failure(NODES, "No nodes found"));
    }

    fn output(status: Result<Option<ExitStatus>, String>, log: &str) -> Output {
        Output { status, log: log.to_owned(), manifest: None }
    }

    fn exited(code: i32) -> Option<ExitStatus> {
        Command::new("sh").args(["-c", &format!("exit {code}")]).status().ok()
    }

    #[test]
    fn a_failed_run_answers_dbt_s_first_error() {
        let error = json!({"info": {"level": "error", "code": "dbt1308", "msg": "[error] [DbDriverFailed (dbt1308)]: no driver", "name": "Event"}});
        let compile = || compile_job(Some(Path::new("/p/m.sql")), Path::new("/p"), None, PathBuf::from("/")).unwrap();
        let read = |job: Job, output: Output| (job.read)(&output)["error"].as_str().unwrap_or_default().to_owned();
        assert!(read(compile(), output(Ok(exited(1)), &format!("{error}\n"))).contains("no driver"));
        assert_eq!(read(compile(), output(Ok(exited(2)), "")), format!("dbt {}", exited(2).unwrap()));
        let launcher = format!("uvx: no network (dbt {})", exited(2).unwrap());
        assert_eq!(read(compile(), output(Ok(exited(2)), "\nuvx: no network\n")), launcher, "a launcher's own line");
        assert!(read(compile(), output(Ok(None), "")).contains("deadline"));
        assert_eq!(read(compile(), output(Err("cannot run dbt: gone".into()), "")), "cannot run dbt: gone");
        let list = output(Ok(exited(1)), "noise\n[error] [IoError (dbt1001)]: Cannot open file\n");
        assert_eq!(read(nodes_job("x"), list), "[error] [IoError (dbt1001)]: Cannot open file");
        assert_eq!(nodes_job("+m.v2+").words, ["list", "--select=+m.v2+", "--output", "json"]);
    }

    #[test]
    fn a_run_whose_reading_panics_is_answered_all_the_same() {
        let dir = scratch_project("run-panics", &[]);
        let (sender, answers) = mpsc::channel();
        let job = Job { command: COMPILE, words: Vec::new(), read: Box::new(|_| panic!("a reader's bug")) };
        let report = Box::new(move |answer| drop(sender.send(answer)));
        let running = start(job, ["true".to_owned()].into(), None, dir.join("run"), Duration::from_secs(10), report);
        let answer = answers.recv_timeout(Duration::from_secs(10)).expect("an answer");
        assert_eq!(answer, failure(COMPILE, "the run ended without an answer"));
        running.stop();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_past_its_deadline_is_killed_and_answered() {
        let dir = scratch_project("run-deadline", &[]);
        let (sender, answers) = mpsc::channel();
        let command = ["sh", "-c", "sleep 30"].map(str::to_owned).to_vec();
        let report = Box::new(move |answer| drop(sender.send(answer)));
        let running = start(nodes_job("x"), command, None, dir.join("run"), Duration::from_millis(100), report);
        let answer = answers.recv_timeout(Duration::from_secs(10)).expect("an answer within the deadline");
        assert!(answer["error"].as_str().unwrap().contains("deadline"), "{answer}");
        running.stop();
        fs::remove_dir_all(&dir).unwrap();
    }
}
