// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The resolved configuration of a project's nodes, which dbt lsp 2.0.9 does
//! not expose, from the user's own `dbt parse`: the user's dbt as a project
//! runs it, read once from the server command, the parse's run and outcome,
//! the index of the manifest it writes, and the summary of a node.

use crate::graph::ServerNode;
use crate::node::Type;
use crate::parse_log;
use crate::run::{self, Ending, Output, Run, Running};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The command under which dbt-lsp-plus answers a node's information.
pub const NODE_INFO: &str = "dbt-lsp-plus.nodeInfo";

/// How long a run of dbt, the parse or a command's, may run before it is
/// killed.
pub const PARSE_DEADLINE: Duration = Duration::from_secs(120);

/// How long a probe of what a command can do, such as `dbt lsp --help`,
/// may run before it is killed.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// How long a cache beside this process's may go unwritten before it is
/// taken for a dead process's.
const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);

/// The flags of `dbt lsp` that `dbt parse` takes too and that change what it
/// reads.
const KEPT: [&str; 5] = ["--profiles-dir", "--profile", "--target", "--vars", "--packages-install-path"];

/// The words of the proxy's `dbt parse`, as `Dbt::run` takes them.
pub const PARSE: [&str; 3] = ["parse", "--log-format", "json"];

/// The log of a parse, at its cache's root, as `remove_stale` reads it.
pub const PARSE_LOG: &str = "parse.log";

/// The log of a command's run, in its cache's `run/`, as `remove_stale`
/// reads it.
pub const RUN_LOG: &str = "run.log";

/// The user's dbt as a project runs it: the server command, read once, and
/// where dbt runs.
#[derive(Debug, Default)]
pub struct Dbt {
    command: Vec<String>,
    /// Where dbt runs: the project, when the client's folder or a document
    /// named it; none, the proxy's working directory.
    dir: Option<PathBuf>,
    /// The words before `lsp`, as `uvx dbt` or `env X=1 dbt`; none without
    /// it, as a wrapper of its own.
    launcher: Option<Vec<String>>,
    /// The flags of KEPT after `lsp`, as `--flag X`.
    kept: Vec<String>,
}

impl Dbt {
    /// The dbt of COMMAND, the server command, run in DIR when given: every
    /// word before `lsp`, and the flags of KEPT after it, as `--flag X` or
    /// `--flag=X`.
    pub fn new(command: Vec<String>, dir: Option<PathBuf>) -> Dbt {
        let at = command.iter().position(|word| word == "lsp");
        let launcher = at.map(|at| command[..at].to_vec());
        let mut kept = Vec::new();
        let mut rest = command[at.map_or(command.len(), |at| at + 1)..].iter();
        while let Some(word) = rest.next() {
            let (flag, value) = word.split_once('=').map_or((word.as_str(), None), |(flag, value)| (flag, Some(value)));
            if KEPT.contains(&flag) {
                kept.push(flag.to_owned());
                kept.extend(value.map(str::to_owned).or_else(|| rest.next().cloned()));
            }
        }
        Dbt { command, dir, launcher, kept }
    }

    /// The server command, as read.
    pub fn command(&self) -> &[String] {
        &self.command
    }

    /// Where dbt runs; none, the proxy's working directory.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The run of dbt WORDS, as `PARSE`: the words before `lsp`, then WORDS,
    /// then the flags of KEPT; the project at ROOT; its output under CACHE,
    /// its log LOG there, with neither telemetry nor a check of versions;
    /// killed past `PARSE_DEADLINE`.  None when the command has no `lsp`
    /// word.
    pub fn run(&self, words: &[String], root: &Path, cache: PathBuf, log: &'static str) -> Option<Run> {
        let path = |path: &Path| path.to_string_lossy().into_owned();
        let mut line = self.launcher.clone()?;
        line.extend_from_slice(words);
        line.extend_from_slice(&self.kept);
        line.extend(["--project-dir".into(), path(root), "--target-path".into(), path(&cache.join("target"))]);
        line.extend(["--log-path".into(), path(&cache.join("logs")), "--write-json".into()]);
        let quiet = ["--no-version-check", "--no-send-anonymous-usage-stats", "--no-manage-state", "--quiet"];
        line.extend(quiet.map(str::to_owned));
        Some(Run { line, cwd: self.dir.clone(), dir: cache, log, deadline: PARSE_DEADLINE })
    }
}

/// SERVER, a server command, with what it names from the directory DIR,
/// the proxy's, made absolute: a first word that is a path, as
/// `.venv/bin/dbt`, and `--profiles-dir`'s value, in either form, which
/// dbt reads from its working directory; so that they name the same once
/// dbt runs in the project.  `--packages-install-path` is the project's
/// already.
pub fn anchored(server: &[String], dir: &Path) -> Vec<String> {
    let absolute = |value: &str| dir.join(value).to_string_lossy().into_owned();
    let anchored = |(at, word): (usize, &String)| {
        let valued = at > 0 && server[at - 1] == "--profiles-dir";
        match word.strip_prefix("--profiles-dir=") {
            Some(value) => format!("--profiles-dir={}", absolute(value)),
            None if valued || (at == 0 && word.contains('/')) => absolute(word),
            None => word.clone(),
        }
    };
    server.iter().enumerate().map(anchored).collect()
}

/// The user's cache directory: `$XDG_CACHE_HOME`, else `$HOME/.cache`;
/// none when neither is absolute, since an empty or relative one would put
/// the cache in the working directory.
pub fn cache_base() -> Option<PathBuf> {
    let absolute = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute());
    absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")))
}

/// PARTS hashed with FNV-1a, which no build changes, each ended by a zero
/// byte, as sixteen hexadecimal digits.
pub fn hash(parts: impl IntoIterator<Item = String>) -> String {
    let key = parts.into_iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, part| {
        part.bytes().chain([0]).fold(hash, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
    });
    format!("{key:016x}")
}

/// Where this process keeps its parses of ROOT with the server command
/// SERVER, under BASE: one directory per root and command, by their
/// `hash`, then one per process.
pub fn cache_dir(base: &Path, root: &Path, server: &[String]) -> PathBuf {
    let parts = std::iter::once(root.to_string_lossy().into_owned()).chain(server.iter().cloned());
    base.join("dbt-lsp-plus").join(hash(parts)).join(std::process::id().to_string())
}

/// Where the manifest of a parse that started while the project lay on
/// disk as STAMP (`cache::disk`) says is kept, beside CACHE, a process's
/// directory, for those after it: in `kept/` under their one parent,
/// which `remove_stale` sweeps by its own time.
pub fn kept(cache: &Path, stamp: u64) -> PathBuf {
    cache.with_file_name("kept").join(format!("{stamp:016x}.json"))
}

/// Keeps the manifest that a parse wrote under CACHE, a process's
/// directory, for the project as it lay on disk as STAMP: renamed to
/// `kept`'s place, the other files of `kept/` removed; nothing when it
/// cannot.
pub fn keep(cache: &Path, stamp: u64) {
    let kept = kept(cache, stamp);
    let Some(dir) = kept.parent() else { return };
    if fs::create_dir_all(dir).is_err() || fs::rename(cache.join("target/manifest.json"), &kept).is_err() {
        return;
    }
    for other in fs::read_dir(dir).into_iter().flatten().flatten().filter(|other| other.path() != kept) {
        let _ = fs::remove_file(other.path());
    }
}

/// Removes the caches beside CACHE that neither a parse nor a run wrote for
/// a day, as a proxy or a select killed by a signal leaves: a sibling's
/// time is its `parse.log`'s, else its own, as a select writes no parse's
/// log, unless its `run/run.log` is newer.  No process is asked whether it
/// lives: a home shared between hosts holds the caches of processes this
/// host cannot see, and a parse or a run under way keeps its log fresh.
pub fn remove_stale(cache: &Path) {
    let own = cache.file_name();
    let siblings = cache.parent().and_then(|parent| fs::read_dir(parent).ok()).into_iter().flatten().flatten();
    let modified = |path: &Path| fs::metadata(path).and_then(|meta| meta.modified()).ok();
    for sibling in siblings.filter(|sibling| Some(sibling.file_name().as_os_str()) != own) {
        let path = sibling.path();
        let parsed = modified(&path.join(PARSE_LOG)).or_else(|| modified(&path));
        let written = parsed.max(modified(&path.join("run").join(RUN_LOG)));
        let age = written.and_then(|written| written.elapsed().ok());
        if age.is_some_and(|age| age > STALE_AFTER) {
            let _ = fs::remove_dir_all(sibling.path());
        }
    }
}

/// The value of FLAG in COMMAND, as `FLAG X` or `FLAG=X`.
pub fn flag<'a>(command: &'a [String], flag: &str) -> Option<&'a str> {
    command.iter().enumerate().find_map(|(at, word)| match word.strip_prefix(flag) {
        Some("") => command.get(at + 1).map(String::as_str),
        Some(rest) => rest.strip_prefix('='),
        None => None,
    })
}

/// The nodes of a manifest by `unique_id`, sources and disabled nodes
/// included, each with `relation` and `tests`, and `disabled` where so; and
/// the manifest's groups.
#[derive(Debug, Default)]
pub struct ParseIndex {
    root_package: String,
    nodes: HashMap<String, Value>,
    groups: HashMap<String, Value>,
    /// The ids of the nodes of each name, so that a lookup reads a few.
    named: HashMap<String, Vec<String>>,
    /// The ids of the last manifest laid, whose nodes alone make a listing:
    /// a failed parse's lacks those deleted since, which the index keeps.
    latest: HashSet<String>,
}

/// The ids of NODES by their name.
fn by_name(nodes: &HashMap<String, Value>) -> HashMap<String, Vec<String>> {
    let mut named: HashMap<String, Vec<String>> = HashMap::new();
    for (id, node) in nodes {
        named.entry(node["name"].as_str().unwrap_or_default().to_owned()).or_default().push(id.clone());
    }
    named
}

impl ParseIndex {
    /// The index of MANIFEST.  dbt gives no relation to an ephemeral model or
    /// a test, and builds no analysis, though it writes one for it.
    pub fn of(manifest: &Value) -> ParseIndex {
        let listed = ["nodes", "sources"].into_iter().filter_map(|key| manifest[key].as_object()).flatten();
        let mut nodes: HashMap<String, Value> = listed.map(|(id, node)| (id.clone(), node.clone())).collect();
        // A disabled node is only under `disabled`, as the list of its versions.
        for (id, versions) in manifest["disabled"].as_object().into_iter().flatten() {
            let mut node = versions.get(0).cloned().unwrap_or_else(|| json!({}));
            node["disabled"] = json!(true);
            nodes.entry(id.clone()).or_insert(node);
        }
        // A test on a source, or a singular test, has no attached node: it
        // tests what it depends on.
        let mut tests: HashMap<String, Vec<String>> = HashMap::new();
        for (id, node) in nodes.iter().filter(|(_, node)| node["resource_type"] == "test") {
            let attached = node["attached_node"].as_str().map(|attached| vec![json!(attached)]);
            let depends_on =
                || node.pointer("/depends_on/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
            for tested in attached.unwrap_or_else(depends_on).iter().filter_map(Value::as_str) {
                tests.entry(tested.to_owned()).or_default().push(id.clone());
            }
        }
        for (id, node) in &mut nodes {
            let built =
                Type::named(node["resource_type"].as_str().unwrap_or_default()).built && node["disabled"] != true;
            node["relation"] = if built { node["relation_name"].clone() } else { Value::Null };
            let mut attached = tests.remove(id).unwrap_or_default();
            attached.sort();
            node["tests"] = json!(attached);
        }
        let groups = manifest["groups"]
            .as_object()
            .map_or_else(HashMap::new, |groups| groups.iter().map(|(id, group)| (id.clone(), group.clone())).collect());
        let root_package = manifest.pointer("/metadata/project_name").and_then(Value::as_str).unwrap_or_default();
        let named = by_name(&nodes);
        let latest = nodes.keys().cloned().collect();
        ParseIndex { root_package: root_package.to_owned(), nodes, groups, named, latest }
    }

    /// This index with the entries of NEWER laid over it: what a parse that
    /// failed part of the way still found.
    pub fn overlay(&mut self, newer: ParseIndex) {
        self.latest = newer.latest;
        self.nodes.extend(newer.nodes);
        self.groups.extend(newer.groups);
        self.named = by_name(&self.nodes);
        if !newer.root_package.is_empty() {
            self.root_package = newer.root_package;
        }
    }

    pub fn node(&self, id: &str) -> Option<&Value> {
        self.nodes.get(id)
    }

    /// The unique ids, sorted, of the enabled nodes of the last manifest
    /// laid whose `depends_on.nodes` names the node ID, every test aside,
    /// by its `resource_type`.
    pub fn children(&self, id: &str) -> Vec<&str> {
        let child = |node: &Value| {
            let depends_on = node.pointer("/depends_on/nodes").and_then(Value::as_array);
            node["resource_type"] != "test"
                && node["disabled"] != true
                && depends_on.is_some_and(|nodes| nodes.iter().any(|parent| parent == id))
        };
        let mut children: Vec<&str> = self
            .latest
            .iter()
            .filter(|latest| self.nodes.get(*latest).is_some_and(child))
            .map(String::as_str)
            .collect();
        children.sort_unstable();
        children
    }

    /// The enabled models, seeds and snapshots of the last manifest named
    /// NAME in PACKAGE, each version of a versioned model apart.
    pub fn resources(&self, package: &str, name: &str) -> Vec<&Value> {
        let latest = self.named.get(name).into_iter().flatten().filter(|id| self.latest.contains(*id));
        let resource = |node: &&Value| {
            Type::named(node["resource_type"].as_str().unwrap_or_default()).refable
                && node["package_name"] == package
                && node["disabled"] != true
        };
        latest.filter_map(|id| self.nodes.get(id)).filter(resource).collect()
    }

    /// Whether the last manifest disables the node ID.
    pub fn disabled(&self, id: &str) -> bool {
        self.latest.contains(id) && self.nodes.get(id).is_some_and(|node| node["disabled"] == true)
    }

    /// The enabled nodes of the last manifest, of the kinds that
    /// `dbt.listNodes` lists, `*` and `source:*`, by id: what a listing would
    /// give, from the manifest.
    pub fn listed(&self) -> Vec<ServerNode> {
        let kinds = ["model", "seed", "snapshot", "analysis", "test", "source"];
        let listed =
            |node: &&Value| node["disabled"] != true && kinds.iter().any(|kind| node["resource_type"] == *kind);
        let latest = self.latest.iter().filter_map(|id| self.nodes.get(id));
        let mut nodes: Vec<ServerNode> = latest.filter(listed).filter_map(ServerNode::of).collect();
        nodes.sort_by(|one, other| one.id.cmp(&other.id));
        nodes
    }

    /// The one node of the root project that the file at PATH, relative to
    /// the root, defines; none for a file that defines several, as YAML does.
    pub fn of_file(&self, path: &str) -> Option<&Value> {
        let defines = |node: &&Value| {
            node["original_file_path"] == path
                && node["package_name"] == self.root_package.as_str()
                && node["resource_type"] != "source"
        };
        let mut found = self.nodes.values().filter(defines);
        let first = found.next();
        found.next().map_or(first, |_| None)
    }

    /// The summary of NODE, for people: one line of what dbt builds and
    /// where, then one of what is set beyond the defaults, if anything is.
    pub fn summary(&self, node: &Value) -> String {
        let config = &node["config"];
        let materialized = config["materialized"].as_str();
        let incremental = (materialized == Some("incremental")).then(|| {
            let strategy = config["incremental_strategy"].as_str().map(|strategy| format!("strategy {strategy}"));
            let keys = config["unique_key"]
                .as_array()
                .map(|keys| keys.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
            let key = config["unique_key"].as_str().map(str::to_owned).or(keys).map(|key| format!("unique key {key}"));
            [strategy, key].into_iter().flatten().collect::<Vec<_>>().join(", ")
        });
        // dbt 2.0.9 writes null counts for a source with no freshness.
        let configured = |after: &str| !node["freshness"][after]["count"].is_null();
        let freshness = (configured("warn_after") || configured("error_after")).then(|| "freshness".to_owned());
        let loaded_at = node["loaded_at_field"].as_str().map(|field| format!("loaded at {field}"));
        let relation = node["relation"].as_str().map_or_else(|| "no relation".to_owned(), str::to_owned);
        let first = [node["resource_type"].as_str().map(str::to_owned), materialized.map(str::to_owned), incremental];
        let first: Vec<String> = first.into_iter().flatten().filter(|part| !part.is_empty()).collect();
        let first = [first.join(" "), relation].into_iter().chain(freshness).chain(loaded_at).collect::<Vec<_>>();
        let tests = node["tests"].as_array().map_or(0, Vec::len);
        let group = config["group"].as_str().map(|group| {
            let owner = self.groups.values().find(|found| found["name"] == group).map(|found| &found["owner"]);
            let owner = owner.and_then(|owner| owner["name"].as_str().or(owner["email"].as_str()));
            owner.map_or_else(|| format!("group {group}"), |owner| format!("group {group} (owner {owner})"))
        });
        let tags = config["tags"]
            .as_array()
            .filter(|tags| !tags.is_empty())
            .map(|tags| format!("tags {}", tags.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
        let set = [
            group,
            node["access"].as_str().filter(|access| *access != "protected").map(|access| format!("access {access}")),
            (config["contract"]["enforced"] == true).then(|| "contract enforced".to_owned()),
            (!node["version"].is_null()).then(|| format!("version {}", node["version"])),
            tags,
            (node["disabled"] == true).then(|| "disabled".to_owned()),
            (tests > 0).then(|| format!("{tests} test{}", if tests == 1 { "" } else { "s" })),
        ];
        let set: Vec<String> = set.into_iter().flatten().collect();
        let first = first.join(", ");
        if set.is_empty() { first } else { format!("{first}\n{}", set.join("; ")) }
    }

    /// What a hover shows of NODE after its summary: one line of its unique
    /// id and its properties file, the one that patches it, else a source's
    /// own, from the root, or, in a package, `<package>://` and from the
    /// package's directory, as dbt writes `patch_path`; then, if any column
    /// is declared, one line of the columns, each its name and its type when
    /// declared, in the manifest's order as read, which is by name.  Each
    /// value is a code span, so that markdown shows its bytes as they are,
    /// which plain text gives as they are too.
    pub fn facts(&self, node: &Value) -> String {
        // Line breaks as spaces, as markdown shows them in a span, so that the
        // span is one line; a fence longer than any run of backticks inside;
        // and a space within it when the value begins or ends with a backtick
        // or a space, since markdown drops one space at each end.
        let code = |text: &str| {
            let text = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
            let fence = "`".repeat(text.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1);
            let edge = [' ', '`'];
            let pad = !text.trim_matches(' ').is_empty() && (text.starts_with(edge) || text.ends_with(edge));
            let space = if pad { " " } else { "" };
            format!("{fence}{space}{text}{space}{fence}")
        };
        let package = node["package_name"].as_str().filter(|package| *package != self.root_package);
        let patch = node["patch_path"].as_str().map(|path| path.split_once("://").map_or(path, |(_, file)| file));
        let source = || node["original_file_path"].as_str().filter(|_| node["resource_type"] == "source");
        let file = patch
            .or_else(source)
            .map(|file| package.map_or_else(|| file.to_owned(), |in_package| format!("{in_package}://{file}")));
        let id = code(node["unique_id"].as_str().unwrap_or_default());
        let first = file.map_or_else(|| id.clone(), |file| format!("{id}, properties {}", code(&file)));
        let columns = node["columns"].as_object().into_iter().flatten().map(|(name, column)| {
            let declared = column["data_type"].as_str().filter(|data_type| !data_type.is_empty());
            declared.map_or_else(|| code(name), |data_type| format!("{} {}", code(name), code(data_type)))
        });
        let columns: Vec<String> = columns.collect();
        if columns.is_empty() { first } else { format!("{first}\ncolumns {}", columns.join(", ")) }
    }
}

/// What a parse gave: the index of the manifest it wrote, if it wrote one;
/// its failure, if it failed; and the errors and warnings of its log, if it
/// ran to its end.
#[derive(Debug)]
pub struct Outcome {
    pub index: Option<ParseIndex>,
    pub failure: Option<String>,
    pub problems: Option<Vec<parse_log::Problem>>,
}

/// Where a parse's outcome goes, once, from the parse's thread: the front
/// end hands it to `Workspace::parsed`.
pub type ReportFn = Box<dyn FnOnce(Outcome) + Send>;

/// Runs RUN, a `dbt parse`, as `run::start` runs it, and hands REPORT the
/// outcome: a failure words dbt's first error event, else the first line
/// that is none of its events, as a launcher's own, and names the log; one
/// that ends with no outcome, as on a panic, fails.
pub fn start(run: Run, report: ReportFn) -> Running {
    let read = |output: &Output| {
        let first = output.error_event().or_else(|| output.line().map(str::to_owned));
        let failure = match &output.ending {
            Ending::Exited(status) if status.success() => None,
            Ending::Exited(status) => Some(first.map_or_else(
                || format!("dbt parse {status}"),
                |first| format!("{first} (see {})", output.log.display()),
            )),
            Ending::Stopped => Some("dbt parse was stopped, or ran past its deadline".into()),
            Ending::NotStarted(why) => Some(why.clone()),
        };
        let read = parse_log::read(&output.text);
        let problems = read.completed.then_some(read.problems);
        Outcome { index: output.manifest.as_ref().map(ParseIndex::of), failure, problems }
    };
    let ended = Outcome { index: None, failure: Some("the parse ended without an outcome".into()), problems: None };
    run::start(run, read, ended, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::scratch_project;
    use std::fs::File;
    use std::sync::mpsc;
    use std::time::Instant;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    /// `start` with the Run's parts as it took them before the Run.
    fn start(line: Vec<String>, cwd: Option<PathBuf>, dir: PathBuf, deadline: Duration, report: ReportFn) -> Running {
        super::start(Run { line, cwd, dir, log: PARSE_LOG, deadline }, report)
    }

    /// The command line of the Dbt of SERVER's run of WORDS, as it was made
    /// before the Dbt.
    fn dbt_command(server: &[String], root: &Path, cache: &Path, words: &[String]) -> Option<Vec<String>> {
        Dbt::new(server.to_vec(), None).run(words, root, cache.to_owned(), PARSE_LOG).map(|run| run.line)
    }

    #[test]
    fn parses_with_the_server_s_dbt_and_its_reading_flags_only() {
        let server = words(&[
            "dbt",
            "lsp",
            "--project-dir",
            "/p",
            "--profiles-dir=/d",
            "--target",
            "prod",
            "--log-level",
            "debug",
        ]);
        let parse = PARSE.map(str::to_owned);
        let command = dbt_command(&server, Path::new("/root"), Path::new("/cache"), &parse).unwrap();
        let expected = words(&[
            "dbt",
            "parse",
            "--log-format",
            "json",
            "--profiles-dir",
            "/d",
            "--target",
            "prod",
            "--project-dir",
            "/root",
            "--target-path",
            "/cache/target",
            "--log-path",
            "/cache/logs",
            "--write-json",
            "--no-version-check",
            "--no-send-anonymous-usage-stats",
            "--no-manage-state",
            "--quiet",
        ]);
        assert_eq!(command, expected);
        let wrapped = dbt_command(&words(&["uvx", "dbt", "lsp"]), Path::new("/r"), Path::new("/c"), &parse).unwrap();
        assert_eq!(wrapped[..3], words(&["uvx", "dbt", "parse"]));
        assert_eq!(dbt_command(&words(&["refmux", "--root", "/r"]), Path::new("/r"), Path::new("/c"), &parse), None);
    }

    #[test]
    fn a_dbt_s_run_runs_where_the_dbt_runs_within_the_parse_s_deadline() {
        let dbt = Dbt::new(words(&["dbt", "lsp"]), Some(PathBuf::from("/dbt")));
        let run = dbt.run(&words(&["list"]), Path::new("/root"), PathBuf::from("/cache/run"), RUN_LOG).unwrap();
        let expected = (Some(PathBuf::from("/dbt")), PathBuf::from("/cache/run"), RUN_LOG, PARSE_DEADLINE);
        assert_eq!((run.cwd, run.dir, run.log, run.deadline), expected, "a command's run as the parse");
        // With no directory, the proxy's, as `Run::cwd` says, not the root.
        let dbt = Dbt::new(words(&["dbt", "lsp"]), None);
        let run = dbt.run(&words(&["list"]), Path::new("/root"), PathBuf::from("/cache/run"), RUN_LOG).unwrap();
        assert_eq!(run.cwd, None, "a Dbt with no directory");
    }

    #[test]
    fn a_relative_program_and_profiles_dir_are_taken_from_the_proxy_s_directory() {
        let dir = Path::new("/work/repo");
        let cases = [
            (
                vec![".venv/bin/dbt", "lsp", "--profiles-dir", "config"],
                vec!["/work/repo/.venv/bin/dbt", "lsp", "--profiles-dir", "/work/repo/config"],
            ),
            (
                vec!["dbt", "lsp", "--profiles-dir=config", "--target", "dev"],
                vec!["dbt", "lsp", "--profiles-dir=/work/repo/config", "--target", "dev"],
            ),
            (
                vec!["/opt/dbt", "lsp", "--profiles-dir", "/etc/dbt"],
                vec!["/opt/dbt", "lsp", "--profiles-dir", "/etc/dbt"],
            ),
            (
                vec!["uvx", "dbt", "lsp", "--packages-install-path", "vendor"],
                vec!["uvx", "dbt", "lsp", "--packages-install-path", "vendor"],
            ),
        ];
        for (server, expected) in cases {
            assert_eq!(anchored(&words(&server), dir), words(&expected), "{server:?}");
        }
    }

    #[test]
    fn a_cache_belongs_to_a_root_a_command_and_a_process() {
        let server = words(&["dbt", "lsp"]);
        let one = cache_dir(Path::new("/base"), Path::new("/a"), &server);
        assert!(one.starts_with("/base/dbt-lsp-plus") && one.ends_with(std::process::id().to_string()));
        assert_eq!(one, cache_dir(Path::new("/base"), Path::new("/a"), &server));
        assert_ne!(one, cache_dir(Path::new("/base"), Path::new("/b"), &server));
        assert_ne!(one, cache_dir(Path::new("/base"), Path::new("/a"), &words(&["dbt", "lsp", "--target", "prod"])));
    }

    #[test]
    fn removes_the_caches_unwritten_for_a_day_only() {
        let files = [
            ("key/old/parse.log", ""),
            ("key/new/parse.log", ""),
            ("key/busy/parse.log", ""),
            ("key/busy/run/run.log", ""),
        ];
        let dir = scratch_project("stale-caches", &files);
        let day_ago = std::time::SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
        for old in ["key/old/parse.log", "key/busy/parse.log"] {
            File::options().write(true).open(dir.join(old)).unwrap().set_modified(day_ago).unwrap();
        }
        let own = dir.join("key").join(std::process::id().to_string());
        fs::create_dir_all(&own).unwrap();
        File::create(own.join("parse.log")).unwrap().set_modified(day_ago).unwrap();
        remove_stale(&own);
        assert!(!dir.join("key/old").exists(), "unwritten for a day");
        assert!(dir.join("key/new").exists() && own.exists(), "recent, or this process's own");
        assert!(dir.join("key/busy").exists(), "a run under way, though no parse for a day");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn manifest() -> Value {
        json!({
            "metadata": {"project_name": "shop"},
            "nodes": {
                "model.shop.orders": {"unique_id": "model.shop.orders", "name": "orders", "resource_type": "model",
                    "package_name": "shop", "original_file_path": "models/marts/orders.sql", "access": "protected",
                    "relation_name": "\"shop\".\"main\".\"orders\"", "version": null,
                    "config": {"materialized": "incremental", "incremental_strategy": "merge", "unique_key": "order_id",
                               "tags": ["daily"], "group": "finance", "contract": {"enforced": true}}},
                "model.utils.orders": {"unique_id": "model.utils.orders", "name": "orders", "resource_type": "model",
                    "package_name": "utils", "original_file_path": "models/orders.sql", "relation_name": "u",
                    "config": {"materialized": "view"}},
                "model.shop.eph": {"unique_id": "model.shop.eph", "name": "eph", "resource_type": "model",
                    "package_name": "shop", "original_file_path": "models/eph.sql", "relation_name": null,
                    "config": {"materialized": "ephemeral"}},
                "analysis.shop.counts": {"unique_id": "analysis.shop.counts", "name": "counts",
                    "resource_type": "analysis", "package_name": "shop", "relation_name": "\"shop\".\"main\".\"counts\"",
                    "original_file_path": "analyses/counts.sql", "config": {}},
                "test.shop.not_null_orders_id.1": {"unique_id": "test.shop.not_null_orders_id.1", "resource_type": "test",
                    "package_name": "shop", "attached_node": "model.shop.orders", "relation_name": null,
                    "original_file_path": "models/marts/_marts.yml", "config": {}},
                "test.shop.unique_raw_customers_id.2": {"unique_id": "test.shop.unique_raw_customers_id.2",
                    "resource_type": "test", "package_name": "shop", "attached_node": null,
                    "depends_on": {"nodes": ["source.shop.raw.customers"]}, "original_file_path": "models/s.yml", "config": {}},
                "model.shop.dim.v1": {"unique_id": "model.shop.dim.v1", "name": "dim", "resource_type": "model",
                    "package_name": "shop", "version": 1, "latest_version": 2, "relation_name": "v1", "config": {}},
                "model.shop.dim.v2": {"unique_id": "model.shop.dim.v2", "name": "dim", "resource_type": "model",
                    "package_name": "shop", "version": 2, "latest_version": 2, "relation_name": "v2",
                    "config": {"materialized": "incremental", "unique_key": ["id", "day"]}}
            },
            "sources": {
                "source.shop.raw.customers": {"unique_id": "source.shop.raw.customers", "name": "customers",
                    "source_name": "raw", "resource_type": "source", "package_name": "shop",
                    "relation_name": "\"shop\".\"main\".\"raw_customers\"", "loaded_at_field": "_loaded_at",
                    "freshness": {"warn_after": {"count": 1, "period": "day"}}, "config": {}},
                "source.shop.raw.orders": {"unique_id": "source.shop.raw.orders", "name": "orders",
                    "source_name": "raw", "resource_type": "source", "package_name": "shop", "relation_name": "o",
                    "freshness": {"error_after": {"count": null, "period": null}, "warn_after": {"count": null, "period": null}},
                    "config": {}}
            },
            "disabled": {"model.shop.off": [{"unique_id": "model.shop.off", "name": "off", "resource_type": "model",
                "package_name": "shop", "relation_name": "\"shop\".\"main\".\"off\"", "config": {"materialized": "table"}}]},
            "groups": {"group.shop.finance": {"name": "finance", "owner": {"name": "Ana", "email": "ana@example.invalid"}}}
        })
    }

    #[test]
    fn indexes_relations_tests_and_disabled_nodes_as_dbt_gives_them() {
        let index = ParseIndex::of(&manifest());
        let orders = index.node("model.shop.orders").unwrap();
        assert_eq!(orders["relation"], "\"shop\".\"main\".\"orders\"");
        assert_eq!(orders["tests"], json!(["test.shop.not_null_orders_id.1"]));
        assert_eq!(index.node("model.shop.eph").unwrap()["relation"], Value::Null);
        assert_eq!(index.node("analysis.shop.counts").unwrap()["relation"], Value::Null, "an analysis is never built");
        assert_eq!(index.node("model.shop.off").unwrap()["disabled"], json!(true));
        assert_eq!(index.node("model.shop.off").unwrap()["relation"], Value::Null, "dbt builds no disabled node");
        let customers = index.node("source.shop.raw.customers").unwrap();
        assert_eq!(customers["tests"], json!(["test.shop.unique_raw_customers_id.2"]), "a test on a source");
        assert_eq!(index.node("source.shop.raw.customers").unwrap()["relation"], "\"shop\".\"main\".\"raw_customers\"");
    }

    #[test]
    fn finds_the_one_node_of_a_file_of_the_root() {
        let index = ParseIndex::of(&manifest());
        let id = |node: Option<&Value>| node.map(|node| node["unique_id"].as_str().unwrap().to_owned());
        assert_eq!(id(index.of_file("models/marts/orders.sql")).as_deref(), Some("model.shop.orders"));
        assert_eq!(index.of_file("models/marts/_marts.yml").map(|_| ()), Some(()), "one test only");
        assert_eq!(index.of_file("models/orders.sql"), None, "a package's file");
    }

    #[test]
    fn lists_the_enabled_nodes_and_sources_as_dbt_lsp_would() {
        let node = |id: &str, depends_on: &[&str]| {
            let kind = id.split('.').next().unwrap();
            json!({"unique_id": id, "name": id.rsplit('.').next().unwrap(), "resource_type": kind,
                   "package_name": "shop", "original_file_path": "models/x.sql", "depends_on": {"nodes": depends_on}})
        };
        let manifest = json!({"metadata": {"project_name": "shop"},
            "nodes": {"model.shop.a": node("model.shop.a", &["source.shop.raw.b"]),
                      "test.shop.t": node("test.shop.t", &["model.shop.a"]),
                      "operation.shop.hook": node("operation.shop.hook", &[])},
            "sources": {"source.shop.raw.b": node("source.shop.raw.b", &[])},
            "disabled": {"model.shop.off": [node("model.shop.off", &[])]}});
        let listed = ParseIndex::of(&manifest).listed();
        let ids: Vec<&str> = listed.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["model.shop.a", "source.shop.raw.b", "test.shop.t"], "no disabled node, no hook");
        assert_eq!(listed[0].depends_on, ["source.shop.raw.b"]);
        // A failed parse's manifest, laid over, lacks the model deleted since.
        let mut index = ParseIndex::of(&manifest);
        let newer = json!({"metadata": {"project_name": "shop"}, "sources": {"source.shop.raw.b": node("source.shop.raw.b", &[])}});
        index.overlay(ParseIndex::of(&newer));
        let ids: Vec<String> = index.listed().into_iter().map(|node| node.id).collect();
        assert_eq!(ids, ["source.shop.raw.b"]);
        assert!(index.node("model.shop.a").is_some(), "the index keeps it for nodeInfo");
    }

    #[test]
    fn a_node_s_children_are_the_enabled_nodes_of_the_last_manifest_that_depend_on_it() {
        let node = |id: &str, depends_on: &[&str]| {
            let (kind, name) = (id.split('.').next().unwrap(), id.rsplit('.').next().unwrap());
            json!({"unique_id": id, "name": name, "resource_type": kind, "package_name": "shop",
                   "depends_on": {"nodes": depends_on}})
        };
        let (a, ab, t) = (
            node("model.shop.a", &[]),
            node("model.shop.ab", &["model.shop.a"]),
            node("test.shop.t", &["model.shop.a"]),
        );
        // A relationships test, attached to b, depends on the model it refers to too.
        let mut r = node("test.shop.r", &["model.shop.a", "model.shop.b"]);
        r["attached_node"] = json!("model.shop.b");
        let manifest = json!({"nodes": {"model.shop.a": a, "model.shop.b": node("model.shop.b", &["model.shop.a"]),
                                        "model.shop.ab": ab, "test.shop.t": t, "test.shop.r": r},
                              "disabled": {"model.shop.c": [node("model.shop.c", &["model.shop.a"])]}});
        let mut index = ParseIndex::of(&manifest);
        assert_eq!(
            (index.children("model.shop.a"), index.children("model.shop.b")),
            (vec!["model.shop.ab", "model.shop.b"], vec![])
        );
        // A failed parse's manifest, laid over, lacks b, which the index keeps.
        index.overlay(ParseIndex::of(&json!({"nodes": {"model.shop.a": a, "model.shop.ab": ab, "test.shop.t": t}})));
        assert_eq!(index.children("model.shop.a"), ["model.shop.ab"]);
    }

    #[test]
    fn a_failed_parse_s_manifest_lays_over_the_last_index() {
        let mut index = ParseIndex::of(&manifest());
        let newer = json!({"metadata": {"project_name": "shop"}, "nodes": {"model.shop.orders": {
            "unique_id": "model.shop.orders", "resource_type": "model", "relation_name": "x", "config": {}}}});
        index.overlay(ParseIndex::of(&newer));
        assert_eq!(index.node("model.shop.orders").unwrap()["relation"], "x");
        assert!(index.node("model.shop.eph").is_some(), "what the newer manifest lacks stays");
    }

    #[test]
    fn summarises_what_dbt_builds_then_what_is_set() {
        let index = ParseIndex::of(&manifest());
        let orders = index.summary(index.node("model.shop.orders").unwrap());
        assert_eq!(
            orders,
            "model incremental strategy merge, unique key order_id, \"shop\".\"main\".\"orders\"\n\
             group finance (owner Ana); contract enforced; tags daily; 1 test"
        );
        assert_eq!(index.summary(index.node("model.shop.eph").unwrap()), "model ephemeral, no relation");
        let source = index.summary(index.node("source.shop.raw.customers").unwrap());
        assert_eq!(source, "source, \"shop\".\"main\".\"raw_customers\", freshness, loaded at _loaded_at\n1 test");
        assert_eq!(index.summary(index.node("source.shop.raw.orders").unwrap()), "source, o", "no freshness set");
        let dim = index.summary(index.node("model.shop.dim.v2").unwrap());
        assert_eq!(dim, "model incremental unique key id, day, v2\nversion 2");
        assert_eq!(index.summary(index.node("model.shop.off").unwrap()), "model table, no relation\ndisabled");
    }

    #[test]
    fn the_facts_give_the_id_the_properties_file_and_the_declared_columns() {
        let columns = json!({"order_id": {"name": "order_id", "data_type": "integer"}, "status": {"name": "status"}});
        let untyped =
            json!({"status": {"name": "status", "data_type": null}, "amount": {"name": "amount", "data_type": null}});
        let manifest = json!({"metadata": {"project_name": "shop"}, "nodes": {
            "model.shop.orders": {"unique_id": "model.shop.orders", "resource_type": "model", "name": "orders",
                "package_name": "shop", "patch_path": "shop://models/marts/_orders.yml", "columns": columns,
                "config": {}},
            "model.shop.bare": {"unique_id": "model.shop.bare", "resource_type": "model", "name": "bare",
                "package_name": "shop", "original_file_path": "models/bare.sql", "columns": {}, "config": {}},
            "model.shop.untyped": {"unique_id": "model.shop.untyped", "resource_type": "model", "name": "untyped",
                "package_name": "shop", "columns": untyped, "config": {}},
            "model.utils.dates": {"unique_id": "model.utils.dates", "resource_type": "model", "name": "dates",
                "package_name": "utils", "patch_path": "utils://models/_dates.yml", "config": {}},
            "model.shop.odd": {"unique_id": "model.shop.odd", "resource_type": "model", "name": "odd",
                "package_name": "shop", "config": {},
                "columns": {"a`b": {"data_type": "`x`"}, "tags": {"data_type": "array<string>"}}},
            "model.shop.lines": {"unique_id": "model.shop.lines", "resource_type": "model", "name": "lines",
                "package_name": "shop", "config": {},
                "columns": {" id ": {}, "nested": {"data_type": "STRUCT<\r\n  a INT64\n>\n"}}}},
            "sources": {"source.shop.raw.customers": {"unique_id": "source.shop.raw.customers",
                "resource_type": "source", "name": "customers", "source_name": "raw", "package_name": "shop",
                "original_file_path": "models/staging/_sources.yml"},
              "source.utils.raw.days": {"unique_id": "source.utils.raw.days", "resource_type": "source",
                "name": "days", "source_name": "raw", "package_name": "utils",
                "original_file_path": "models/_sources.yml"}}});
        let index = ParseIndex::of(&manifest);
        let facts = |id: &str| index.facts(index.node(id).unwrap());
        assert_eq!(
            facts("model.shop.orders"),
            "`model.shop.orders`, properties `models/marts/_orders.yml`\ncolumns `order_id` `integer`, `status`"
        );
        assert_eq!(facts("model.shop.bare"), "`model.shop.bare`", "a model's own file is no properties file");
        assert_eq!(
            facts("source.shop.raw.customers"),
            "`source.shop.raw.customers`, properties `models/staging/_sources.yml`"
        );
        assert_eq!(facts("model.shop.untyped"), "`model.shop.untyped`\ncolumns `amount`, `status`");
        // A package's files, from its directory, which the package names.
        assert_eq!(facts("model.utils.dates"), "`model.utils.dates`, properties `utils://models/_dates.yml`");
        // Verbatim inside spans that markdown shows literally, backticks included.
        assert_eq!(facts("model.shop.odd"), "`model.shop.odd`\ncolumns ``a`b`` `` `x` ``, `tags` `array<string>`");
        // A line break as a space, so that one span holds the value; a space
        // at an end kept inside the padding that markdown strips.
        assert_eq!(
            facts("model.shop.lines"),
            "`model.shop.lines`\ncolumns `  id  `, `nested` ` STRUCT<   a INT64 >  `"
        );
        assert_eq!(facts("source.utils.raw.days"), "`source.utils.raw.days`, properties `utils://models/_sources.yml`");
    }

    #[test]
    fn a_parse_reports_its_manifest_or_is_killed_past_its_deadline() {
        let dir = scratch_project("runner", &[("x", "")]);
        let write =
            format!("mkdir -p '{0}/target' && echo '{{\"nodes\": {{}}}}' > '{0}/target/manifest.json'", dir.display());
        let (sender, outcomes) = mpsc::channel();
        let report =
            |sender: mpsc::Sender<Outcome>| -> ReportFn { Box::new(move |outcome| drop(sender.send(outcome))) };
        let running =
            start(words(&["sh", "-c", &write]), None, dir.clone(), Duration::from_secs(10), report(sender.clone()));
        let written = outcomes.recv().unwrap();
        running.stop();
        assert!(written.failure.is_none() && written.index.is_some(), "{written:?}");
        let started = Instant::now();
        let killed =
            start(words(&["sleep", "30"]), None, dir.clone(), Duration::from_millis(200), report(sender.clone()));
        let late = outcomes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(late.index.is_none(), "a manifest left by an earlier run does not count");
        assert!(late.failure.unwrap().contains("deadline") && started.elapsed() < Duration::from_secs(3));
        killed.stop();
        start(words(&["sleep", "30"]), None, dir.clone(), Duration::from_secs(60), report(sender.clone())).stop();
        assert!(outcomes.recv_timeout(Duration::from_secs(5)).unwrap().failure.unwrap().contains("stopped"));
        // dbt's version comes first; its first error says why.
        let json = r#"echo '{"info": {"code": "A001", "level": "info", "msg": "Running with dbt=2.0.9"}}'
            echo '{"info": {"code": "1501", "level": "error", "msg": "[error] [JinjaError (dbt1501)]: Failed"}}'
            echo '{"info": {"code": "1048", "level": "error", "msg": "[error] [DependencyNotFound (dbt1048)]"}}'
            exit 1"#;
        let failing =
            start(words(&["sh", "-c", json]), None, dir.clone(), Duration::from_secs(10), report(sender.clone()));
        let failure = outcomes.recv_timeout(Duration::from_secs(5)).unwrap().failure.unwrap();
        failing.stop();
        assert!(
            failure.starts_with("[JinjaError (dbt1501)]: Failed (see ") && failure.ends_with("parse.log)"),
            "{failure}"
        );
        // A launcher fails before dbt says anything.
        let launcher = "echo; echo 'error: Failed to fetch the index' >&2; exit 2";
        let failing = start(words(&["sh", "-c", launcher]), None, dir.clone(), Duration::from_secs(10), report(sender));
        let failure = outcomes.recv_timeout(Duration::from_secs(5)).unwrap().failure.unwrap();
        failing.stop();
        assert!(failure.starts_with("error: Failed to fetch the index (see "), "{failure}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_parse_that_ran_to_its_end_says_what_it_found() {
        let dir = scratch_project("runner-problems", &[("x", "")]);
        let (sender, outcomes) = mpsc::channel();
        let report =
            |sender: mpsc::Sender<Outcome>| -> ReportFn { Box::new(move |outcome| drop(sender.send(outcome))) };
        let error =
            r#"echo '{"info": {"code": "1048", "level": "error", "msg": "[error] [DependencyNotFound (dbt1048)]"}}'"#;
        let ended =
            format!(r#"{error}; echo '{{"info": {{"code": "Q039", "level": "info", "msg": "Finished"}}}}'; exit 1"#);
        let complete =
            start(words(&["sh", "-c", &ended]), None, dir.clone(), Duration::from_secs(10), report(sender.clone()));
        let found = outcomes.recv_timeout(Duration::from_secs(5)).unwrap().problems;
        complete.stop();
        assert_eq!(found.map(|problems| problems[0].code.clone()), Some("1048".to_owned()));
        // Dead, or killed, before its end, it says nothing of what it did not reach.
        let died = format!("{error}; exit 101");
        let dead =
            start(words(&["sh", "-c", &died]), None, dir.clone(), Duration::from_secs(10), report(sender.clone()));
        assert_eq!(outcomes.recv_timeout(Duration::from_secs(5)).unwrap().problems, None);
        dead.stop();
        let cut = format!("{error}; exec sleep 30");
        let killed = start(words(&["sh", "-c", &cut]), None, dir.clone(), Duration::from_millis(300), report(sender));
        assert_eq!(outcomes.recv_timeout(Duration::from_secs(5)).unwrap().problems, None);
        killed.stop();
        fs::remove_dir_all(&dir).unwrap();
    }
}
