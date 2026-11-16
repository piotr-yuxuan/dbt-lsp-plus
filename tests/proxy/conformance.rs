// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy's resolution against dbt's.  Opt-in, since it needs dbt 2:
//!
//!     DBT_LSP_PLUS_TEST_DBT=/path/to/dbt cargo test --test proxy conformance -- --ignored
//!
//! Each project of `tests/fixtures/conformance/` is parsed by that dbt as
//! the proxy parses, offline, and what the proxy's library resolves in it
//! is compared with the manifest.  Every divergence must be registered in
//! `divergences.txt` with its reason, and every registered one must still
//! diverge, so that the registry is the exact list of gaps.

use crate::support::scratch;
use dbt_lsp_plus::graph::Graph;
use dbt_lsp_plus::project::{self, Project};
use dbt_lsp_plus::{config, navigation, properties, scan, sources};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// What one lookup answers otherwise than dbt: the subject, the lookup
/// (`scan`, `location`, `navigation` or `graph`) and what each side gives.
type Divergence = (String, &'static str, String);

/// The sections of the manifest that hold what the proxy may name.
const SECTIONS: [&str; 5] = ["nodes", "sources", "functions", "macros", "docs"];

/// The labels of the entries of a properties file that patch a resource.
const PATCHED: [&str; 5] = ["model", "seed", "snapshot", "analysis", "macro"];

#[test]
#[ignore = "needs DBT_LSP_PLUS_TEST_DBT naming a dbt 2 executable"]
fn corpus() {
    let dbt = std::env::var("DBT_LSP_PLUS_TEST_DBT").expect("DBT_LSP_PLUS_TEST_DBT names a dbt 2 executable");
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conformance");
    let namespace = Command::new("unshare").args(["-rn", "true"]).status().is_ok_and(|status| status.success());
    let entries = std::fs::read_dir(&corpus).expect("read the corpus").flatten();
    let mut cases: Vec<PathBuf> = entries.map(|entry| entry.path()).filter(|path| path.is_dir()).collect();
    cases.sort();
    let (mut found, mut versions) = (BTreeSet::new(), BTreeSet::new());
    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let (copy, manifest) = parse(&dbt, case, namespace);
        versions.insert(manifest["metadata"]["dbt_version"].as_str().unwrap_or("of no version").to_owned());
        let project = project::scan(&copy, &HashMap::new()).expect("scan the case");
        let compared = [
            nodes_compared(&project, &manifest),
            calls_compared(&project, &manifest),
            entries_compared(&project, &manifest),
        ];
        for (subject, lookup, answers) in compared.into_iter().flatten() {
            found.insert(format!("{name}\t{subject}\t{lookup}\t{answers}"));
        }
    }
    let registry = std::fs::read_to_string(corpus.join("divergences.txt")).expect("read the registry");
    let lines = registry.lines().filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
    let registered: BTreeSet<String> = lines
        .map(|line| {
            let fields: Vec<&str> = line.splitn(5, '\t').collect();
            assert!(fields.len() == 5 && !fields[4].trim().is_empty(), "a registry line without its reason: {line:?}");
            fields[..4].join("\t")
        })
        .collect();
    let unregistered: Vec<&str> = found.difference(&registered).map(String::as_str).collect();
    let stale: Vec<&str> = registered.difference(&found).map(String::as_str).collect();
    let cut = if namespace { "yes" } else { "no" };
    let versions: Vec<String> = versions.into_iter().collect();
    // Past the harness's capture of `eprintln!`, so that a run that passes says so.
    let _ = writeln!(
        std::io::stderr(),
        "conformance: dbt {}, network namespace: {cut}, {} cases, {} divergences",
        versions.join(" and "),
        cases.len(),
        found.len()
    );
    assert!(
        unregistered.is_empty() && stale.is_empty(),
        "divergences to fix, or to register with a reason:\n{}\nregistered, but no longer diverging:\n{}",
        unregistered.join("\n"),
        stale.join("\n")
    );
}

/// The oracle: CASE copied to a scratch directory and parsed there by DBT as
/// the proxy parses it, under a network namespace when NAMESPACE, in an
/// empty environment but `PATH` and a scratch home; the copy, and the
/// manifest that dbt wrote.  It fails when dbt runs past the proxy's
/// deadline, changes the copy, or exits otherwise than the case's
/// `expected-error` says (dbt's code, a space, then the start of its
/// message).
fn parse(dbt: &str, case: &Path, namespace: bool) -> (PathBuf, Value) {
    let name = case.file_name().unwrap().to_string_lossy().into_owned();
    let dir = scratch(&format!("conformance/{name}"));
    let copy = dir.join("project");
    let copied = Command::new("cp").arg("-R").arg(case).arg(&copy).status().expect("run cp");
    assert!(copied.success(), "copy {case:?}");
    let before = files(&copy);
    let server = [dbt.to_owned(), "lsp".into(), "--profiles-dir".into(), copy.to_string_lossy().into_owned()];
    let parse = config::parse_command(&server, &copy, &dir.join("cache")).expect("the proxy's parse");
    let cut = if namespace { vec!["unshare".to_owned(), "-rn".to_owned()] } else { Vec::new() };
    let words: Vec<String> = cut.into_iter().chain(parse).collect();
    let (home, log) = (dir.join("home"), dir.join("parse.log"));
    std::fs::create_dir_all(&home).expect("create the scratch home");
    let output = std::fs::File::create(&log).expect("create the parse's log");
    let mut command = Command::new(&words[0]);
    command.args(&words[1..]).env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command.env("HOME", &home).env("XDG_CACHE_HOME", dir.join("xdg")).current_dir(&dir).stdin(Stdio::null());
    command.stdout(output.try_clone().expect("share the log")).stderr(output);
    let mut child = command.spawn().expect("start dbt");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for dbt") {
            break status;
        }
        if started.elapsed() > config::PARSE_DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name}: dbt parse ran past {:?}", config::PARSE_DEADLINE);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let printed = std::fs::read_to_string(&log).unwrap_or_default();
    // A change first: dbt may install packages, and then fail.
    let after = files(&copy);
    let changed: BTreeSet<&PathBuf> =
        before.keys().chain(after.keys()).filter(|path| before.get(*path) != after.get(*path)).collect();
    assert!(changed.is_empty(), "{name}: dbt parse changed {changed:?} in its project:\n{printed}");
    let expected = std::fs::read_to_string(case.join("expected-error")).unwrap_or_default();
    let (code, message) = expected.trim().split_once(' ').unwrap_or((expected.trim(), ""));
    let as_expected = match code {
        "" => status.success(),
        _ => !status.success() && printed.contains(code) && printed.contains(message),
    };
    assert!(as_expected, "{name}: dbt parse exited {status}, expected {:?}:\n{printed}", expected.trim());
    let manifest = std::fs::read_to_string(dir.join("cache/target/manifest.json")).expect("dbt wrote a manifest");
    (copy, serde_json::from_str(&manifest).expect("a manifest of JSON"))
}

/// Every file under DIR, by its path relative to DIR, with its bytes.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        for path in std::fs::read_dir(&next).into_iter().flatten().flatten().map(|entry| entry.path()) {
            if path.is_dir() {
                pending.push(path);
            } else {
                found.insert(path.strip_prefix(dir).unwrap().to_owned(), std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    found
}

/// What the manifest holds under ID, in any of SECTIONS.
fn manifest_entry<'a>(manifest: &'a Value, id: &str) -> Option<&'a Value> {
    SECTIONS.iter().find_map(|section| manifest[section].get(id))
}

/// What dbt and the scan each know in the project's packages, by id, and
/// where each is written: dbt's nodes but its generic tests, sources,
/// functions, macros and docs blocks, against the scan's entities and source
/// tables.
fn nodes_compared(project: &Project, manifest: &Value) -> Vec<Divergence> {
    let ours = |entry: &Value| entry["package_name"].as_str().is_some_and(|p| project.packages.contains_key(p));
    let mut dbt = BTreeMap::new();
    for section in SECTIONS {
        let entries = manifest[section].as_object().into_iter().flatten();
        for (id, entry) in entries.filter(|(_, entry)| ours(entry) && entry["test_metadata"].is_null()) {
            dbt.insert(id.clone(), entry["original_file_path"].as_str().unwrap_or_default().to_owned());
        }
    }
    let relative = |package: &str, path: &Path| {
        path.strip_prefix(&project.packages[package]).unwrap_or(path).to_string_lossy().into_owned()
    };
    let mut scanned: BTreeMap<String, String> =
        project.entities.iter().map(|entity| (entity.id.clone(), relative(&entity.package, &entity.path))).collect();
    let tables = project.sources.iter();
    scanned.extend(
        tables.map(|t| (format!("source.{}.{}.{}", t.package, t.source, t.table), relative(&t.package, &t.path))),
    );
    let mut found = Vec::new();
    for id in dbt.keys().chain(scanned.keys()).collect::<BTreeSet<_>>() {
        let subject = format!("node:{id}");
        match (dbt.get(id), scanned.get(id)) {
            (Some(file), Some(ours)) if file != ours => {
                found.push((subject, "location", format!("proxy {ours}, dbt {file}")))
            }
            (Some(_), None) => found.push((subject, "scan", "only dbt has it".to_owned())),
            (None, Some(_)) => found.push((subject, "scan", "only the proxy has it".to_owned())),
            _ => {}
        }
    }
    found
}

/// Each call that the scan reads, as the graph and, for a `ref` or a
/// `source`, navigation resolve it, against what dbt says it names: those of
/// the caller's dependencies (its `depends_on` nodes, and its macros of the
/// project's packages) that bear the call's name; none for a caller that dbt
/// dropped.  dbt lists for a node what the macros it calls call in turn,
/// and resolves their `ref` and `source` for that node: so a node is
/// compared on its own calls and on the `ref` and `source` of each macro it
/// reaches through the graph; a macro, on its own calls of macros.  Then
/// each dependency that no call of the caller, or of a macro it reaches,
/// names.
fn calls_compared(project: &Project, manifest: &Value) -> Vec<Divergence> {
    let graph = Graph::scanned(project);
    let entities: HashMap<&str, &project::Entity> = project.entities.iter().map(|e| (e.id.as_str(), e)).collect();
    let field = |id: &str, key: &str| {
        manifest_entry(manifest, id).and_then(|entry| entry[key].as_str()).unwrap_or_default().to_owned()
    };
    let dependencies = |id: &str| -> BTreeSet<String> {
        let depends = manifest_entry(manifest, id).map_or(&Value::Null, |entry| &entry["depends_on"]);
        let ids = |key: &'static str| depends[key].as_array().into_iter().flatten().filter_map(Value::as_str);
        let macros = ids("macros").filter(|id| project.packages.contains_key(&field(id, "package_name")));
        ids("nodes").chain(macros).map(str::to_owned).collect()
    };
    // The calls that name DEPENDENCY, as `written` writes them: a dispatch,
    // to any namespace, names each implementation `*__name`, since the
    // root's `dispatch:` may send one namespace to another package.
    let callers = |dependency: &str| -> Vec<String> {
        let (package, name) = (field(dependency, "package_name"), field(dependency, "name"));
        match field(dependency, "resource_type").as_str() {
            "model" | "seed" | "snapshot" => vec![format!("ref('{name}')"), format!("ref('{package}', '{name}')")],
            "source" => vec![format!("source('{}', '{name}')", field(dependency, "source_name"))],
            "macro" => {
                let dispatched = name.rsplit_once("__").map(|(_, name)| name.to_owned());
                let namespaces = || project.packages.keys().map(|namespace| format!(", '{namespace}'"));
                let dispatches = dispatched.iter().flat_map(|name| {
                    std::iter::once(String::new())
                        .chain(namespaces())
                        .map(move |n| format!("adapter.dispatch('{name}'{n})"))
                });
                [format!("{name}()"), format!("{package}.{name}()")].into_iter().chain(dispatches).collect()
            }
            _ => Vec::new(),
        }
    };
    // A call as written, and what navigation looks up for it.
    let written = |target: &scan::Target| match target {
        scan::Target::Ref { package, name } => {
            let quoted = package.iter().chain([name]).map(|word| format!("'{word}'")).collect::<Vec<_>>();
            let named = navigation::Target::Named { kind: "ref", package: package.clone(), name: name.clone() };
            (format!("ref({})", quoted.join(", ")), Some(named))
        }
        scan::Target::Source { source, table } => {
            let table_target = sources::Target::Table { source: source.clone(), table: table.clone() };
            (format!("source('{source}', '{table}')"), Some(navigation::Target::Source(table_target)))
        }
        scan::Target::Macro { package, name } => {
            (format!("{}{name}()", package.iter().map(|p| format!("{p}.")).collect::<String>()), None)
        }
        scan::Target::Dispatch { package, name } => (
            format!("adapter.dispatch('{name}'{})", package.iter().map(|p| format!(", '{p}'")).collect::<String>()),
            None,
        ),
    };
    // ID, then each macro that it reaches through the graph's edges.
    let reached = |id: &str| -> Vec<String> {
        let mut reached = vec![id.to_owned()];
        let mut next = 0;
        while let Some(from) = reached.get(next).cloned() {
            for edge in graph.edges.iter().filter(|edge| edge.from == from && edge.to.starts_with("macro.")) {
                if !reached.contains(&edge.to) {
                    reached.push(edge.to.clone());
                }
            }
            next += 1;
        }
        reached
    };
    // What the graph and navigation answer for CALL, in the file of HOLDER.
    let answers = |holder: &project::Entity, call: &scan::Call, target: Option<navigation::Target>| {
        let edges = graph.edges.iter().filter(|edge| edge.from == holder.id && edge.sites.contains(&call.range));
        let graphed: BTreeSet<String> = edges.map(|edge| edge.to.clone()).collect();
        // A keyword argument after a ref's name, as `v=2`, names a version, which navigation forwards.
        let text = project.texts.get(&holder.path).map_or("", String::as_str);
        let versioned = text.get(call.range.end..).is_some_and(|rest| rest.trim_start().starts_with(','));
        let navigated = target.filter(|_| !versioned).map(|target| {
            navigation::resolve(project, &holder.path, &target).into_iter().map(|f| f.id).collect::<BTreeSet<_>>()
        });
        [("graph", Some(graphed)), ("navigation", navigated)]
    };
    // The subject under which ENTITY compares a call of HOLDER, a `ref` or a
    // `source` when REFERENCE, if it does: a node, its own calls and the
    // `ref` and `source` of the macros it reaches; a macro, its own calls of
    // macros.
    let subject = |entity: &project::Entity, holder: &project::Entity, reference: bool| {
        let own = holder.id == entity.id;
        let compared = if entity.resource_type == "macro" { own && !reference } else { own || reference };
        let via = if own { String::new() } else { format!(" via {}", holder.id) };
        compared.then(|| format!("{}{via}", entity.id))
    };
    let mut found = Vec::new();
    for entity in &project.entities {
        let dependencies: Vec<(Vec<String>, String)> =
            dependencies(&entity.id).into_iter().map(|dependency| (callers(&dependency), dependency)).collect();
        let mut named = BTreeSet::new();
        for holder in reached(&entity.id).iter().filter_map(|id| entities.get(id.as_str())) {
            for call in &holder.calls {
                let (written, target) = written(&call.target);
                let naming = dependencies.iter().filter(|(callers, _)| callers.contains(&written));
                let dbt: BTreeSet<String> = naming.map(|(_, dependency)| dependency.clone()).collect();
                named.extend(dbt.iter().cloned());
                let Some(subject) = subject(entity, holder, target.is_some()) else { continue };
                for (lookup, ours) in answers(holder, call, target) {
                    if let Some(ours) = ours.filter(|ours| *ours != dbt) {
                        found.push((format!("{subject} {written}"), lookup, format!("proxy {ours:?}, dbt {dbt:?}")));
                    }
                }
            }
        }
        for (_, dependency) in dependencies.iter().filter(|(_, dependency)| !named.contains(dependency)) {
            found.push((format!("{} -> {dependency}", entity.id), "scan", "no call that the scan reads".to_owned()));
        }
    }
    found
}

/// `doc('` or `doc("`, YAML's escape included: where navigation reads a doc.
static DOC_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\bdoc\s*\(\s*\\?['"]"#).unwrap());

/// In each properties file, each entry's `name:` that navigation reads, and
/// each `doc()` in that entry, as navigation resolves them, against what
/// dbt did with that file: the node or macro of the entry's name that it
/// patched from there, and the docs block of the doc's name whose text it
/// rendered in the description that holds the call, a column's or the
/// node's own.
fn entries_compared(project: &Project, manifest: &Value) -> Vec<Divergence> {
    let resolved = |path: &Path, target: &navigation::Target| -> BTreeSet<String> {
        navigation::resolve(project, path, target).into_iter().map(|found| found.id).collect()
    };
    let ids = |section: &str, keep: &dyn Fn(&Value) -> bool| -> BTreeSet<String> {
        manifest[section].as_object().into_iter().flatten().filter(|(_, e)| keep(e)).map(|(id, _)| id.clone()).collect()
    };
    // Each `doc()` of ENTRY, written in TEXT, against the description of NODE that holds it.
    let docs_compared = |path: &Path,
                         text: &str,
                         entries: &[properties::Entry],
                         entry: &properties::Entry,
                         node: &Value| {
        let mut found = Vec::new();
        for call in DOC_CALL.find_iter(&text[entry.range.clone()]) {
            let offset = entry.range.start + call.end();
            let column = entries.iter().find(|e| e.label == "column" && e.range.contains(&offset));
            let description =
                column.map_or(&node["description"], |column| &node["columns"][&column.name]["description"]);
            let description = description.as_str().unwrap_or_default();
            let literal: String = text[offset..].chars().take_while(|c| !matches!(c, '\'' | '"' | '\\')).collect();
            let (name, ours) = match navigation::target_at(path, text, offset) {
                Some(navigation::Target::Named { kind: "doc", package, name }) => {
                    let ours = resolved(path, &navigation::Target::Named { kind: "doc", package, name: name.clone() });
                    (name, ours)
                }
                _ => (literal, BTreeSet::new()),
            };
            let rendered = |block: &Value| {
                let contents = block["block_contents"].as_str().unwrap_or_default().trim();
                block["name"] == name.as_str() && !contents.is_empty() && description.contains(contents)
            };
            let dbt = ids("docs", &rendered);
            if ours != dbt {
                let place = column.map_or(String::new(), |column| format!(" column {}", column.name));
                found.push((format!("{place} doc('{name}')"), "navigation", format!("proxy {ours:?}, dbt {dbt:?}")));
            }
        }
        found
    };
    let mut found = Vec::new();
    for (path, text) in project.texts.iter().filter(|(path, _)| sources::is_yaml(path)) {
        let holders = project.packages.iter().filter(|(_, dir)| path.starts_with(dir));
        let Some((package, dir)) = holders.max_by_key(|(_, dir)| dir.components().count()) else { continue };
        let file = format!("{package}://{}", path.strip_prefix(dir).unwrap().to_string_lossy());
        let entries = properties::entries(text);
        for entry in entries.iter().filter(|entry| PATCHED.contains(&entry.label)) {
            let target = navigation::target_at(path, text, entry.name_range.start);
            let ours = target.map_or_else(BTreeSet::new, |target| resolved(path, &target));
            let patched = |e: &Value| e["patch_path"] == file.as_str() && e["name"] == entry.name.as_str();
            let dbt: BTreeSet<String> =
                ["nodes", "macros"].into_iter().flat_map(|section| ids(section, &patched)).collect();
            let subject = format!("{file} {}", entry.name);
            if ours != dbt {
                found.push((subject.clone(), "navigation", format!("proxy {ours:?}, dbt {dbt:?}")));
            }
            let node = dbt.iter().find_map(|id| manifest_entry(manifest, id)).unwrap_or(&Value::Null);
            let docs = docs_compared(path, text, &entries, entry, node);
            found.extend(
                docs.into_iter().map(|(place, lookup, answers)| (format!("{subject}{place}"), lookup, answers)),
            );
        }
    }
    found
}
