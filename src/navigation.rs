// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Where a name leads, for the definitions that dbt lsp 2.0.9 does not
//! give: the target at a position, as its document alone shows it, and
//! what it names in the project, by dbt 2.0.9's lookup.  Pure.

use crate::config::ParseIndex;
use crate::project::{self, Entity, Project};
use crate::sources;
use crate::yaml::{self, Tree};
use crate::{properties, scan};
use regex::Regex;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// What a position names.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A `source()` argument, or a source's or table's `name:`.
    Source(sources::Target),
    /// A call that dbt resolves, as `Lookup::call` does.
    Call(scan::Target),
    /// A name that dbt looks up among the resources, as the row KIND of
    /// `LOOKUP` says, in the package it gives, if any.
    Named { kind: &'static str, package: Option<String>, name: String },
}

impl Target {
    /// The kind of this target in `experimental.dbtLspPlus`: `source`, `ref`
    /// (a unit test's `model:` too), `macro`, `function`, `doc`, `test`, or
    /// `entry`, the `name:` of an entry, a function's included.
    pub fn kind(&self) -> &'static str {
        match self {
            Target::Source(_) => "source",
            Target::Call(scan::Target::Macro { .. }) => "macro",
            Target::Call(scan::Target::Function { .. }) => "function",
            Target::Call(_) | Target::Named { kind: "unit test model", .. } => "ref",
            Target::Named { kind: kind @ ("doc" | "test"), .. } => kind,
            Target::Named { .. } => "entry",
        }
    }
}

/// Where a target leads: dbt's unique id of what it names, and the range of
/// that name in its file.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub id: String,
    pub path: PathBuf,
    pub range: Range<usize>,
}

/// A package searched for a name.  For a call, dbt 2.0.9 searches the
/// calling file's package first and the root project second, the other
/// way round when `flags: require_ref_searches_node_package_before_root` is
/// a YAML 1.2 false: a plain `false`, `False` or `FALSE`.
#[derive(Clone, Copy)]
enum Step {
    Own,
    Root,
    First,
    Second,
    Any,
}

/// How dbt 2.0.9 looks up a call's name, a `ref` or a `source`.
const CALLS: &[Step] = &[Step::First, Step::Second, Step::Any];

/// How dbt 2.0.9 looks up each kind of name with no package of its own,
/// but a call's: the kind, the resource types it may name, the prefix of
/// the name in their ids, and the packages searched in turn.  A `doc` or
/// generic test names its kind; the `name:` of an entry, the entry's label;
/// a unit test's `model:`, `unit test model`.  A `doc` ignores the calling
/// file's package: past the root, the first package by path, which is the
/// byte order of their names, since dbt installs each at
/// `dbt_packages/<name>`.  An entry describes a resource of its own package
/// only: dbt 2.0.9 calls one naming another package's resource unused,
/// where dbt-core 1.12 patches it.
const LOOKUP: [(&str, &[&str], &str, &[Step]); 9] = [
    ("doc", &["doc"], "", &[Step::Root, Step::Any]),
    ("test", &["macro"], "test_", &[Step::Own, Step::Root]),
    ("unit test model", &["model"], "", &[Step::Own]),
    ("model", &["model"], "", &[Step::Own]),
    ("seed", &["seed"], "", &[Step::Own]),
    ("snapshot", &["snapshot"], "", &[Step::Own]),
    ("analysis", &["analysis"], "", &[Step::Own]),
    ("macro", &["macro"], "", &[Step::Own]),
    ("function", &["function"], "", &[Step::Own]),
];

/// One quote, as YAML writes it inside a quoted scalar too: `''` in single
/// quotes, `\"` in double quotes.
const QUOTE: &str = r#"(?:''|\\?["'])"#;

/// `doc('name')` or `doc('package', 'name')`.
static DOC: LazyLock<Regex> = LazyLock::new(|| {
    let argument = format!(r#"{QUOTE}([^'"\\]*){QUOTE}"#);
    Regex::new(&format!(r"\bdoc\s*\(\s*{argument}(?:\s*,\s*{argument})?\s*\)")).unwrap()
});

/// What the position OFFSET of TEXT, the file at PATH, names, from that
/// text alone: a `source` anywhere; elsewhere than in YAML, a `ref`'s
/// argument, a macro call's name or package, or the name in the tag of a
/// macro or a generic test, which looks itself up; in YAML, a `ref`, a `doc`, a macro call in
/// the Jinja of a value,
/// the `name:` of a navigable entry, a generic test's name, or a unit
/// test's `model:`.
pub fn target_at(path: &Path, text: &str, offset: usize) -> Option<Target> {
    let inside = |range: &Range<usize>| range.start <= offset && offset <= range.end;
    let source = sources::target_at(path, text, offset).map(Target::Source);
    if source.is_some() {
        return source;
    }
    if !sources::is_yaml(path) {
        // A generic test is the macro `test_<name>`, as dbt names it.
        let macro_named = |block: &scan::Symbol| match block.kind {
            scan::Kind::Macro => Some(block.name.clone()),
            scan::Kind::Test => Some(format!("test_{}", block.name)),
            _ => None,
        };
        let blocks = scan::blocks(text).into_iter().filter(|block| inside(&block.name_range));
        if let Some(name) = blocks.filter_map(|block| macro_named(&block)).next() {
            return Some(Target::Call(scan::Target::Macro { package: None, name }));
        }
        let named = |call: &scan::Call| match call.target {
            scan::Target::Ref { .. } | scan::Target::Function { .. } => call.arguments.iter().any(inside),
            scan::Target::Macro { .. } => inside(&call.range),
            _ => false,
        };
        return sources::file_calls(path, text).into_iter().find(named).map(|call| Target::Call(call.target));
    }
    let named = |kind: &'static str, package: Option<&str>, name: &str| Target::Named {
        kind,
        package: package.map(str::to_owned),
        name: name.to_owned(),
    };
    let reference = scan::yaml_calls(text)
        .into_iter()
        .find(|call| matches!(call.target, scan::Target::Ref { .. }) && call.arguments.iter().any(inside))
        .map(|call| Target::Call(call.target));
    let doc = || {
        let call = DOC
            .captures_iter(text)
            .find(|call| [call.get(1), call.get(2)].into_iter().flatten().any(|m| inside(&m.range())))?;
        Some(match call.get(2) {
            Some(name) => named("doc", Some(&call[1]), name.as_str()),
            None => named("doc", None, &call[1]),
        })
    };
    let entry = || {
        let entries = properties::entries(text).into_iter();
        // An entry's label is its kind, when it is one of `LOOKUP`'s.
        let navigable = |label: &str| LOOKUP.iter().any(|(kind, ..)| *kind == label);
        let found = entries.filter(|entry| navigable(entry.label)).find(|entry| inside(&entry.name_range))?;
        Some(named(found.label, None, &found.name))
    };
    let test = || {
        let found = properties::tests(text).into_iter().find(|test| inside(&test.range))?;
        Some(named("test", found.package.as_deref(), &found.name))
    };
    let model = || {
        let root = yaml::parse(text)?;
        let (name, _) = project::unit_test_models(text, &root).into_iter().find(|(_, range)| inside(range))?;
        Some(named("unit test model", None, &name))
    };
    let macro_call = || {
        let called = |call: &scan::Call| match call.target {
            scan::Target::Macro { .. } => inside(&call.range),
            scan::Target::Function { .. } => call.arguments.iter().any(inside),
            _ => false,
        };
        scan::calls(&scan::without_yaml_comments(text)).into_iter().find(called).map(|call| Target::Call(call.target))
    };
    reference.or_else(doc).or_else(entry).or_else(test).or_else(model).or_else(macro_call)
}

/// dbt 2.0.9's lookup over a project, made once for a request or a graph:
/// the lookup flag read once, the resources by id, and the last parse's
/// index, if any.
pub struct Lookup<'a> {
    project: &'a Project,
    /// What the last parse knows that the scan does not: versions, Python
    /// models, snapshots defined in YAML, and which nodes are disabled.
    index: Option<&'a ParseIndex>,
    /// The resources of each id, in order, so that a package's macros read
    /// as a range; two files may give one id, one of them disabled.
    by_id: BTreeMap<&'a str, Vec<&'a Entity>>,
    /// The packages, the root first, then the others in the byte order of
    /// their names, which is the order of their paths, since dbt installs
    /// each at `dbt_packages/<name>`.
    packages: Vec<&'a str>,
    /// Whether the lookup flag puts the root project before the calling
    /// file's package (see `Step`).
    root_first: bool,
}

impl<'a> Lookup<'a> {
    /// The lookup of PROJECT, with INDEX, the last parse's, if any.
    pub fn new(project: &'a Project, index: Option<&'a ParseIndex>) -> Lookup<'a> {
        let root = project.root_package.as_str();
        let config = project.packages.get(root).and_then(|dir| project.text(&dir.join("dbt_project.yml")));
        let root_first = config.and_then(yaml::parse).is_some_and(|config| {
            let value =
                config.get("flags").and_then(|flags| flags.get("require_ref_searches_node_package_before_root"));
            // As dbt reads the flag: neither `fAlse` nor a quoted `'false'`.
            let yaml_false = ["false", "False", "FALSE"];
            matches!(value, Some(Tree::Scalar { value, plain: true, .. }) if yaml_false.contains(&value.as_str()))
        });
        let mut packages: Vec<&str> = project.packages.keys().map(String::as_str).collect();
        packages.sort_by_key(|package| (*package != root, *package));
        let mut by_id: BTreeMap<&str, Vec<&Entity>> = BTreeMap::new();
        for entity in &project.entities {
            by_id.entry(entity.id.as_str()).or_default().push(entity);
        }
        Lookup { project, index, by_id, packages, root_first }
    }

    /// The package of the file at PATH: the installed package whose
    /// directory holds it, else the root project.
    pub fn package_of(&self, path: &Path) -> &'a str {
        let holders = self.project.packages.iter().filter(|(_, dir)| path.starts_with(dir));
        let deepest = holders.max_by_key(|(_, dir)| dir.as_os_str().len());
        deepest.map_or(self.project.root_package.as_str(), |(name, _)| name.as_str())
    }

    /// Where a call of TARGET from the package OWN leads, by dbt 2.0.9's
    /// lookup: in the package the call names; else a `ref`, a `source`, a
    /// function or a dispatch in OWN and the root, in the flag's order, then in any
    /// package; an unqualified macro in OWN, then the root only, since dbt
    /// keeps another package's macros behind its name.  Every resource of
    /// the first of these that has some, but those the last parse disables,
    /// in the order of their paths: one, but where dbt fails on several
    /// (dbt1005), and a dispatch's every implementation (`*__name`).  A
    /// `ref` leads to its file's start, as dbt lsp answers, a macro to its
    /// whole block, as dbt lsp answers too.
    pub fn call(&self, own: &str, target: &scan::Target) -> Vec<Found> {
        let root = self.project.root_package.as_str();
        let (first, second) = if self.root_first { (root, own) } else { (own, root) };
        let steps = match target {
            scan::Target::Ref { package: Some(package), .. }
            | scan::Target::Macro { package: Some(package), .. }
            | scan::Target::Dispatch { package: Some(package), .. }
            | scan::Target::Function { package: Some(package), .. } => vec![Some(package.as_str())],
            scan::Target::Macro { package: None, .. } => vec![Some(own), Some(root)],
            _ => vec![Some(first), Some(second), None],
        };
        let get = |id: String| self.by_id.get(id.as_str()).map_or(&[][..], Vec::as_slice);
        let found = |entity: &Entity| Found {
            id: entity.id.clone(),
            path: entity.path.clone(),
            range: if entity.resource_type == "macro" { entity.range.clone() } else { entity.name_range.clone() },
        };
        // The models, seeds and snapshots that the last parse knows, at their file's start: the one of
        // VERSION, if given, which a model without versions has not; else the latest version, or the one
        // model without versions.
        let parsed = |package: &str, name: &str, version: Option<&str>| -> Vec<Found> {
            let Some(index) = self.index else { return Vec::new() };
            let text = |value: &Value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            };
            let named = |node: &&Value| match version {
                Some(version) => text(&node["version"]).as_deref() == Some(version),
                None => node["latest_version"].is_null() || node["version"] == node["latest_version"],
            };
            index.resources(package, name).into_iter().filter(named).filter_map(|node| self.parsed_at(node)).collect()
        };
        let in_package = |package: &str| -> Vec<Found> {
            match target {
                scan::Target::Ref { name, version, .. } => {
                    // Once a parse knows the name in PACKAGE, its nodes decide: they know versions, Python
                    // models, snapshots defined in YAML, and which of two files of one name dbt keeps.
                    if self.index.is_some_and(|index| !index.resources(package, name).is_empty()) {
                        return parsed(package, name, version.as_deref());
                    }
                    // Else the scan's: a version by its name and version, the latest when none is named, which may
                    // have no file; a model, seed or snapshot when the name has no versions and the call names none.
                    let prefix = format!("model.{package}.{name}.v");
                    let ids = self.by_id.range(prefix.as_str()..).take_while(|(id, _)| id.starts_with(&prefix));
                    let versions: Vec<&Entity> = ids
                        .flat_map(|(_, entities)| entities.iter().copied())
                        .filter(|entity| entity.version.is_some() && entity.name == *name)
                        .collect();
                    let chosen = |entity: &&Entity| match version {
                        Some(version) => entity.version.as_ref() == Some(version),
                        None => entity.latest,
                    };
                    let plain = versions.is_empty() && version.is_none();
                    let kinds = ["model", "seed", "snapshot"].into_iter().filter(|_| plain);
                    let unversioned = kinds.flat_map(|kind| get(format!("{kind}.{package}.{name}")).iter().copied());
                    versions.into_iter().filter(chosen).chain(unversioned).map(found).collect()
                }
                scan::Target::Source { source, table } => {
                    get(format!("source.{package}.{source}.{table}")).iter().map(|entity| found(entity)).collect()
                }
                scan::Target::Macro { name, .. } => {
                    get(format!("macro.{package}.{name}")).iter().map(|entity| found(entity)).collect()
                }
                scan::Target::Function { name, .. } => {
                    get(format!("function.{package}.{name}")).iter().map(|entity| found(entity)).collect()
                }
                scan::Target::Dispatch { name, .. } => {
                    let (prefix, suffix) = (format!("macro.{package}."), format!("__{name}"));
                    let in_package = self.by_id.range(prefix.as_str()..).take_while(|(id, _)| id.starts_with(&prefix));
                    let implementations = in_package.filter(|(id, _)| id.ends_with(&suffix));
                    implementations.flat_map(|(_, entities)| entities.iter().map(|entity| found(entity))).collect()
                }
            }
        };
        let disabled = |found: &Found| self.index.is_some_and(|index| index.disabled(&found.id));
        let in_step = |step: &Option<&str>| {
            let mut found = match step {
                Some(package) => in_package(package),
                None => self.packages.iter().flat_map(|package| in_package(package)).collect(),
            };
            found.retain(|found| !disabled(found));
            found.sort_by(|one, other| (&one.path, one.range.start).cmp(&(&other.path, other.range.start)));
            found
        };
        steps.iter().map(in_step).find(|found| !found.is_empty()).unwrap_or_default()
    }

    /// Where the node of the last parse NODE leads: its name where the scan
    /// holds its id in its file, as a snapshot declared in a properties
    /// file, else its file's start, as dbt lsp answers a `ref`.
    fn parsed_at(&self, node: &Value) -> Option<Found> {
        let (id, file) = (node["unique_id"].as_str()?, node["original_file_path"].as_str()?);
        let path = self.project.packages.get(node["package_name"].as_str()?)?.join(file);
        let scanned = self.by_id.get(id).into_iter().flatten().find(|entity| entity.path == path);
        let range = scanned.map_or(0..0, |entity| entity.name_range.clone());
        Some(Found { id: id.to_owned(), path, range })
    }
}

/// What TARGET, named in the file at PATH, leads to by LOOKUP, dbt 2.0.9's:
/// a call as `Lookup::call` resolves it; a source looked up as a `ref`,
/// every declaration of the first package that has some; any other name in
/// the package it gives, else as `LOOKUP` says, the first that exists, and
/// for an entry of a model, seed or snapshot what the last parse knows that
/// the scan does not, each version of a versioned model, as dbt patches
/// them all from the entry.
pub fn resolve(lookup: &Lookup, path: &Path, target: &Target) -> Vec<Found> {
    let project = lookup.project;
    let (root, own) = (project.root_package.as_str(), lookup.package_of(path));
    let (first, second) = if lookup.root_first { (root, own) } else { (own, root) };
    let packages = |steps: &[Step]| -> Vec<Option<&str>> {
        let package = |step: &Step| match step {
            Step::Own => Some(own),
            Step::Root => Some(root),
            Step::First => Some(first),
            Step::Second => Some(second),
            Step::Any => None,
        };
        steps.iter().map(package).collect()
    };
    let (kind, package, name) = match target {
        Target::Source(source) => {
            let declared = project.sources.iter().filter(|declared| sources::declares(declared, source));
            let found = declared.map(|declared| {
                let (id, range) = match source {
                    sources::Target::Source { source } => {
                        (format!("source.{}.{source}", declared.package), declared.source_range.clone())
                    }
                    sources::Target::Table { source, table } => {
                        (format!("source.{}.{source}.{table}", declared.package), declared.table_range.clone())
                    }
                };
                (declared.package.as_str(), Found { id, path: declared.path.clone(), range })
            });
            return first_step(found.collect(), &packages(CALLS), true);
        }
        Target::Call(call) => return lookup.call(own, call),
        Target::Named { kind, package, name } => (kind, package, name),
    };
    let Some((_, types, prefix, steps)) = LOOKUP.iter().find(|(row, ..)| row == kind) else { return Vec::new() };
    let steps = package.as_deref().map_or_else(|| packages(steps), |package| vec![Some(package)]);
    // A versioned model's entry, or a unit test of it, names each version.
    let matching: Vec<&Entity> = project
        .entities
        .iter()
        .filter(|entity| {
            types.contains(&entity.resource_type)
                && (entity.id == format!("{}.{}.{prefix}{name}", entity.resource_type, entity.package)
                    || entity.version.is_some() && entity.name == *name)
        })
        .collect();
    let every = matching.iter().any(|entity| entity.version.is_some());
    let found = matching.into_iter().map(|entity| {
        let found = Found { id: entity.id.clone(), path: entity.path.clone(), range: entity.name_range.clone() };
        (entity.package.as_str(), found)
    });
    // Once a parse knows the resource of an entry or of a unit test's model, its nodes decide, each version
    // of a versioned model, as dbt patches or tests them all.
    let resource_type = if *kind == "unit test model" { "model" } else { kind };
    let resource = ["model", "seed", "snapshot"].contains(&resource_type) && package.is_none();
    let nodes = lookup.index.filter(|_| resource).map_or_else(Vec::new, |index| index.resources(own, name));
    let typed = nodes.into_iter().filter(|node| node["resource_type"] == resource_type);
    let mut parsed: Vec<Found> = typed.filter_map(|node| lookup.parsed_at(node)).collect();
    if parsed.is_empty() {
        return first_step(found.collect(), &steps, every);
    }
    parsed.sort_by(|one, other| (&one.path, one.range.start).cmp(&(&other.path, other.range.start)));
    parsed
}

/// Where what TARGET, named in the file at PATH, is used, by LOOKUP: each
/// call that resolves to it, as its definition would, at its name, a unit
/// test's `model:` naming it, and a generic test's name in a properties file
/// for the macro `test_<name>`; with DECLARATION, where it is declared
/// too: its file's start or its block, and the `name:` of each properties
/// entry declaring it in its own package.  Each leads to its id, in the
/// order of their paths.
pub fn references<'a>(lookup: &Lookup<'a>, path: &Path, target: &Target, declaration: bool) -> Vec<Found> {
    let declared = resolve(lookup, path, target);
    let ids: HashSet<&str> = declared.iter().map(|found| found.id.as_str()).collect();
    let name = match target {
        Target::Call(
            scan::Target::Ref { name, .. } | scan::Target::Macro { name, .. } | scan::Target::Function { name, .. },
        )
        | Target::Named { name, .. } => name,
        _ => return Vec::new(),
    };
    // What a use at RANGE of FILE leads to, if it is TARGET's.
    let leading = |file: &Path, range: Range<usize>, led: Vec<Found>| {
        let id = led.into_iter().find(|found| ids.contains(found.id.as_str()))?.id;
        Some(Found { id, path: file.to_owned(), range })
    };
    // Calls alike from one package lead alike: each is looked up once.
    let resolved: RefCell<HashMap<(&str, &scan::Target), Option<String>>> = RefCell::default();
    let lead = |own: &'a str, target: &'a scan::Target| {
        let found =
            || lookup.call(own, target).into_iter().find(|found| ids.contains(found.id.as_str())).map(|found| found.id);
        resolved.borrow_mut().entry((own, target)).or_insert_with(found).clone()
    };
    let calls = |file: &Path, scanned: &'a project::Scanned| -> Vec<Found> {
        let called = |call: &&scan::Call| match &call.target {
            scan::Target::Ref { name: called, .. }
            | scan::Target::Macro { name: called, .. }
            | scan::Target::Function { name: called, .. } => called == name,
            _ => false,
        };
        let at = |call: &scan::Call| match &call.target {
            scan::Target::Ref { .. } | scan::Target::Function { .. } => {
                call.arguments.last().cloned().unwrap_or(call.range.clone())
            }
            _ => call.range.end - name.len()..call.range.end,
        };
        let own = lookup.package_of(file);
        scanned
            .calls
            .iter()
            .filter(called)
            .filter_map(|call| Some(Found { id: lead(own, &call.target)?, path: file.to_owned(), range: at(call) }))
            .collect()
    };
    // A unit test's `model:`, a generic test, the macro `test_<name>`, and with DECLARATION an entry's
    // `name:`, those of the name only, looked up as their definitions.
    let test_name = name.strip_prefix("test_");
    let named = |file: &Path, scanned: &project::Scanned| -> Vec<Found> {
        let models = scanned.models.iter().filter(|(model, _)| model == name);
        let models = models.map(|(model, range)| ("unit test model", None, model, range));
        let tests = scanned.tests.iter().filter(|test| test_name == Some(test.name.as_str()));
        let tests = tests.map(|test| ("test", test.package.as_deref(), &test.name, &test.range));
        let entries = scanned.entries.iter().filter(|entry| declaration && entry.name == *name);
        let entries = entries.map(|entry| (entry.label, None, &entry.name, &entry.name_range));
        let found = |(kind, package, named, range): (&'static str, Option<&str>, &String, &Range<usize>)| {
            let target = Target::Named { kind, package: package.map(str::to_owned), name: named.clone() };
            leading(file, range.clone(), resolve(lookup, file, &target))
        };
        models.chain(tests).chain(entries).filter_map(found).collect()
    };
    let files = lookup.project.files.iter().map(|(file, scanned)| (file, &**scanned));
    let mut found: Vec<Found> =
        files.flat_map(|(file, scanned)| [calls(file, scanned), named(file, scanned)].concat()).collect();
    found.extend(declared.into_iter().filter(|_| declaration));
    found.sort_by(|one, other| (&one.path, one.range.start).cmp(&(&other.path, other.range.start)));
    found.dedup_by(|one, other| one.path == other.path && one.range == other.range);
    found
}

/// What a hover on a `doc` or a generic test, TARGET, shows of FOUND, the
/// block it leads to in PROJECT: a docs block's text between its tags, as
/// written, if any; a test's opening tag and its package, in a code block
/// and code spans when MARKDOWN.
pub fn hover(project: &Project, target: &Target, found: &Found, markdown: bool) -> Option<String> {
    let Target::Named { kind: kind @ ("doc" | "test"), .. } = target else { return None };
    let text = project.text(&found.path)?;
    let block = scan::blocks(text).into_iter().find(|block| block.name_range == found.range)?;
    if *kind == "doc" {
        return Some(text[block.inner].trim().to_owned()).filter(|docs| !docs.is_empty());
    }
    let tag = text[block.range.start..block.inner.start].trim_start_matches("{%").trim_end_matches("%}");
    let (tag, package) =
        (tag.trim_matches(['-', '+', ' ', '\t', '\n']), found.id.split('.').nth(1).unwrap_or_default());
    Some(match markdown {
        true => format!("```jinja\n{tag}\n```\n\nin package `{package}`"),
        false => format!("{tag}\n\nin package {package}"),
    })
}

/// Of CANDIDATES, each with its package, those of the first of STEPS (a
/// package, or any) that has some: the first, or every one, deduplicated,
/// when ALL.
fn first_step(mut candidates: Vec<(&str, Found)>, steps: &[Option<&str>], all: bool) -> Vec<Found> {
    candidates.sort_by(|(_, a), (_, b)| (&a.path, a.range.start).cmp(&(&b.path, b.range.start)));
    candidates.dedup_by(|(_, a), (_, b)| a.path == b.path && a.range == b.range);
    let holds = |step: &Option<&str>, package: &str| step.is_none_or(|step| step == package);
    let step = steps.iter().find(|step| candidates.iter().any(|(package, _)| holds(step, package)));
    let found = candidates.into_iter().filter(|(package, _)| step.is_some_and(|step| holds(step, package)));
    let mut found: Vec<Found> = found.map(|(_, found)| found).collect();
    if !all {
        found.truncate(1);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::scratch_project;
    use serde_json::json;
    use std::collections::HashMap;

    const MODELS: &str = "\
version: 2
models:
  - name: orders
    description: '{{ doc(''order_status'') }}'
    columns:
      - name: customer_id
        data_tests:
          - is_even
          - unique
          - not_null
          - pkg_test
          - pkg.pkg_test
          - relationships:
              to: ref('customers')
              field: id
  - name: only_pkg
    description: \"{{ doc(\\\"pkg_only\\\") }}\"
  - name: versioned
    data_tests:
      - relationships:
          to: ref('orders', v=2)
macros:
  - name: test_is_even
  - name: pkg_macro
analyses:
  - name: a
    description: \"{{ doc('pkg', 'order_status') }}\"
seeds:
  - name: raw
snapshots:
  - name: snap
exposures:
  - name: weekly
    depends_on:
      - ref('pkg', 'orders')
      - source('raw', 'orders')
unit_tests:
  - name: t
    model: orders
  - name: u
    model: only_pkg
";

    const PACKAGE_MODELS: &str = "\
version: 2
models:
  - name: orders
    description: \"{{ doc('order_status') }}\"
    data_tests:
      - relationships:
          to: ref('orders')
";

    const SOURCES: &str =
        "version: 2\nsources:\n  - name: raw\n    tables:\n      - name: orders\n      - name: customers\n";

    const EXTERNAL: &str = "version: 2\nsources:\n  - name: ext\n    tables:\n      - name: events\n";

    fn shop(name: &str, root_config: &str) -> PathBuf {
        scratch_project(
            name,
            &[
                ("dbt_project.yml", root_config),
                ("models/orders.sql", "select 1"),
                ("models/customers.sql", "select * from {{ source('raw', 'orders') }} -- {{ doc('order_status') }}"),
                ("models/_models.yml", MODELS),
                ("models/_sources.yml", SOURCES),
                (
                    "models/docs.md",
                    "{% docs order_status %}\nThe root's.\n{% enddocs %}\n{% docs empty %}{% enddocs %}\n",
                ),
                ("analyses/a.sql", "select 1"),
                ("seeds/raw.csv", "id\n1\n"),
                ("snapshots/snap.sql", "{% snapshot snap %}\nselect 1\n{% endsnapshot %}\n"),
                (
                    "macros/tests.sql",
                    "{% macro is_even() %}{% endmacro %}\n\
                     {% test is_even(model, column_name) %}\nselect 1\n{% endtest %}\n\
                     {% test unique(model, column_name) %}\nselect 1\n{% endtest %}\n",
                ),
                ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
                ("dbt_packages/pkg/models/orders.sql", "select 2"),
                ("dbt_packages/pkg/models/only_pkg.sql", "select 3"),
                ("dbt_packages/pkg/models/_pkg.yml", PACKAGE_MODELS),
                ("dbt_packages/pkg/models/_sources.yml", SOURCES),
                ("dbt_packages/pkg/models/_external.yml", EXTERNAL),
                ("dbt_packages/pkg/analyses/a.sql", "select 4"),
                (
                    "dbt_packages/pkg/models/docs.md",
                    "{% docs order_status %}\nThe package's.\n{% enddocs %}\n\
                     {% docs pkg_only %}\nOnly here.\n{% enddocs %}\n",
                ),
                (
                    "dbt_packages/pkg/macros/m.sql",
                    "{% test pkg_test(model) %}\nselect 1\n{% endtest %}\n{% macro pkg_macro() %}{% endmacro %}\n\
                     {% test is_even(model) %}\nselect 1\n{% endtest %}\n",
                ),
                ("dbt_packages/other/dbt_project.yml", "name: other\n"),
                ("dbt_packages/other/models/docs.md", "{% docs pkg_only %}\nThe other's.\n{% enddocs %}\n"),
                ("dbt_packages/other/models/_external.yml", EXTERNAL),
            ],
        )
    }

    /// The target at the first occurrence of NEEDLE in TEXT, DELTA bytes in.
    fn at(path: &Path, text: &str, needle: &str, delta: usize) -> Option<Target> {
        target_at(path, text, text.find(needle).unwrap() + delta)
    }

    fn called(package: Option<&str>, name: &str) -> Option<Target> {
        let (package, name) = (package.map(str::to_owned), name.to_owned());
        Some(Target::Call(scan::Target::Ref { package, name, version: None }))
    }

    fn named(kind: &'static str, package: Option<&str>, name: &str) -> Option<Target> {
        Some(Target::Named { kind, package: package.map(str::to_owned), name: name.to_owned() })
    }

    const YML: &str = "/p/models/_models.yml";

    #[test]
    fn finds_the_calls_of_yaml_from_the_document_alone() {
        let yml = Path::new(YML);
        assert_eq!(at(yml, MODELS, "ref('customers')", 6), called(None, "customers"));
        assert_eq!(at(yml, MODELS, "ref('pkg', 'orders')", 14), called(Some("pkg"), "orders"));
        // A doc's quotes as YAML escapes them in single and double quotes.
        assert_eq!(at(yml, MODELS, "order_status''", 2), named("doc", None, "order_status"));
        assert_eq!(at(yml, MODELS, "pkg_only", 2), named("doc", None, "pkg_only"));
        assert_eq!(at(yml, MODELS, "'pkg', 'order_status'", 9), named("doc", Some("pkg"), "order_status"));
        let table = sources::Target::Table { source: "raw".into(), table: "orders".into() };
        assert_eq!(at(yml, MODELS, "'raw', 'orders'", 9), Some(Target::Source(table)));
        // A ref naming a version, which the last parse resolves.
        let versioned = scan::Target::Ref { package: None, name: "orders".into(), version: Some("2".into()) };
        assert_eq!(at(yml, MODELS, "ref('orders', v=2)", 6), Some(Target::Call(versioned)));
    }

    #[test]
    fn a_macro_call_in_a_yaml_comment_is_no_target() {
        let path = Path::new("/p/dbt_project.yml");
        let hooks = "name: shop\non-run-start: \"{{ log_start() }}\"\n# on-run-end: \"{{ log_end() }}\"\n";
        let macro_named = |name: &str| Some(Target::Call(scan::Target::Macro { package: None, name: name.into() }));
        assert_eq!(at(path, hooks, "log_start", 1), macro_named("log_start"), "a hook's macro");
        assert_eq!(at(path, hooks, "log_end", 1), None, "dbt renders no comment");
    }

    #[test]
    fn finds_the_names_of_yaml_from_the_document_alone() {
        let yml = Path::new(YML);
        assert_eq!(at(yml, MODELS, "name: orders", 8), named("model", None, "orders"));
        assert_eq!(at(yml, MODELS, "name: test_is_even", 8), named("macro", None, "test_is_even"));
        assert_eq!(at(yml, MODELS, "name: a\n", 6), named("analysis", None, "a"));
        assert_eq!(at(yml, MODELS, "name: raw\n", 7), named("seed", None, "raw"));
        assert_eq!(at(yml, MODELS, "name: snap", 7), named("snapshot", None, "snap"));
    }

    #[test]
    fn finds_the_tests_and_unit_test_models_of_yaml_from_the_document_alone() {
        let yml = Path::new(YML);
        assert_eq!(at(yml, MODELS, "- is_even", 4), named("test", None, "is_even"));
        assert_eq!(at(yml, MODELS, "pkg.pkg_test", 6), named("test", Some("pkg"), "pkg_test"));
        assert_eq!(at(yml, MODELS, "model: orders", 9), named("unit test model", None, "orders"));
        // A column's or an exposure's name leads nowhere of its own.
        assert_eq!(at(yml, MODELS, "customer_id", 2), None);
        assert_eq!(at(yml, MODELS, "weekly", 2), None);
    }

    #[test]
    fn finds_a_source_a_ref_and_a_macro_call_in_sql() {
        let sql = Path::new("/p/models/customers.sql");
        let text = "select * from {{ source('raw', 'orders') }} join {{ ref('orders') }} -- {{ doc('order_status') }}";
        let table = sources::Target::Table { source: "raw".into(), table: "orders".into() };
        assert_eq!(at(sql, text, "'orders') }} join", 2), Some(Target::Source(table)));
        let orders = scan::Target::Ref { package: None, name: "orders".into(), version: None };
        assert_eq!(at(sql, text, "ref('orders')", 6), Some(Target::Call(orders)));
        // The keyword: a macro call, as a project may override `ref`.
        let keyword = scan::Target::Macro { package: None, name: "ref".into() };
        assert_eq!(at(sql, text, "ref('orders')", 1), Some(Target::Call(keyword)));
        // dbt has no doc in SQL: a call of a macro, which none defines.
        let doc = scan::Target::Macro { package: None, name: "doc".into() };
        assert_eq!(at(sql, text, "doc(", 1), Some(Target::Call(doc)));
        assert_eq!(at(sql, text, "order_status", 2), None);
    }

    /// Where TARGET, named in the file REL of the project at ROOT, leads:
    /// each id with its file relative to ROOT and the text of its range.
    fn leads(root: &Path, rel: &str, target: Option<Target>) -> Vec<(String, String, String)> {
        let project = project::scan(root, &HashMap::new()).unwrap();
        let found = resolve(&Lookup::new(&project, None), &root.join(rel), &target.unwrap());
        let text = |found: &Found| project.text(&found.path).unwrap()[found.range.clone()].to_owned();
        let relative = |found: &Found| found.path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        found.iter().map(|found| (found.id.clone(), relative(found), text(found))).collect()
    }

    fn lead(id: &str, path: &str, text: &str) -> Vec<(String, String, String)> {
        vec![(id.to_owned(), path.to_owned(), text.to_owned())]
    }

    #[test]
    fn a_call_looks_in_its_file_s_package_first_unless_the_flag_says_root() {
        let root = shop("navigation-calls", "name: shop\n");
        let root_yml = "models/_models.yml";
        let pkg_yml = "dbt_packages/pkg/models/_pkg.yml";
        // A model's file is at its start, as dbt lsp answers a ref.
        assert_eq!(leads(&root, root_yml, called(None, "orders")), lead("model.shop.orders", "models/orders.sql", ""));
        assert_eq!(
            leads(&root, pkg_yml, called(None, "orders")),
            lead("model.pkg.orders", "dbt_packages/pkg/models/orders.sql", "")
        );
        assert_eq!(leads(&root, root_yml, called(Some("pkg"), "orders"))[0].0, "model.pkg.orders");
        assert_eq!(leads(&root, root_yml, called(None, "only_pkg"))[0].0, "model.pkg.only_pkg");
        assert_eq!(leads(&root, root_yml, called(Some("shop"), "only_pkg")), Vec::new());
        let table = Some(Target::Source(sources::Target::Table { source: "raw".into(), table: "orders".into() }));
        assert_eq!(
            leads(&root, root_yml, table.clone()),
            lead("source.shop.raw.orders", "models/_sources.yml", "orders")
        );
        assert_eq!(leads(&root, pkg_yml, table)[0].0, "source.pkg.raw.orders");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_function_is_looked_up_as_a_ref_is() {
        let files = |flag: &str| {
            [
                ("dbt_project.yml", format!("name: r\n{flag}")),
                ("functions/f.sql", "x * 2".into()),
                ("models/c.sql", "select 1".into()),
                ("dbt_packages/pa/dbt_project.yml", "name: pa\n".into()),
                ("dbt_packages/pa/functions/f.sql", "x + 1".into()),
                ("dbt_packages/pa/functions/g.sql", "x + 2".into()),
                ("dbt_packages/pa/models/pm.sql", "select 1".into()),
                ("dbt_packages/pb/dbt_project.yml", "name: pb\n".into()),
                ("dbt_packages/pb/functions/k.sql", "x + 3".into()),
            ]
        };
        let function = |package: Option<&str>, name: &str| {
            Some(Target::Call(scan::Target::Function { package: package.map(str::to_owned), name: name.into() }))
        };
        let ids = |root: &Path, rel: &str, target| -> Vec<String> {
            leads(root, rel, target).into_iter().map(|(id, ..)| id).collect()
        };
        // As both builds resolve them: the caller's package, the root, then any; a qualified call in its package.
        let owned = files("");
        let root = scratch_project("navigation-functions", &owned.each_ref().map(|(p, t)| (*p, t.as_str())));
        let pm = "dbt_packages/pa/models/pm.sql";
        assert_eq!(ids(&root, "models/c.sql", function(None, "f")), ["function.r.f"]);
        assert_eq!(ids(&root, pm, function(None, "f")), ["function.pa.f"]);
        assert_eq!(ids(&root, "models/c.sql", function(None, "g")), ["function.pa.g"]);
        assert_eq!(ids(&root, pm, function(None, "k")), ["function.pb.k"]);
        assert_eq!(ids(&root, "models/c.sql", function(Some("pa"), "f")), ["function.pa.f"]);
        assert_eq!(ids(&root, "models/c.sql", function(Some("r"), "g")), Vec::<String>::new());
        std::fs::remove_dir_all(root).unwrap();
        // The flag puts the root first, for a function as for a ref.
        let flagged = files("flags:\n  require_ref_searches_node_package_before_root: false\n");
        let root = scratch_project("navigation-functions-flag", &flagged.each_ref().map(|(p, t)| (*p, t.as_str())));
        assert_eq!(ids(&root, pm, function(None, "f")), ["function.r.f"]);
        assert_eq!(ids(&root, pm, function(None, "g")), ["function.pa.g"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_snapshot_declared_in_yaml_leads_to_its_name_before_and_after_a_parse() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/x.sql", "select 1 as id"),
            ("models/c.sql", "select * from {{ ref('ys') }}"),
            (
                "snapshots/_s.yml",
                "snapshots:\n  - name: other\n    relation: ref('x')\n  - name: ys\n    relation: ref('x')\n",
            ),
            ("snapshots/old.sql", "{% snapshot old %}\nselect 1\n{% endsnapshot %}\n"),
        ];
        let root = scratch_project("navigation-yaml-snapshot", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let named = |name: &str| Target::Call(scan::Target::Ref { package: None, name: name.into(), version: None });
        let at = |lookup: &Lookup, name: &str| -> Vec<(String, String)> {
            let found = resolve(lookup, &root.join("models/c.sql"), &named(name));
            found.iter().map(|f| (f.id.clone(), project.text(&f.path).unwrap()[f.range.clone()].to_owned())).collect()
        };
        let expected = |name: &str| vec![(format!("snapshot.shop.{name}"), name.to_owned())];
        assert_eq!(at(&Lookup::new(&project, None), "ys"), expected("ys"));
        let node = |name: &str, file: &str| {
            json!({"unique_id": format!("snapshot.shop.{name}"), "name": name, "resource_type": "snapshot",
                   "package_name": "shop", "original_file_path": file})
        };
        let nodes = json!({"snapshot.shop.ys": node("ys", "snapshots/_s.yml"), "snapshot.shop.old": node("old", "snapshots/old.sql")});
        let index = ParseIndex::of(&json!({"metadata": {"project_name": "shop"}, "nodes": nodes}));
        let parsed = Lookup::new(&project, Some(&index));
        assert_eq!(at(&parsed, "ys"), expected("ys"), "its name, not its file's start");
        assert_eq!(at(&parsed, "old"), expected("old"), "a snapshot's block at its name, as before the parse");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn before_a_parse_a_ref_finds_a_version_through_the_scan() {
        let properties = "models:\n  - name: m\n    versions:\n      - v: 1\n      - v: 2\n  - name: gone\n\
                          \x20   versions:\n      - v: 1\n      - v: 2\nunit_tests:\n  - name: t\n    model: m\n";
        let files = [
            ("dbt_project.yml", "name: r\n"),
            ("models/_v.yml", properties),
            ("models/m_v1.sql", "select 1"),
            ("models/m.sql", "select 2"),
            ("models/gone_v1.sql", "select 1"),
            ("models/x.sql", "select 1"),
            ("models/c.sql", "select * from {{ ref('m') }}, {{ ref('m', v=1) }}"),
        ];
        let root = scratch_project("navigation-versions-scanned", &files);
        let reference = |name: &str, version: Option<&str>| {
            Some(Target::Call(scan::Target::Ref {
                package: None,
                name: name.into(),
                version: version.map(str::to_owned),
            }))
        };
        let ids =
            |target| -> Vec<String> { leads(&root, "models/c.sql", target).into_iter().map(|(id, ..)| id).collect() };
        // As both builds resolve them: `m.sql` is the latest, version 2, as no file has its default name.
        assert_eq!(ids(reference("m", None)), ["model.r.m.v2"]);
        assert_eq!(ids(reference("m", Some("1"))), ["model.r.m.v1"]);
        assert_eq!(ids(reference("x", Some("1"))), Vec::<String>::new(), "no version of a model without versions");
        assert_eq!(ids(reference("gone", None)), Vec::<String>::new(), "a latest with no file, which dbt fails on");
        let named = |kind: &'static str| Some(Target::Named { kind, package: None, name: "m".into() });
        let each = ["model.r.m.v1", "model.r.m.v2"];
        let mut entry = ids(named("model"));
        entry.sort();
        assert_eq!(entry, each, "an entry names each version");
        let mut unit = ids(named("unit test model"));
        unit.sort();
        assert_eq!(unit, each, "as dbt tests each version");
        let used = used(&root, "models/c.sql", "ref('m')", 6, true);
        let files: Vec<&str> = used.iter().map(|(file, _)| file.as_str()).collect();
        assert_eq!(
            files,
            ["models/_v.yml", "models/_v.yml", "models/c.sql", "models/m.sql"],
            "its entry, its unit test, its call"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn after_a_parse_its_nodes_decide_a_name_it_knows() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/m.sql", "select 1"),
            ("models/fact.sql", "select 1"),
            ("models/fact_v2.sql", "select 2"),
            ("models/a/twin.sql", "select 1"),
            ("models/b/twin.sql", "{{ config(enabled=false) }} select 2"),
        ];
        let root = scratch_project("navigation-parse-decides", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let node = |id: &str, name: &str, file: &str, version: Value| {
            json!({"unique_id": id, "name": name, "resource_type": "model", "package_name": "shop",
                   "original_file_path": file, "version": version, "latest_version": 2})
        };
        let mut twin = node("model.shop.twin", "twin", "models/b/twin.sql", Value::Null);
        twin["latest_version"] = Value::Null;
        let mut enabled = twin.clone();
        enabled["original_file_path"] = json!("models/a/twin.sql");
        // fact's version 1 is defined in the file named after the model, as `defined_in: fact` says.
        let manifest = json!({"metadata": {"project_name": "shop"}, "nodes": {
            "model.shop.fact.v1": node("model.shop.fact.v1", "fact", "models/fact.sql", json!(1)),
            "model.shop.fact.v2": node("model.shop.fact.v2", "fact", "models/fact_v2.sql", json!(2)),
            "model.shop.twin": enabled}, "disabled": {"model.shop.twin": [twin]}});
        let index = ParseIndex::of(&manifest);
        let files_of = |lookup: &Lookup, target: Target| -> Vec<String> {
            let found = resolve(lookup, &root.join("models/m.sql"), &target);
            found.iter().map(|found| found.path.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect()
        };
        let reference =
            |name: &str| Target::Call(scan::Target::Ref { package: None, name: name.into(), version: None });
        let parsed = Lookup::new(&project, Some(&index));
        assert_eq!(files_of(&parsed, reference("fact")), ["models/fact_v2.sql"], "the latest alone");
        let v1 = Target::Call(scan::Target::Ref { package: None, name: "fact".into(), version: Some("1".into()) });
        assert_eq!(files_of(&parsed, v1), ["models/fact.sql"], "the version its file holds");
        assert_eq!(files_of(&parsed, reference("twin")), ["models/a/twin.sql"], "the one dbt keeps");
        let unit = Target::Named { kind: "unit test model", package: None, name: "fact".into() };
        assert_eq!(files_of(&parsed, unit), ["models/fact.sql", "models/fact_v2.sql"], "each version");
        // Before a parse, two files of one name are both candidates: the scan reads no
        // `config(enabled=false)`, which only the parse knows.
        let scanned = Lookup::new(&project, None);
        assert_eq!(files_of(&scanned, reference("twin")), ["models/a/twin.sql", "models/b/twin.sql"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_name_that_two_packages_but_the_caller_s_and_the_root_define_leads_to_both() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/m.sql", "select 1"),
            ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
            ("dbt_packages/pkg/models/twice.sql", "select 2"),
            ("dbt_packages/other/dbt_project.yml", "name: other\n"),
            ("dbt_packages/other/models/twice.sql", "select 3"),
        ];
        let root = scratch_project("navigation-ambiguous", &files);
        let ids = |rel: &str| -> Vec<String> {
            leads(&root, rel, called(None, "twice")).into_iter().map(|(id, ..)| id).collect()
        };
        // dbt fails there (dbt1005): every candidate, in the order of their paths.
        assert_eq!(ids("models/m.sql"), ["model.other.twice", "model.pkg.twice"]);
        assert_eq!(ids("dbt_packages/pkg/models/twice.sql"), ["model.pkg.twice"], "the caller's own first");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn after_a_parse_a_ref_finds_versions_and_python_models_and_skips_disabled_nodes() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/orders.sql", "select 1"),
            ("models/dim_v1.sql", "select 1"),
            ("models/dim_v2.sql", "select 2"),
            ("models/py_model.py", "def model(dbt, session):\n    return dbt.ref('orders')\n"),
            ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
            ("dbt_packages/pkg/models/orders.sql", "select 3"),
            ("dbt_packages/pkg/models/uses.sql", "select 4"),
        ];
        let root = scratch_project("navigation-parsed", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let node = |id: &str, package: &str, file: &str, version: Value, latest: Value| {
            let name = id.split('.').nth(2).unwrap();
            json!({"unique_id": id, "name": name, "resource_type": "model", "package_name": package,
                   "original_file_path": file, "version": version, "latest_version": latest})
        };
        let manifest = json!({
            "metadata": {"project_name": "shop"},
            "nodes": {
                "model.shop.orders": node("model.shop.orders", "shop", "models/orders.sql", Value::Null, Value::Null),
                "model.shop.dim.v1": node("model.shop.dim.v1", "shop", "models/dim_v1.sql", json!(1), json!(1)),
                "model.shop.dim.v2": node("model.shop.dim.v2", "shop", "models/dim_v2.sql", json!(2), json!(1)),
                "model.shop.py_model": node("model.shop.py_model", "shop", "models/py_model.py", Value::Null, Value::Null),
            },
            "disabled": {"model.pkg.orders": [node("model.pkg.orders", "pkg", "models/orders.sql", Value::Null, Value::Null)]},
        });
        let index = ParseIndex::of(&manifest);
        let leads = |lookup: &Lookup, rel: &str, name: &str, version: Option<&str>| -> Vec<(String, String)> {
            let target = scan::Target::Ref { package: None, name: name.into(), version: version.map(str::to_owned) };
            let relative = |found: &Found| found.path.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
            lookup
                .call(lookup.package_of(&root.join(rel)), &target)
                .iter()
                .map(|f| (f.id.clone(), relative(f)))
                .collect()
        };
        let lead = |id: &str, file: &str| vec![(id.to_owned(), file.to_owned())];
        let parsed = Lookup::new(&project, Some(&index));
        assert_eq!(leads(&parsed, "models/orders.sql", "dim", None), lead("model.shop.dim.v1", "models/dim_v1.sql"));
        assert_eq!(
            leads(&parsed, "models/orders.sql", "dim", Some("2")),
            lead("model.shop.dim.v2", "models/dim_v2.sql")
        );
        assert_eq!(leads(&parsed, "models/orders.sql", "dim", Some("3")), Vec::new());
        assert_eq!(
            leads(&parsed, "models/orders.sql", "orders", Some("1")),
            Vec::new(),
            "v= on a model without versions"
        );
        assert_eq!(
            leads(&parsed, "models/orders.sql", "py_model", None),
            lead("model.shop.py_model", "models/py_model.py")
        );
        let uses = "dbt_packages/pkg/models/uses.sql";
        assert_eq!(
            leads(&parsed, uses, "orders", None),
            lead("model.shop.orders", "models/orders.sql"),
            "pkg's disabled"
        );
        // Before a parse: no version, and the package's own model, which dbt disables.
        // An entry of a versioned model leads to each version, as dbt patches them all from it.
        let entry = Target::Named { kind: "model", package: None, name: "dim".into() };
        let ids: Vec<String> =
            resolve(&parsed, &root.join("models/orders.sql"), &entry).into_iter().map(|f| f.id).collect();
        assert_eq!(ids, ["model.shop.dim.v1", "model.shop.dim.v2"]);
        let scanned = Lookup::new(&project, None);
        assert_eq!(leads(&scanned, "models/orders.sql", "dim", None), Vec::new());
        assert_eq!(
            leads(&scanned, uses, "orders", None),
            lead("model.pkg.orders", "dbt_packages/pkg/models/orders.sql")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_macro_call_looks_in_its_file_s_package_then_the_root_only() {
        const MODEL: &str = "{{ helper() }} {{ pkg.outer() }} {{ only_pkg() }} {{ dbt.current_timestamp() }}";
        const PACKAGE: &str = "{% macro helper() %}pkg{% endmacro %}\n{% macro outer() %}{{ helper() }}{% endmacro %}\n\
                               {% macro only_pkg() %}{% endmacro %}\n";
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("macros/helper.sql", "{% macro helper() %}root{% endmacro %}\n"),
            ("models/m.sql", MODEL),
            ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
            ("dbt_packages/pkg/macros/m.sql", PACKAGE),
        ];
        let root = scratch_project("navigation-macros", &files);
        let model = |needle: &str, delta: usize| {
            leads(&root, "models/m.sql", at(&root.join("models/m.sql"), MODEL, needle, delta))
        };
        // A macro leads to its whole block, as dbt lsp answers.
        let helper = lead("macro.shop.helper", "macros/helper.sql", "{% macro helper() %}root{% endmacro %}");
        assert_eq!(model("helper", 1), helper);
        assert_eq!(model("pkg.outer", 1)[0].0, "macro.pkg.outer", "on its package");
        assert_eq!(model("outer", 1)[0].0, "macro.pkg.outer");
        assert_eq!(model("only_pkg", 1), Vec::new(), "another package's macro, behind its name");
        assert_eq!(model("current_timestamp", 1), Vec::new(), "dbt's own");
        // Inside a package's macro, the file's package first; dbt looks in the rendering node's.
        let package = "dbt_packages/pkg/macros/m.sql";
        let inner = leads(&root, package, at(&root.join(package), PACKAGE, "{{ helper", 4));
        assert_eq!(inner[0].0, "macro.pkg.helper");
        std::fs::remove_dir_all(root).unwrap();
    }

    const USES_YML: &str = "version: 2\nmodels:\n  - name: b\n    description: '{{ m() }}'\n  - name: a\n    data_tests:\n\
                            \x20     - is_even\n      - relationships:\n          to: ref('b')\n          field: id\nunit_tests:\n  - name: t\n\
                            \x20   model: b\n";

    /// A project whose model `b` and macro `m` are used in every kind of
    /// file, and whose package `pkg` has a `b` of its own.
    fn uses(name: &str) -> PathBuf {
        let files = [
            ("dbt_project.yml", "name: shop\non-run-start: \"{{ m() }}\"\n"),
            ("models/b.sql", "select 1"),
            ("models/a.sql", "select * from {{ ref('b') }} {{ m() }}"),
            ("models/py.py", "def model(dbt, session):\n    return dbt.ref('b')\n"),
            ("models/_m.yml", USES_YML),
            ("analyses/x.sql", "{{ ref('b') }}"),
            ("macros/m.sql", "{% macro m() %}1{% endmacro %}\n"),
            ("macros/tests.sql", "{% test is_even(model, column_name) %}\nselect 1\n{% endtest %}\n"),
            ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
            ("dbt_packages/pkg/models/b.sql", "select 2"),
            ("dbt_packages/pkg/models/c.sql", "{{ ref('b') }} {{ m() }}"),
            ("dbt_packages/pkg/macros/m.sql", "{% macro m() %}2{% endmacro %}\n"),
        ];
        scratch_project(name, &files)
    }

    #[test]
    fn a_function_s_calls_and_entry_are_its_references() {
        let files = [
            ("dbt_project.yml", "name: r\nmodels:\n  r:\n    +post_hook: \"select {{ function('f') }}(1)\"\n"),
            ("functions/f.sql", "x * 2"),
            ("functions/_f.yml", "functions:\n  - name: f\n"),
            ("models/a.sql", "select {{ function('f') }}(1), {{ function('r', 'f') }}(2)"),
            (
                "models/_a.yml",
                "models:\n  - name: a\n    config:\n      post_hook: \"select {{ function('f') }}(3)\"\n",
            ),
        ];
        let root = scratch_project("navigation-function-references", &files);
        let sql = Path::new("/p/models/a.sql");
        let text = files[3].1;
        let function = |package: Option<&str>| {
            Some(Target::Call(scan::Target::Function { package: package.map(str::to_owned), name: "f".into() }))
        };
        assert_eq!(at(sql, text, "function('f')", 10), function(None), "at its name");
        assert_eq!(at(sql, text, "'r', 'f'", 1), function(Some("r")), "at its package");
        let keyword = Some(Target::Call(scan::Target::Macro { package: None, name: "function".into() }));
        assert_eq!(at(sql, text, "function('f')", 2), keyword, "its keyword a macro call, which a project may define");
        assert_eq!(at(sql, text, "function('f')", 2).unwrap().kind(), "macro");
        assert_eq!(function(None).unwrap().kind(), "function");
        let uses = [("dbt_project.yml", "f"), ("models/_a.yml", "f"), ("models/a.sql", "f"), ("models/a.sql", "f")];
        let uses = uses.map(|(file, text)| (file.to_owned(), text.to_owned()));
        assert_eq!(used(&root, "models/a.sql", "function('f')", 10, false), uses);
        let mut declared = uses.to_vec();
        declared.insert(1, ("functions/_f.yml".into(), "f".into()));
        declared.insert(2, ("functions/f.sql".into(), String::new()));
        assert_eq!(used(&root, "models/_a.yml", "function('f')", 10, true), declared, "its entry and its file too");
        // The entry's name leads to the function's file.
        assert_eq!(
            leads(&root, "functions/_f.yml", at(Path::new("/p/functions/_f.yml"), files[2].1, "f\n", 0)),
            lead("function.r.f", "functions/f.sql", "")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The references of the target at NEEDLE, DELTA bytes in, in the file
    /// REL of the project at ROOT: each file relative to ROOT with the text
    /// of its range.  Without DECLARATION, a definition at each use leads
    /// back to the same id.
    fn used(root: &Path, rel: &str, needle: &str, delta: usize, declaration: bool) -> Vec<(String, String)> {
        let project = project::scan(root, &HashMap::new()).unwrap();
        let lookup = Lookup::new(&project, None);
        let path = root.join(rel);
        let target =
            target_at(&path, project.text(&path).unwrap(), project.text(&path).unwrap().find(needle).unwrap() + delta);
        let found = references(&lookup, &path, target.as_ref().unwrap(), declaration);
        // A declaration is no use: only the calls lead back.
        for use_ in found.iter().filter(|_| !declaration) {
            let text = project.text(&use_.path).unwrap();
            let back =
                target_at(&use_.path, text, use_.range.start).map(|target| resolve(&lookup, &use_.path, &target));
            assert!(back.unwrap_or_default().iter().any(|found| found.id == use_.id), "{use_:?} leads elsewhere");
        }
        let relative = |found: &Found| found.path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        found
            .iter()
            .map(|found| (relative(found), project.text(&found.path).unwrap()[found.range.clone()].to_owned()))
            .collect()
    }

    #[test]
    fn a_model_s_references_are_its_refs_across_files_and_its_unit_tests() {
        let root = uses("navigation-model-uses");
        let pairs = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(path, text)| (path.to_string(), text.to_string())).collect()
        };
        let calls = [
            ("analyses/x.sql", "b"),
            ("models/_m.yml", "b"),
            ("models/_m.yml", "b"),
            ("models/a.sql", "b"),
            ("models/py.py", "b"),
        ];
        // The package's own `b` takes its `ref('b')`: no reference of the root's.
        assert_eq!(used(&root, "models/a.sql", "ref('b')", 5, false), pairs(&calls));
        let mut declared = calls.to_vec();
        declared.extend([("models/_m.yml", "b"), ("models/b.sql", "")]);
        declared.sort();
        let mut with_declaration = used(&root, "models/a.sql", "ref('b')", 5, true);
        with_declaration.sort();
        assert_eq!(with_declaration, pairs(&declared));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_generic_test_s_references_are_its_uses_in_properties_files() {
        let root = uses("navigation-test-uses");
        assert_eq!(used(&root, "macros/tests.sql", "is_even", 0, false), [("models/_m.yml".into(), "is_even".into())]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_macro_s_references_are_its_calls_in_sql_yaml_values_and_hooks() {
        let root = uses("navigation-macro-uses");
        let calls = ["analyses", "dbt_project.yml", "models/_m.yml", "models/a.sql"];
        let found = used(&root, "models/a.sql", "m()", 0, false);
        assert_eq!(found.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(), calls[1..]);
        assert!(found.iter().all(|(_, text)| text == "m"));
        // From its own tag, with its block.
        let declared = used(&root, "macros/m.sql", "m()", 0, true);
        assert_eq!(declared.len(), 4);
        assert!(declared.contains(&("macros/m.sql".into(), "{% macro m() %}1{% endmacro %}".into())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_the_flag_as_a_yaml_false_puts_the_root_first() {
        let pkg_yml = "dbt_packages/pkg/models/_pkg.yml";
        let table = Some(Target::Source(sources::Target::Table { source: "raw".into(), table: "orders".into() }));
        let events = Some(Target::Source(sources::Target::Table { source: "ext".into(), table: "events".into() }));
        // Each value of the flag, if any, and the package searched first, as
        // dbt 2.0.9 and dbt-oss 2.0.11 read it: a YAML 1.2 boolean.
        let values = [
            (Some("false"), "shop"),
            (Some("False"), "shop"),
            (Some("FALSE"), "shop"),
            (Some("fAlse"), "pkg"),
            (Some("'false'"), "pkg"),
            (Some("\"false\""), "pkg"),
            (Some("no"), "pkg"),
            (Some("true"), "pkg"),
            (None, "pkg"),
        ];
        for (i, (value, first)) in values.into_iter().enumerate() {
            let flag = value.map(|value| format!("flags:\n  require_ref_searches_node_package_before_root: {value}\n"));
            let root = shop(&format!("navigation-flag-{i}"), &format!("name: shop\n{}", flag.unwrap_or_default()));
            let found = leads(&root, pkg_yml, called(None, "orders"));
            assert_eq!(found[0].0, format!("model.{first}.orders"), "ref with the flag {value:?}");
            let found = leads(&root, pkg_yml, table.clone());
            assert_eq!(found[0].0, format!("source.{first}.raw.orders"), "source with the flag {value:?}");
            // The file's package comes before any other, the root aside; a doc
            // ignores the flag.
            let ids: Vec<String> = leads(&root, pkg_yml, events.clone()).into_iter().map(|(id, ..)| id).collect();
            assert_eq!(ids, ["source.pkg.ext.events"], "source in two packages with the flag {value:?}");
            let found = leads(&root, pkg_yml, named("doc", None, "order_status"));
            assert_eq!(found[0].0, "doc.shop.order_status", "doc with the flag {value:?}");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_source_leads_to_its_one_entry_and_any_package_to_every_declaration() {
        let root = shop("navigation-sources", "name: shop\n");
        let source = Some(Target::Source(sources::Target::Source { source: "raw".into() }));
        // One entry for the source, however many tables it has.
        assert_eq!(leads(&root, "models/_models.yml", source), lead("source.shop.raw", "models/_sources.yml", "raw"));
        let events = Some(Target::Source(sources::Target::Table { source: "ext".into(), table: "events".into() }));
        let found: Vec<String> = leads(&root, "models/_models.yml", events).into_iter().map(|(id, ..)| id).collect();
        assert_eq!(found, ["source.other.ext.events", "source.pkg.ext.events"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_doc_looks_in_the_root_first() {
        let root = shop("navigation-docs", "name: shop\n");
        let pkg_yml = "dbt_packages/pkg/models/_pkg.yml";
        let found = leads(&root, pkg_yml, named("doc", None, "order_status"));
        assert_eq!(found, lead("doc.shop.order_status", "models/docs.md", "order_status"));
        // From the root or a package, that of the package whose name sorts first.
        assert_eq!(leads(&root, "models/_models.yml", named("doc", None, "pkg_only"))[0].0, "doc.other.pkg_only");
        assert_eq!(leads(&root, pkg_yml, named("doc", None, "pkg_only"))[0].0, "doc.other.pkg_only");
        assert_eq!(
            leads(&root, "models/_models.yml", named("doc", Some("pkg"), "order_status"))[0].0,
            "doc.pkg.order_status"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_doc_ignores_the_calling_file_s_package() {
        let calls = "version: 2\nmodels:\n  - name: m\n    description: \"{{ doc('shared') }} {{ doc('both') }}\"\n";
        let shared = "{% docs shared %}\n{% enddocs %}\n";
        let both = "{% docs both %}\n{% enddocs %}\n";
        let root = scratch_project(
            "navigation-doc-order",
            &[
                ("dbt_project.yml", "name: shop\n"),
                ("models/_models.yml", calls),
                ("models/docs.md", both),
                ("dbt_packages/aaa/dbt_project.yml", "name: aaa\n"),
                ("dbt_packages/aaa/models/_models.yml", calls),
                ("dbt_packages/aaa/models/docs.md", shared),
                ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
                ("dbt_packages/pkg/models/_models.yml", calls),
                ("dbt_packages/pkg/models/docs.md", shared),
                ("dbt_packages/shop_utils/dbt_project.yml", "name: shop_utils\n"),
                ("dbt_packages/shop_utils/models/_models.yml", calls),
                ("dbt_packages/shop_utils/models/docs.md", &format!("{shared}{both}")),
            ],
        );
        // As dbt 2.0.9 gives it from a model of each: the root's, else that of
        // the package whose name sorts first.
        for package in ["", "dbt_packages/aaa/", "dbt_packages/pkg/", "dbt_packages/shop_utils/"] {
            let yml = format!("{package}models/_models.yml");
            assert_eq!(leads(&root, &yml, named("doc", None, "shared"))[0].0, "doc.aaa.shared", "from {yml}");
        }
        let yml = "dbt_packages/shop_utils/models/_models.yml";
        assert_eq!(leads(&root, yml, named("doc", None, "both")), lead("doc.shop.both", "models/docs.md", "both"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_generic_test_is_its_test_block_in_its_file_s_package_or_the_root() {
        let root = shop("navigation-tests", "name: shop\n");
        let yml = "models/_models.yml";
        // The test block, not the macro of the same name beside it.
        assert_eq!(
            leads(&root, yml, named("test", None, "is_even")),
            lead("macro.shop.test_is_even", "macros/tests.sql", "is_even")
        );
        // A root test overrides dbt's own; dbt's own are not in the project.
        assert_eq!(leads(&root, yml, named("test", None, "unique"))[0].0, "macro.shop.test_unique");
        assert_eq!(leads(&root, yml, named("test", None, "not_null")), Vec::new());
        // Never another installed package's without its name.
        assert_eq!(leads(&root, yml, named("test", None, "pkg_test")), Vec::new());
        assert_eq!(leads(&root, yml, named("test", Some("pkg"), "pkg_test"))[0].0, "macro.pkg.test_pkg_test");
        // A package's file finds its own package's test first.
        let pkg_yml = "dbt_packages/pkg/models/_pkg.yml";
        assert_eq!(leads(&root, pkg_yml, named("test", None, "is_even"))[0].0, "macro.pkg.test_is_even");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hover_shows_a_doc_s_text_and_a_test_s_signature() {
        let root = shop("navigation-hover", "name: shop\n");
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let path = root.join("models/_models.yml");
        let shown = |target: Option<Target>, markdown| {
            let target = target.unwrap();
            let found = resolve(&Lookup::new(&project, None), &path, &target);
            hover(&project, &target, &found[0], markdown)
        };
        assert_eq!(shown(named("doc", None, "order_status"), true).as_deref(), Some("The root's."));
        // An empty docs block shows nothing, so that the server's answer passes.
        assert_eq!(shown(named("doc", None, "empty"), true), None);
        let signature = "```jinja\ntest is_even(model, column_name)\n```\n\nin package `shop`";
        assert_eq!(shown(named("test", None, "is_even"), true).as_deref(), Some(signature));
        let plain = "test is_even(model, column_name)\n\nin package shop";
        assert_eq!(shown(named("test", None, "is_even"), false).as_deref(), Some(plain));
        assert_eq!(shown(called(None, "orders"), true), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_entry_names_a_resource_of_its_own_package() {
        let root = shop("navigation-entries", "name: shop\n");
        let (yml, pkg_yml) = ("models/_models.yml", "dbt_packages/pkg/models/_pkg.yml");
        // dbt 2.0.9 lets no entry describe another package's resource.
        let expected = [
            (yml, "model", "orders", Some("model.shop.orders")),
            (pkg_yml, "model", "orders", Some("model.pkg.orders")),
            (yml, "model", "only_pkg", None),
            (yml, "seed", "raw", Some("seed.shop.raw")),
            (yml, "snapshot", "snap", Some("snapshot.shop.snap")),
            (yml, "macro", "test_is_even", Some("macro.shop.test_is_even")),
            (yml, "macro", "pkg_macro", None),
            (yml, "analysis", "a", Some("analysis.shop.a")),
            (pkg_yml, "analysis", "a", Some("analysis.pkg.a")),
            (yml, "unit test model", "orders", Some("model.shop.orders")),
            (yml, "unit test model", "only_pkg", None),
        ];
        for (file, kind, name, id) in expected {
            let found = leads(&root, file, named(kind, None, name));
            assert_eq!(found.first().map(|(found, ..)| found.as_str()), id, "{kind} {name} from {file}");
        }
        assert_eq!(leads(&root, yml, named("seed", None, "raw")), lead("seed.shop.raw", "seeds/raw.csv", ""));
        std::fs::remove_dir_all(root).unwrap();
    }
}
