// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The resolved configuration of a project's nodes, which dbt lsp 2.0.9 does
//! not expose, from the user's own `dbt parse`: the command line that
//! matches the server's, its run in a thread of its own, the index of the
//! manifest it writes, and the summary of a node.

use crate::graph::ServerNode;
use crate::parse_log;
use crate::scan::Target;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The command under which dbt-lsp-plus answers a node's information.
pub const NODE_INFO: &str = "dbt-lsp-plus.nodeInfo";

/// How long a parse may run before it is killed.
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

/// The cut, before a command and the directory it guards: a user namespace
/// that no network reaches, where that directory is mounted read-only, so
/// that dbt's install of packages fails there rather than rewrite it.
/// Recursive first, as a directory holding a mount point needs, where the
/// system has it.  The working directory is then entered again, through
/// the mount: one in that directory would still reach what lies under it;
/// one that cannot be entered again stops the run.
const CUT: [&str; 5] = [
    "unshare",
    "-rnm",
    "sh",
    "-c",
    r#"{ mount -o rbind,ro=recursive -- "$0" "$0" 2>/dev/null || mount -o bind,ro -- "$0" "$0"; } && here=$(pwd -P) && cd -- "$here" && exec "$@""#,
];

/// The words of the proxy's `dbt parse`, as `dbt_command` takes them.
pub const PARSE: [&str; 3] = ["parse", "--log-format", "json"];

/// Starts COMMAND in DIR, when given, with no input and both its outputs in
/// the file at LOG.
pub fn spawn_logged(command: &[String], dir: Option<&Path>, log: &Path) -> std::io::Result<Child> {
    let out = File::create(log)?;
    let err = out.try_clone()?;
    let mut child = Command::new(&command[0]);
    child.args(&command[1..]).stdin(Stdio::null()).stdout(out).stderr(err);
    if let Some(dir) = dir {
        child.current_dir(dir);
    }
    child.spawn()
}

/// The dbt command WORDS, as `PARSE`, that matches
/// SERVER, the command that runs `dbt lsp`: every word before `lsp`, as
/// `uvx dbt` or `env X=1 dbt`, then WORDS; the flags of KEPT after it, as
/// `--flag X` or `--flag=X`; the project at ROOT; its output under CACHE,
/// with neither telemetry nor a check of versions; under the cut when CUT,
/// which guards ROOT.  None when SERVER has no `lsp` word, as a wrapper of
/// its own.
pub fn dbt_command(server: &[String], root: &Path, cache: &Path, cut: bool, words: &[String]) -> Option<Vec<String>> {
    let at = server.iter().position(|word| word == "lsp")?;
    let path = |path: &Path| path.to_string_lossy().into_owned();
    let mut command: Vec<String> = if cut { CUT.map(str::to_owned).into() } else { Vec::new() };
    command.extend(cut.then(|| path(root)));
    command.extend_from_slice(&server[..at]);
    command.extend_from_slice(words);
    let mut rest = server[at + 1..].iter();
    while let Some(word) = rest.next() {
        let (flag, value) = word.split_once('=').map_or((word.as_str(), None), |(flag, value)| (flag, Some(value)));
        if KEPT.contains(&flag) {
            command.push(flag.to_owned());
            command.extend(value.map(str::to_owned).or_else(|| rest.next().cloned()));
        }
    }
    command.extend(["--project-dir".into(), path(root), "--target-path".into(), path(&cache.join("target"))]);
    command.extend(["--log-path".into(), path(&cache.join("logs")), "--write-json".into()]);
    let quiet = ["--no-version-check", "--no-send-anonymous-usage-stats", "--no-manage-state", "--quiet"];
    command.extend(quiet.map(str::to_owned));
    Some(command)
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

/// Whether this system can run a command under the cut: Linux, where an
/// unprivileged user may make a user namespace and mount there.  Probed by
/// `true` under the cut of the system's temporary directory, killed past
/// PROBE_DEADLINE.
pub fn can_cut() -> bool {
    let mut probe = Command::new(CUT[0]);
    probe.args(&CUT[1..]).arg(std::env::temp_dir()).arg("true");
    probe.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let status = probe.spawn().ok().and_then(|child| finish(child, PROBE_DEADLINE, &AtomicBool::new(false)));
    status.is_some_and(|status| status.success())
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

/// Removes the caches beside CACHE that no parse wrote for a day, as a
/// proxy killed by a signal leaves.  No process is asked whether it lives:
/// a home shared between hosts holds the caches of processes this host
/// cannot see, and a parse under way keeps its log fresh.
pub fn remove_stale(cache: &Path) {
    let own = cache.file_name();
    let siblings = cache.parent().and_then(|parent| fs::read_dir(parent).ok()).into_iter().flatten().flatten();
    for sibling in siblings.filter(|sibling| Some(sibling.file_name().as_os_str()) != own) {
        let log = fs::metadata(sibling.path().join("parse.log")).or_else(|_| sibling.metadata());
        let age = log.and_then(|meta| meta.modified()).ok().and_then(|modified| modified.elapsed().ok());
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
pub struct Index {
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

impl Index {
    /// The index of MANIFEST.  dbt gives no relation to an ephemeral model or
    /// a test, and builds no analysis, though it writes one for it.
    pub fn of(manifest: &Value) -> Index {
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
            let built = node["resource_type"] != "analysis" && node["disabled"] != true;
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
        Index { root_package: root_package.to_owned(), nodes, groups, named, latest }
    }

    /// This index with the entries of NEWER laid over it: what a parse that
    /// failed part of the way still found.
    pub fn overlay(&mut self, newer: Index) {
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

    /// The enabled models, seeds and snapshots of the last manifest named
    /// NAME in PACKAGE, each version of a versioned model apart.
    pub fn resources(&self, package: &str, name: &str) -> Vec<&Value> {
        let latest = self.named.get(name).into_iter().flatten().filter(|id| self.latest.contains(*id));
        let resource = |node: &&Value| {
            matches!(node["resource_type"].as_str(), Some("model" | "seed" | "snapshot"))
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

    /// The node that a `ref` or a `source` of TARGET names, as dbt looks it
    /// up: an enabled one first, in the package the call names, else the
    /// root project first, then any.
    pub fn named(&self, target: &Target) -> Option<&Value> {
        let names = |node: &&Value| match target {
            Target::Ref { package, name, .. } => {
                matches!(node["resource_type"].as_str(), Some("model" | "seed" | "snapshot"))
                    && node["name"] == name.as_str()
                    && package.as_deref().is_none_or(|package| node["package_name"] == package)
            }
            Target::Source { source, table } => {
                node["resource_type"] == "source"
                    && node["source_name"] == source.as_str()
                    && node["name"] == table.as_str()
            }
            _ => false,
        };
        let name = match target {
            Target::Ref { name, .. } | Target::Source { table: name, .. } => name,
            _ => return None,
        };
        let candidates = self.named.get(name).into_iter().flatten().filter_map(|id| self.nodes.get(id));
        let mut found: Vec<&Value> = candidates.filter(names).collect();
        // A versioned model without a version named is its latest version.
        let latest = |node: &&Value| node["version"] == node["latest_version"] || node["latest_version"].is_null();
        found.sort_by_key(|node| {
            let package = node["package_name"] != self.root_package.as_str();
            (node["disabled"] == true, package, !latest(node), node["unique_id"].to_string())
        });
        found.first().copied()
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
    /// own; then, if any column is declared, one line of the columns, each
    /// its name and its type when declared, in the manifest's order as read,
    /// which is by name.
    pub fn facts(node: &Value) -> String {
        let patch = node["patch_path"].as_str().map(|path| path.split_once("://").map_or(path, |(_, file)| file));
        let source = || node["original_file_path"].as_str().filter(|_| node["resource_type"] == "source");
        let id = node["unique_id"].as_str().unwrap_or_default();
        let first = patch.or_else(source).map_or_else(|| id.to_owned(), |file| format!("{id}, properties {file}"));
        let columns = node["columns"].as_object().into_iter().flatten().map(|(name, column)| {
            let declared = column["data_type"].as_str().filter(|data_type| !data_type.is_empty());
            declared.map_or_else(|| name.clone(), |data_type| format!("{name} {data_type}"))
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
    pub index: Option<Index>,
    pub failure: Option<String>,
    pub problems: Option<Vec<parse_log::Problem>>,
}

/// A parse under way, which its owner stops at the latest when it ends.
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    /// Runs WORK in a thread of its own, which the flag it gets, set when the
    /// run is stopped, asks to end.
    pub fn spawn(work: impl FnOnce(&AtomicBool) + Send + 'static) -> Running {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        Running { stop, thread: Some(thread::spawn(move || work(&stopped))) }
    }

    /// Stops the parse, killing dbt if it still runs, and waits for the
    /// thread, which has then reported.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

type ReportFn = Box<dyn FnOnce(Outcome) + Send>;

/// Hands a parse's outcome over once: the one the thread sends, else a
/// failure, should the thread end without one.
struct Report(Option<ReportFn>);

impl Drop for Report {
    fn drop(&mut self) {
        if let Some(report) = self.0.take() {
            let failure = Some("the parse ended without an outcome".into());
            report(Outcome { index: None, failure, problems: None });
        }
    }
}

/// Runs COMMAND, a `dbt parse` writing under CACHE, in DIR when given, in a
/// thread of its own, with no input and its output in `CACHE/parse.log`,
/// and hands REPORT the outcome.  dbt is killed past DEADLINE, or when the
/// parse is stopped.
pub fn start(
    command: Vec<String>,
    dir: Option<PathBuf>,
    cache: PathBuf,
    deadline: Duration,
    report: ReportFn,
) -> Running {
    Running::spawn(move |stopped| {
        let mut report = Report(Some(report));
        let (log, manifest) = (cache.join("parse.log"), cache.join("target/manifest.json"));
        let _ = fs::create_dir_all(&cache);
        // Only a manifest this run writes counts.
        let _ = fs::remove_file(&manifest);
        let status = spawn_logged(&command, dir.as_deref(), &log).map(|child| finish(child, deadline, stopped));
        let text = fs::read_to_string(&log).unwrap_or_default();
        // dbt's first error, else the first line that is none of its events,
        // as a launcher's own.
        let read = parse_log::read(&text);
        let mut errors = read.problems.iter().filter(|problem| problem.severity == 1);
        let other = || text.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with('{'));
        let first = errors.next().map(|error| error.message.clone()).or_else(|| other().map(str::to_owned));
        let failure = match status {
            Ok(Some(status)) if status.success() => None,
            Ok(Some(status)) => Some(
                first.map_or_else(|| format!("dbt parse {status}"), |first| format!("{first} (see {})", log.display())),
            ),
            Ok(None) => Some("dbt parse was stopped, or ran past its deadline".into()),
            Err(error) => Some(format!("cannot run {}: {error}", command[0])),
        };
        let manifest = fs::read_to_string(&manifest).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if let Some(report) = report.0.take() {
            let problems = read.completed.then_some(read.problems);
            report(Outcome { index: manifest.as_ref().map(Index::of), failure, problems });
        }
    })
}

/// Waits for CHILD to exit within DEADLINE, unless STOP is set; else kills
/// and reaps it, and gives none.
pub fn finish(mut child: Child, deadline: Duration, stop: &AtomicBool) -> Option<ExitStatus> {
    let end = Instant::now() + deadline;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::scratch_project;
    use std::sync::mpsc;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
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
        let command = dbt_command(&server, Path::new("/root"), Path::new("/cache"), false, &parse).unwrap();
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
        let wrapped =
            dbt_command(&words(&["uvx", "dbt", "lsp"]), Path::new("/r"), Path::new("/c"), false, &parse).unwrap();
        assert_eq!(wrapped[..3], words(&["uvx", "dbt", "parse"]));
        let cut = dbt_command(&words(&["uvx", "dbt", "lsp"]), Path::new("/r"), Path::new("/c"), true, &parse).unwrap();
        assert_eq!(cut[..2], words(&["unshare", "-rnm"]));
        assert_eq!(cut[5..9], words(&["/r", "uvx", "dbt", "parse"]), "the root guarded, then the parse");
        assert_eq!(cut[9..], wrapped[3..]);
        assert_eq!(
            dbt_command(&words(&["refmux", "--root", "/r"]), Path::new("/r"), Path::new("/c"), true, &parse),
            None
        );
    }

    #[test]
    fn under_the_cut_its_directory_is_read_only_from_within_too() {
        let cut = can_cut();
        assert!(cut || !cfg!(target_os = "linux"), "unshare -rnm cannot cut on this Linux");
        if !cut {
            return;
        }
        let dir = scratch_project("cut-within", &[("x", "")]);
        let write = r#"touch relative 2>/dev/null; touch "$(pwd)/absolute" 2>/dev/null; exit 0"#;
        let mut command = Command::new(CUT[0]);
        command.args(&CUT[1..]).arg(&dir).args(["sh", "-c", write]).current_dir(&dir);
        assert!(command.status().unwrap().success());
        assert!(!dir.join("relative").exists() && !dir.join("absolute").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn under_the_cut_a_working_directory_that_cannot_be_entered_again_stops_the_run() {
        if !can_cut() {
            return;
        }
        let dir = scratch_project("cut-gone", &[("guarded/x", ""), ("outside/x", "")]);
        let marker = dir.join("outside/ran");
        // The run's working directory removed before the cut starts.
        let gone = "mkdir gone && cd gone && rmdir ../gone && exec \"$@\"";
        let mut command = Command::new("sh");
        command.args(["-c", gone, "sh"]).args(CUT).arg(dir.join("guarded")).args(["sh", "-c", "touch \"$0\""]);
        command.arg(&marker).current_dir(dir.join("outside")).stderr(Stdio::null());
        assert!(!command.status().unwrap().success());
        assert!(!marker.exists(), "the command ran");
        fs::remove_dir_all(&dir).unwrap();
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
        let dir = scratch_project("stale-caches", &[("key/old/parse.log", ""), ("key/new/parse.log", "")]);
        let day_ago = std::time::SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
        File::options().write(true).open(dir.join("key/old/parse.log")).unwrap().set_modified(day_ago).unwrap();
        let own = dir.join("key").join(std::process::id().to_string());
        fs::create_dir_all(&own).unwrap();
        File::create(own.join("parse.log")).unwrap().set_modified(day_ago).unwrap();
        remove_stale(&own);
        assert!(!dir.join("key/old").exists(), "unwritten for a day");
        assert!(dir.join("key/new").exists() && own.exists(), "recent, or this process's own");
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
        let index = Index::of(&manifest());
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
    fn finds_a_named_node_as_dbt_looks_it_up_and_the_node_of_a_file() {
        let index = Index::of(&manifest());
        let id = |node: Option<&Value>| node.map(|node| node["unique_id"].as_str().unwrap().to_owned());
        let orders = Target::Ref { package: None, name: "orders".into(), version: None };
        assert_eq!(id(index.named(&orders)).as_deref(), Some("model.shop.orders"), "the root project first");
        let theirs = Target::Ref { package: Some("utils".into()), name: "orders".into(), version: None };
        assert_eq!(id(index.named(&theirs)).as_deref(), Some("model.utils.orders"));
        let customers = Target::Source { source: "raw".into(), table: "customers".into() };
        assert_eq!(id(index.named(&customers)).as_deref(), Some("source.shop.raw.customers"));
        let dim = Target::Ref { package: None, name: "dim".into(), version: None };
        assert_eq!(id(index.named(&dim)).as_deref(), Some("model.shop.dim.v2"), "the latest version");
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
        let listed = Index::of(&manifest).listed();
        let ids: Vec<&str> = listed.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["model.shop.a", "source.shop.raw.b", "test.shop.t"], "no disabled node, no hook");
        assert_eq!(listed[0].depends_on, ["source.shop.raw.b"]);
        // A failed parse's manifest, laid over, lacks the model deleted since.
        let mut index = Index::of(&manifest);
        let newer = json!({"metadata": {"project_name": "shop"}, "sources": {"source.shop.raw.b": node("source.shop.raw.b", &[])}});
        index.overlay(Index::of(&newer));
        let ids: Vec<String> = index.listed().into_iter().map(|node| node.id).collect();
        assert_eq!(ids, ["source.shop.raw.b"]);
        assert!(index.node("model.shop.a").is_some(), "the index keeps it for nodeInfo");
    }

    #[test]
    fn an_enabled_node_wins_over_a_disabled_one_of_the_root() {
        let model = |id: &str, package: &str| {
            json!({"unique_id": id, "name": "customers", "resource_type": "model", "package_name": package,
                   "relation_name": format!("\"{package}\""), "config": {}})
        };
        let manifest = json!({"metadata": {"project_name": "shop"},
            "nodes": {"model.utils.customers": model("model.utils.customers", "utils")},
            "disabled": {"model.shop.customers": [model("model.shop.customers", "shop")]}});
        let mut index = Index::of(&manifest);
        let customers = Target::Ref { package: None, name: "customers".into(), version: None };
        assert_eq!(index.named(&customers).unwrap()["unique_id"], "model.utils.customers");
        // A newer manifest's names are found too.
        let newer = json!({"metadata": {"project_name": "shop"},
            "nodes": {"model.shop.fresh": {"unique_id": "model.shop.fresh", "name": "fresh", "resource_type": "model",
                                           "package_name": "shop", "config": {}}}});
        index.overlay(Index::of(&newer));
        let fresh = Target::Ref { package: None, name: "fresh".into(), version: None };
        assert_eq!(index.named(&fresh).unwrap()["unique_id"], "model.shop.fresh");
    }

    #[test]
    fn a_failed_parse_s_manifest_lays_over_the_last_index() {
        let mut index = Index::of(&manifest());
        let newer = json!({"metadata": {"project_name": "shop"}, "nodes": {"model.shop.orders": {
            "unique_id": "model.shop.orders", "resource_type": "model", "relation_name": "x", "config": {}}}});
        index.overlay(Index::of(&newer));
        assert_eq!(index.node("model.shop.orders").unwrap()["relation"], "x");
        assert!(index.node("model.shop.eph").is_some(), "what the newer manifest lacks stays");
    }

    #[test]
    fn summarises_what_dbt_builds_then_what_is_set() {
        let index = Index::of(&manifest());
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
                "patch_path": "shop://models/marts/_orders.yml", "columns": columns, "config": {}},
            "model.shop.bare": {"unique_id": "model.shop.bare", "resource_type": "model", "name": "bare",
                "original_file_path": "models/bare.sql", "columns": {}, "config": {}},
            "model.shop.untyped": {"unique_id": "model.shop.untyped", "resource_type": "model", "name": "untyped",
                "columns": untyped, "config": {}}},
            "sources": {"source.shop.raw.customers": {"unique_id": "source.shop.raw.customers",
                "resource_type": "source", "name": "customers", "source_name": "raw",
                "original_file_path": "models/staging/_sources.yml"}}});
        let index = Index::of(&manifest);
        let facts = |id: &str| Index::facts(index.node(id).unwrap());
        assert_eq!(
            facts("model.shop.orders"),
            "model.shop.orders, properties models/marts/_orders.yml\ncolumns order_id integer, status"
        );
        assert_eq!(facts("model.shop.bare"), "model.shop.bare", "a model's own file is no properties file");
        assert_eq!(
            facts("source.shop.raw.customers"),
            "source.shop.raw.customers, properties models/staging/_sources.yml"
        );
        assert_eq!(facts("model.shop.untyped"), "model.shop.untyped\ncolumns amount, status");
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
