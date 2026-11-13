// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The resolved configuration of a project's nodes, which dbt lsp 2.0.9 does
//! not expose, from the user's own `dbt parse`: the command line that
//! matches the server's, its run in a thread of its own, the index of the
//! manifest it writes, and the summary of a node.

use crate::scan::Target;
use crate::yaml::{self, Node};
use serde_json::{Value, json};
use std::collections::HashMap;
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

/// How long a cache beside this process's may go unwritten before it is
/// taken for a dead process's.
const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);

/// The flags of `dbt lsp` that `dbt parse` takes too and that change what it
/// reads.
const KEPT: [&str; 5] = ["--profiles-dir", "--profile", "--target", "--vars", "--packages-install-path"];

/// The `dbt parse` that matches SERVER, the command that runs `dbt lsp`:
/// every word before `lsp`, as `uvx dbt` or `env X=1 dbt`; the flags of KEPT
/// after it, as `--flag X` or `--flag=X`; the project at ROOT; and its output
/// under CACHE, with neither telemetry nor a check of versions.  None when
/// SERVER has no `lsp` word, as a wrapper of its own.
pub fn parse_command(server: &[String], root: &Path, cache: &Path) -> Option<Vec<String>> {
    let at = server.iter().position(|word| word == "lsp")?;
    let mut command: Vec<String> = server[..at].to_vec();
    command.push("parse".into());
    let mut rest = server[at + 1..].iter();
    while let Some(word) = rest.next() {
        let (flag, value) = word.split_once('=').map_or((word.as_str(), None), |(flag, value)| (flag, Some(value)));
        if KEPT.contains(&flag) {
            command.push(flag.to_owned());
            command.extend(value.map(str::to_owned).or_else(|| rest.next().cloned()));
        }
    }
    let path = |path: &Path| path.to_string_lossy().into_owned();
    command.extend(["--project-dir".into(), path(root), "--target-path".into(), path(&cache.join("target"))]);
    command.extend(["--log-path".into(), path(&cache.join("logs")), "--write-json".into()]);
    let quiet = ["--no-version-check", "--no-send-anonymous-usage-stats", "--no-manage-state", "--quiet"];
    command.extend(quiet.map(str::to_owned));
    Some(command)
}

/// Where this process keeps its parses of ROOT with the server command
/// SERVER, under BASE: one directory per root and command, hashed with
/// FNV-1a, which no build changes, then one per process.
pub fn cache_dir(base: &Path, root: &Path, server: &[String]) -> PathBuf {
    let parts = std::iter::once(root.to_string_lossy().into_owned()).chain(server.iter().cloned());
    let key = parts.fold(0xcbf2_9ce4_8422_2325_u64, |hash, part| {
        part.bytes().chain([0]).fold(hash, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
    });
    base.join("dbt-lsp-plus").join(format!("{key:016x}")).join(std::process::id().to_string())
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

/// Whether dbt can parse the project at ROOT without resolving its packages
/// over the network, which writes `dbt_packages/` and `package-lock.yml`:
/// every package is local, or the lock is no older than the file that names
/// the packages.
pub fn packages_settled(root: &Path) -> bool {
    let modified = |name: &str| fs::metadata(root.join(name)).and_then(|meta| meta.modified()).ok();
    let lock = modified("package-lock.yml");
    ["packages.yml", "dependencies.yml"].into_iter().all(|name| {
        let Ok(text) = fs::read_to_string(root.join(name)) else { return true };
        // A file of comments only, or empty, parses to no document: no packages.
        let local = yaml::parse(&text).is_none_or(|doc| {
            doc.get("packages").map_or(&[][..], Node::items).iter().all(|package| package.get("local").is_some())
        });
        local || lock.is_some_and(|lock| modified(name).is_some_and(|named| lock >= named))
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
        Index { root_package: root_package.to_owned(), nodes, groups, named }
    }

    /// This index with the entries of NEWER laid over it: what a parse that
    /// failed part of the way still found.
    pub fn overlay(&mut self, newer: Index) {
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

    /// The node that a `ref` or a `source` of TARGET names, as dbt looks it
    /// up: an enabled one first, in the package the call names, else the
    /// root project first, then any.
    pub fn named(&self, target: &Target) -> Option<&Value> {
        let names = |node: &&Value| match target {
            Target::Ref { package, name } => {
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
}

/// What a parse gave: the index of the manifest it wrote, if it wrote one,
/// and its failure, if it failed.
#[derive(Debug)]
pub struct Outcome {
    pub index: Option<Index>,
    pub failure: Option<String>,
}

/// A parse under way, which its owner stops at the latest when it ends.
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
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
            report(Outcome { index: None, failure: Some("the parse ended without an outcome".into()) });
        }
    }
}

/// Runs COMMAND, a `dbt parse` writing under CACHE, in a thread of its own,
/// with no input and its output in `CACHE/parse.log`, and hands REPORT the
/// outcome.  dbt is killed past DEADLINE, or when the parse is stopped.
pub fn start(command: Vec<String>, cache: PathBuf, deadline: Duration, report: ReportFn) -> Running {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let mut report = Report(Some(report));
        let (log, manifest) = (cache.join("parse.log"), cache.join("target/manifest.json"));
        let _ = fs::create_dir_all(&cache);
        // Only a manifest this run writes counts.
        let _ = fs::remove_file(&manifest);
        let spawned = File::create(&log).and_then(|out| {
            let err = out.try_clone()?;
            Command::new(&command[0]).args(&command[1..]).stdin(Stdio::null()).stdout(out).stderr(err).spawn()
        });
        // dbt writes a banner of `=` before its `[error]` lines.
        let first_line = || {
            let text = fs::read_to_string(&log).ok()?;
            let lines = || text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('='));
            let line = lines().find(|line| line.starts_with("[error]")).or_else(|| lines().next())?;
            Some(format!("{line} (see {})", log.display()))
        };
        let failure = match spawned.map(|child| finish(child, deadline, &stopped)) {
            Ok(Some(status)) if status.success() => None,
            Ok(Some(status)) => Some(first_line().unwrap_or_else(|| format!("dbt parse {status}"))),
            Ok(None) => Some("dbt parse was stopped, or ran past its deadline".into()),
            Err(error) => Some(format!("cannot run {}: {error}", command[0])),
        };
        let manifest = fs::read_to_string(&manifest).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if let Some(report) = report.0.take() {
            report(Outcome { index: manifest.as_ref().map(Index::of), failure });
        }
    });
    Running { stop, thread: Some(thread) }
}

/// Waits for CHILD to exit within DEADLINE, unless STOP is set; else kills
/// and reaps it, and gives none.
fn finish(mut child: Child, deadline: Duration, stop: &AtomicBool) -> Option<ExitStatus> {
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
        let command = parse_command(&server, Path::new("/root"), Path::new("/cache")).unwrap();
        let expected = words(&[
            "dbt",
            "parse",
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
        let wrapped = parse_command(&words(&["uvx", "dbt", "lsp"]), Path::new("/r"), Path::new("/c")).unwrap();
        assert_eq!(wrapped[..3], words(&["uvx", "dbt", "parse"]));
        assert_eq!(parse_command(&words(&["refmux", "--root", "/r"]), Path::new("/r"), Path::new("/c")), None);
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

    #[test]
    fn packages_are_settled_when_local_or_locked() {
        let dir = scratch_project("settled", &[("packages.yml", "packages:\n  - local: ../utils\n")]);
        assert!(packages_settled(&dir));
        fs::write(dir.join("packages.yml"), "packages:\n  - package: dbt-labs/dbt_utils\n    version: 1.4.1\n")
            .unwrap();
        assert!(!packages_settled(&dir), "a hub package with no lock");
        fs::write(dir.join("package-lock.yml"), "packages: []\n").unwrap();
        assert!(packages_settled(&dir), "a lock no older than packages.yml");
        let older = std::time::SystemTime::now() - Duration::from_secs(3600);
        File::options().write(true).open(dir.join("package-lock.yml")).unwrap().set_modified(older).unwrap();
        assert!(!packages_settled(&dir), "a lock older than packages.yml");
        fs::write(dir.join("packages.yml"), "# packages:\n#   - package: dbt-labs/dbt_utils\n").unwrap();
        fs::remove_file(dir.join("package-lock.yml")).unwrap();
        assert!(packages_settled(&dir), "comments only name no package");
        fs::write(dir.join("dependencies.yml"), "packages:\n  - package: dbt-labs/dbt_utils\n").unwrap();
        assert!(!packages_settled(&dir), "a hub package in dependencies.yml");
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
        let orders = Target::Ref { package: None, name: "orders".into() };
        assert_eq!(id(index.named(&orders)).as_deref(), Some("model.shop.orders"), "the root project first");
        let theirs = Target::Ref { package: Some("utils".into()), name: "orders".into() };
        assert_eq!(id(index.named(&theirs)).as_deref(), Some("model.utils.orders"));
        let customers = Target::Source { source: "raw".into(), table: "customers".into() };
        assert_eq!(id(index.named(&customers)).as_deref(), Some("source.shop.raw.customers"));
        let dim = Target::Ref { package: None, name: "dim".into() };
        assert_eq!(id(index.named(&dim)).as_deref(), Some("model.shop.dim.v2"), "the latest version");
        assert_eq!(id(index.of_file("models/marts/orders.sql")).as_deref(), Some("model.shop.orders"));
        assert_eq!(index.of_file("models/marts/_marts.yml").map(|_| ()), Some(()), "one test only");
        assert_eq!(index.of_file("models/orders.sql"), None, "a package's file");
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
        let customers = Target::Ref { package: None, name: "customers".into() };
        assert_eq!(index.named(&customers).unwrap()["unique_id"], "model.utils.customers");
        // A newer manifest's names are found too.
        let newer = json!({"metadata": {"project_name": "shop"},
            "nodes": {"model.shop.fresh": {"unique_id": "model.shop.fresh", "name": "fresh", "resource_type": "model",
                                           "package_name": "shop", "config": {}}}});
        index.overlay(Index::of(&newer));
        let fresh = Target::Ref { package: None, name: "fresh".into() };
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
    fn a_parse_reports_its_manifest_or_is_killed_past_its_deadline() {
        let dir = scratch_project("runner", &[("x", "")]);
        let write =
            format!("mkdir -p '{0}/target' && echo '{{\"nodes\": {{}}}}' > '{0}/target/manifest.json'", dir.display());
        let (sender, outcomes) = mpsc::channel();
        let report =
            |sender: mpsc::Sender<Outcome>| -> ReportFn { Box::new(move |outcome| drop(sender.send(outcome))) };
        let running = start(words(&["sh", "-c", &write]), dir.clone(), Duration::from_secs(10), report(sender.clone()));
        let written = outcomes.recv().unwrap();
        running.stop();
        assert!(written.failure.is_none() && written.index.is_some(), "{written:?}");
        let started = Instant::now();
        let killed = start(words(&["sleep", "30"]), dir.clone(), Duration::from_millis(200), report(sender.clone()));
        let late = outcomes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(late.index.is_none(), "a manifest left by an earlier run does not count");
        assert!(late.failure.unwrap().contains("deadline") && started.elapsed() < Duration::from_secs(3));
        killed.stop();
        start(words(&["sleep", "30"]), dir.clone(), Duration::from_secs(60), report(sender.clone())).stop();
        assert!(outcomes.recv_timeout(Duration::from_secs(5)).unwrap().failure.unwrap().contains("stopped"));
        let banner = "echo; echo '======= Errors and Warnings ======='; echo '[error] Compilation failed'; exit 1";
        let failing = start(words(&["sh", "-c", banner]), dir.clone(), Duration::from_secs(10), report(sender));
        let failure = outcomes.recv_timeout(Duration::from_secs(5)).unwrap().failure.unwrap();
        failing.stop();
        assert!(
            failure.starts_with("[error] Compilation failed (see ") && failure.ends_with("parse.log)"),
            "{failure}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
