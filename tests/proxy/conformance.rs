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
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    let (mut found, mut versions) = (BTreeMap::new(), BTreeSet::new());
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
            found.insert(format!("{name}\t{subject}\t{lookup}"), answers);
        }
    }
    let registry = std::fs::read_to_string(corpus.join("divergences.txt")).expect("read the registry");
    let lines = registry.lines().filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
    let registered: BTreeSet<String> = lines
        .map(|line| {
            let fields: Vec<&str> = line.splitn(4, '\t').collect();
            assert!(fields.len() == 4 && !fields[3].trim().is_empty(), "a registry line without its reason: {line:?}");
            fields[..3].join("\t")
        })
        .collect();
    let unregistered: Vec<String> = found
        .iter()
        .filter(|(key, _)| !registered.contains(*key))
        .map(|(key, answers)| format!("{key}\t{answers}"))
        .collect();
    let stale: Vec<&str> = registered.iter().filter(|key| !found.contains_key(*key)).map(String::as_str).collect();
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

/// Each call of a node or a macro that dbt knows, as the graph and, for a
/// node's `ref` or `source`, navigation resolve it, against what dbt says
/// the call names: those of the caller's dependencies (its `depends_on`
/// nodes, and its macros of the project's packages) that bear the call's
/// name; then each dependency that no call that the scan reads names.
fn calls_compared(project: &Project, manifest: &Value) -> Vec<Divergence> {
    let graph = Graph::scanned(project);
    let field = |id: &str, key: &str| {
        manifest_entry(manifest, id).and_then(|entry| entry[key].as_str()).unwrap_or_default().to_owned()
    };
    let dependencies = |id: &str| -> BTreeSet<String> {
        let depends = manifest_entry(manifest, id).map_or(&Value::Null, |entry| &entry["depends_on"]);
        let ids = |key: &'static str| depends[key].as_array().into_iter().flatten().filter_map(Value::as_str);
        let macros = ids("macros").filter(|id| project.packages.contains_key(&field(id, "package_name")));
        ids("nodes").chain(macros).map(str::to_owned).collect()
    };
    // The calls that name DEPENDENCY, as `written` writes them: a dispatch
    // names each implementation, `*__name`.
    let callers = |dependency: &str| -> Vec<String> {
        let (package, name) = (field(dependency, "package_name"), field(dependency, "name"));
        match field(dependency, "resource_type").as_str() {
            "model" | "seed" | "snapshot" => vec![format!("ref('{name}')"), format!("ref('{package}', '{name}')")],
            "source" => vec![format!("source('{}', '{name}')", field(dependency, "source_name"))],
            "macro" => {
                let dispatched = name.rsplit_once("__").map(|(_, name)| name.to_owned());
                let dispatches = dispatched.iter().flat_map(|name| {
                    [format!("adapter.dispatch('{name}')"), format!("adapter.dispatch('{name}', '{package}')")]
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
    let mut found = Vec::new();
    for entity in project.entities.iter().filter(|entity| manifest_entry(manifest, &entity.id).is_some()) {
        let dependencies: Vec<(Vec<String>, String)> =
            dependencies(&entity.id).into_iter().map(|dependency| (callers(&dependency), dependency)).collect();
        let mut named = BTreeSet::new();
        for call in &entity.calls {
            let (written, target) = written(&call.target);
            let naming = dependencies.iter().filter(|(callers, _)| callers.contains(&written));
            let dbt: BTreeSet<String> = naming.map(|(_, dependency)| dependency.clone()).collect();
            named.extend(dbt.iter().cloned());
            let edges = graph.edges.iter().filter(|edge| edge.from == entity.id && edge.sites.contains(&call.range));
            let graphed: BTreeSet<String> = edges.map(|edge| edge.to.clone()).collect();
            let navigated = target.filter(|_| manifest["nodes"].get(&entity.id).is_some()).map(|target| {
                navigation::resolve(project, &entity.path, &target).into_iter().map(|f| f.id).collect::<BTreeSet<_>>()
            });
            for (lookup, ours) in [("graph", Some(graphed)), ("navigation", navigated)] {
                if let Some(ours) = ours.filter(|ours| *ours != dbt) {
                    found.push((format!("{} {written}", entity.id), lookup, format!("proxy {ours:?}, dbt {dbt:?}")));
                }
            }
        }
        for (_, dependency) in dependencies.iter().filter(|(_, dependency)| !named.contains(dependency)) {
            found.push((format!("{} -> {dependency}", entity.id), "scan", "no call that the scan reads".to_owned()));
        }
    }
    found
}

/// In each properties file, each entry's `name:` that navigation reads, and
/// each `doc()` in that entry, as navigation resolves them, against what
/// dbt did with that file: the node or macro of the entry's name that it
/// patched from there, and the docs block whose text it rendered as that
/// node's description.  A `doc()` counts for the entry that holds it, so
/// the corpus writes one only as a top-level description.
fn entries_compared(project: &Project, manifest: &Value) -> Vec<Divergence> {
    let resolved = |path: &Path, target: &navigation::Target| -> BTreeSet<String> {
        navigation::resolve(project, path, target).into_iter().map(|found| found.id).collect()
    };
    let ids = |section: &str, keep: &dyn Fn(&Value) -> bool| -> BTreeSet<String> {
        manifest[section].as_object().into_iter().flatten().filter(|(_, e)| keep(e)).map(|(id, _)| id.clone()).collect()
    };
    let docs_in = |path: &Path, text: &str, range: Range<usize>| {
        let mut docs = Vec::new();
        for offset in range.filter(|offset| text.is_char_boundary(*offset)) {
            match navigation::target_at(path, text, offset) {
                Some(navigation::Target::Named { kind: "doc", package, name })
                    if !docs.contains(&(package.clone(), name.clone())) =>
                {
                    docs.push((package, name))
                }
                _ => {}
            }
        }
        docs
    };
    let mut found = Vec::new();
    for (path, text) in project.texts.iter().filter(|(path, _)| sources::is_yaml(path)) {
        let holders = project.packages.iter().filter(|(_, dir)| path.starts_with(dir));
        let Some((package, dir)) = holders.max_by_key(|(_, dir)| dir.components().count()) else { continue };
        let file = format!("{package}://{}", path.strip_prefix(dir).unwrap().to_string_lossy());
        for entry in properties::entries(text).into_iter().filter(|entry| PATCHED.contains(&entry.label)) {
            let Some(target) = navigation::target_at(path, text, entry.name_range.start) else { continue };
            let patched = |e: &Value| e["patch_path"] == file.as_str() && e["name"] == entry.name.as_str();
            let dbt: BTreeSet<String> =
                ["nodes", "macros"].into_iter().flat_map(|section| ids(section, &patched)).collect();
            let subject = format!("{file} {}", entry.name);
            let ours = resolved(path, &target);
            if ours != dbt {
                found.push((subject.clone(), "navigation", format!("proxy {ours:?}, dbt {dbt:?}")));
            }
            let node = dbt.iter().find_map(|id| manifest_entry(manifest, id));
            let description = node.and_then(|node| node["description"].as_str()).unwrap_or_default().trim().to_owned();
            for (package, name) in docs_in(path, text, entry.range.clone()) {
                let ours = resolved(path, &navigation::Target::Named { kind: "doc", package, name: name.clone() });
                let rendered =
                    |block: &Value| block["block_contents"].as_str().map(str::trim) == Some(description.as_str());
                let dbt = if description.is_empty() { BTreeSet::new() } else { ids("docs", &rendered) };
                if ours != dbt {
                    found.push((
                        format!("{subject} doc('{name}')"),
                        "navigation",
                        format!("proxy {ours:?}, dbt {dbt:?}"),
                    ));
                }
            }
        }
    }
    found
}
