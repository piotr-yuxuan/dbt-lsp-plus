// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The Name at a position, from the document's scan, and where it leads:
//! dbt's one lookup, as dbt 2.0.9 does it, for the definitions and
//! references that dbt lsp 2.0.9 does not give, and the node of the last
//! parse that a Name describes, for hover, nodeInfo and inlay hints.  Pure.

use crate::config::ParseIndex;
use crate::project::{self, Entity, Project, ResourceType, Scanned, TYPES};
use crate::reference::{Kind, Name};
use crate::scan;
use crate::sources;
use crate::yaml::{self, Tree};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Where a Name leads, the model's Target of a Node or a source group:
/// dbt's unique id of what it names, or `source.<package>.<source>` for a
/// whole source, which is no node of dbt's, and the range of that name in
/// its file.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
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
/// the name in their ids, and the packages searched in turn.  The `name:`
/// of an entry is an entry of its label.  A `doc` ignores the calling
/// file's package: past the root, the first package by path, which is the
/// byte order of their names, since dbt installs each at
/// `dbt_packages/<name>`.  An entry describes a resource of its own package
/// only: dbt 2.0.9 calls one naming another package's resource unused,
/// where dbt-core 1.12 patches it.
const LOOKUP: [(Kind, &[&str], &str, &[Step]); 9] = [
    (Kind::Doc, &["doc"], "", &[Step::Root, Step::Any]),
    (Kind::Test, &["macro"], "test_", &[Step::Own, Step::Root]),
    (Kind::UnitTestModel, &["model"], "", &[Step::Own]),
    (Kind::Entry { label: "model" }, &["model"], "", &[Step::Own]),
    (Kind::Entry { label: "seed" }, &["seed"], "", &[Step::Own]),
    (Kind::Entry { label: "snapshot" }, &["snapshot"], "", &[Step::Own]),
    (Kind::Entry { label: "analysis" }, &["analysis"], "", &[Step::Own]),
    (Kind::Entry { label: "macro" }, &["macro"], "", &[Step::Own]),
    (Kind::Entry { label: "function" }, &["function"], "", &[Step::Own]),
];

/// What the position OFFSET of the file at PATH names, from SCANNED, its
/// scan: a `source` anywhere; elsewhere than in YAML, a `ref`'s or a
/// function's argument, a macro call's name or package, or the name in the
/// tag of a macro or a generic test, which looks itself up; in YAML, a
/// `ref`, a `doc`, the `name:` of a navigable entry, a generic test's name,
/// a unit test's `model:`, or a macro or function call in the Jinja of a
/// value, in that order.
pub fn reference_at(path: &Path, scanned: &Scanned, offset: usize) -> Option<Name> {
    let inside = |range: &Range<usize>| range.start <= offset && offset <= range.end;
    // A call names what it calls at an argument of a `ref` or a function, at its name for a macro.
    let called = |call: &&scan::Call| match call.name.kind {
        Kind::Ref | Kind::Function => call.arguments.iter().any(inside),
        Kind::Macro => inside(&call.range),
        _ => false,
    };
    let source = sources::name_at(scanned, offset);
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
        let blocks = scanned.blocks.iter().filter(|block| inside(&block.name_range));
        if let Some(name) = blocks.filter_map(macro_named).next() {
            return Some(Name::new(Kind::Macro, None, &name));
        }
        return scanned.calls.iter().find(called).map(|call| call.name.clone());
    }
    let reference = scanned.calls.iter().find(|call| call.name.kind == Kind::Ref && called(call));
    let reference = reference.map(|call| call.name.clone());
    let doc = || scanned.docs.iter().find(|call| call.arguments.iter().any(inside)).map(|call| call.name.clone());
    let entry = || {
        // An entry of its label, when that is one of `LOOKUP`'s.
        let navigable = |label: &'static str| LOOKUP.iter().any(|(kind, ..)| *kind == Kind::Entry { label });
        let found =
            scanned.entries.iter().filter(|entry| navigable(entry.label)).find(|entry| inside(&entry.name_range))?;
        Some(Name::new(Kind::Entry { label: found.label }, None, &found.name))
    };
    let test = || {
        let found = scanned.tests.iter().find(|test| inside(&test.range))?;
        Some(Name::new(Kind::Test, found.package.as_deref(), &found.name))
    };
    let model = || {
        let (name, _) = scanned.models.iter().find(|(_, range)| inside(range))?;
        Some(Name::new(Kind::UnitTestModel, None, name))
    };
    let macro_call = || {
        let found = scanned.calls.iter().find(|call| call.name.kind != Kind::Ref && called(call));
        found.map(|call| call.name.clone())
    };
    reference.or_else(doc).or_else(entry).or_else(test).or_else(model).or_else(macro_call)
}

/// dbt 2.0.9's lookup over a project, made once for a request or a graph:
/// the lookup flag read once, the resources by id, and the last parse's
/// index, if any.
pub struct Lookup<'a, 'i> {
    project: &'a Project,
    /// What the last parse knows that the scan does not: versions, Python
    /// models, snapshots defined in YAML, and which nodes are disabled.
    index: Option<&'i ParseIndex>,
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

impl<'a, 'i> Lookup<'a, 'i> {
    pub fn new(project: &'a Project, index: Option<&'i ParseIndex>) -> Lookup<'a, 'i> {
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

    /// Where a call of NAME from the package OWN leads, by dbt 2.0.9's
    /// lookup: in the package the call names; else a `ref`, a `source`, a
    /// function or a dispatch in OWN and the root, in the flag's order, then in any
    /// package; an unqualified macro in OWN, then the root only, since dbt
    /// keeps another package's macros behind its name.  Every resource of
    /// the first of these that has some, but those the last parse disables,
    /// in the order of their paths: one, but where dbt fails on several
    /// (dbt1005), and a dispatch's every implementation (`*__name`).  A
    /// `ref` leads to its file's start, as dbt lsp answers, a macro to its
    /// whole block, as dbt lsp answers too.
    pub fn call(&self, own: &str, name: &Name) -> Vec<Target> {
        let root = self.project.root_package.as_str();
        let (first, second) = if self.root_first { (root, own) } else { (own, root) };
        let steps = match (&name.kind, &name.package) {
            (Kind::Ref | Kind::Macro | Kind::Dispatch | Kind::Function, Some(package)) => vec![Some(package.as_str())],
            (Kind::Macro, None) => vec![Some(own), Some(root)],
            _ => vec![Some(first), Some(second), None],
        };
        let get = |id: String| self.by_id.get(id.as_str()).map_or(&[][..], Vec::as_slice);
        let found = |entity: &Entity| Target {
            id: entity.id.clone(),
            path: entity.path.clone(),
            range: if entity.resource_type == "macro" { entity.range.clone() } else { entity.name_range.clone() },
        };
        // The models, seeds and snapshots that the last parse knows, at their file's start: the one of
        // VERSION, if given, which a model without versions has not; else the latest version, or the one
        // model without versions.
        let parsed = |package: &str, name: &str, version: Option<&str>| -> Vec<Target> {
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
        let in_package = |package: &str| -> Vec<Target> {
            let Name { kind, name: called, version, .. } = name;
            match kind {
                Kind::Ref => {
                    // Once a parse knows the name in PACKAGE, its nodes decide: they know versions, Python
                    // models, snapshots defined in YAML, and which of two files of one name dbt keeps.
                    if self.index.is_some_and(|index| !index.resources(package, called).is_empty()) {
                        return parsed(package, called, version.as_deref());
                    }
                    // Else the scan's: a version by its name and version, the latest when none is named, which may
                    // have no file; a model, seed or snapshot when the name has no versions and the call names none.
                    let prefix = project::version_id(package, called, "");
                    let ids = self.by_id.range(prefix.as_str()..).take_while(|(id, _)| id.starts_with(&prefix));
                    let versions: Vec<&Entity> = ids
                        .flat_map(|(_, entities)| entities.iter().copied())
                        .filter(|entity| entity.version.is_some() && entity.name == *called)
                        .collect();
                    let chosen = |entity: &&Entity| match version {
                        Some(version) => entity.version.as_ref() == Some(version),
                        None => entity.latest,
                    };
                    let plain = versions.is_empty() && version.is_none();
                    let rows = TYPES.iter().filter(|row| row.refable && plain);
                    let unversioned = rows.flat_map(|row| get(project::node_id(row, package, called)).iter().copied());
                    versions.into_iter().filter(chosen).chain(unversioned).map(found).collect()
                }
                Kind::Table { source } => {
                    get(project::table_id(package, source, called)).iter().map(|entity| found(entity)).collect()
                }
                Kind::Macro => {
                    get(project::node_id(&project::MACRO, package, called)).iter().map(|entity| found(entity)).collect()
                }
                Kind::Function => get(project::node_id(&project::FUNCTION, package, called))
                    .iter()
                    .map(|entity| found(entity))
                    .collect(),
                Kind::Dispatch => {
                    let (prefix, suffix) = (project::node_id(&project::MACRO, package, ""), format!("__{called}"));
                    let in_package = self.by_id.range(prefix.as_str()..).take_while(|(id, _)| id.starts_with(&prefix));
                    let implementations = in_package.filter(|(id, _)| id.ends_with(&suffix));
                    implementations.flat_map(|(_, entities)| entities.iter().map(|entity| found(entity))).collect()
                }
                _ => Vec::new(),
            }
        };
        let disabled = |found: &Target| self.index.is_some_and(|index| index.disabled(&found.id));
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
    fn parsed_at(&self, node: &Value) -> Option<Target> {
        let (id, file) = (node["unique_id"].as_str()?, node["original_file_path"].as_str()?);
        let path = self.project.packages.get(node["package_name"].as_str()?)?.join(file);
        let scanned = self.by_id.get(id).into_iter().flatten().find(|entity| entity.path == path);
        let range = scanned.map_or(0..0, |entity| entity.name_range.clone());
        Some(Target { id: id.to_owned(), path, range })
    }

    /// The node of the last parse that NAME, named in the file at PATH,
    /// describes: the index's node of the first place `resolve` leads it
    /// to, where its definition goes.  None without an index, where NAME
    /// leads nowhere, or to what the index lacks.
    pub fn node(&self, path: &Path, name: &Name) -> Option<&'i Value> {
        let index = self.index?;
        let target = resolve(self, path, name).into_iter().next()?;
        index.node(&target.id)
    }
}

/// What NAME, named in the file at PATH, leads to by LOOKUP, dbt 2.0.9's:
/// a source looked up as a `ref`, every declaration of the first package
/// that has some; a kind of `LOOKUP` in the package it gives, else as
/// `LOOKUP` says, the first that exists, and for an entry of a model, seed
/// or snapshot what the last parse knows that the scan does not, each
/// version of a versioned model, as dbt patches them all from the entry;
/// any other, a call, as `Lookup::call` resolves it.
pub fn resolve(lookup: &Lookup, path: &Path, name: &Name) -> Vec<Target> {
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
    if let Some((source, table)) = name.source_table() {
        let declared = project.sources.iter().filter(|declared| sources::declares(declared, name));
        let found = declared.map(|declared| {
            let (id, range) = match table {
                None => (project::group_id(&declared.package, source), declared.source_range.clone()),
                Some(table) => (project::table_id(&declared.package, source, table), declared.table_range.clone()),
            };
            (declared.package.as_str(), Target { id, path: declared.path.clone(), range })
        });
        return first_step(found.collect(), &packages(CALLS), true);
    }
    let Some((_, types, prefix, steps)) = LOOKUP.iter().find(|(row, ..)| *row == name.kind) else {
        return lookup.call(own, name);
    };
    let steps = name.package.as_deref().map_or_else(|| packages(steps), |package| vec![Some(package)]);
    // A versioned model's entry, or a unit test of it, names each version.
    let matching: Vec<&Entity> = project
        .entities
        .iter()
        .filter(|entity| {
            types.contains(&entity.resource_type)
                && (entity.id
                    == project::node_id(
                        ResourceType::named(entity.resource_type),
                        &entity.package,
                        &format!("{prefix}{}", name.name),
                    )
                    || entity.version.is_some() && entity.name == name.name)
        })
        .collect();
    let every = matching.iter().any(|entity| entity.version.is_some());
    let found = matching.into_iter().map(|entity| {
        let found = Target { id: entity.id.clone(), path: entity.path.clone(), range: entity.name_range.clone() };
        (entity.package.as_str(), found)
    });
    // Once a parse knows the resource of an entry or of a unit test's model, its nodes decide, each version
    // of a versioned model, as dbt patches or tests them all.
    let resource_type = match name.kind {
        Kind::UnitTestModel => Some("model"),
        Kind::Entry { label } => Some(label),
        _ => None,
    };
    let resource = resource_type.is_some_and(|kind| ResourceType::named(kind).refable) && name.package.is_none();
    let nodes = lookup.index.filter(|_| resource).map_or_else(Vec::new, |index| index.resources(own, &name.name));
    let typed = nodes.into_iter().filter(|node| node["resource_type"].as_str() == resource_type);
    let mut parsed: Vec<Target> = typed.filter_map(|node| lookup.parsed_at(node)).collect();
    if parsed.is_empty() {
        return first_step(found.collect(), &steps, every);
    }
    parsed.sort_by(|one, other| (&one.path, one.range.start).cmp(&(&other.path, other.range.start)));
    parsed
}

/// Where what NAME, named in the file at PATH, is used, by LOOKUP: each
/// call that resolves to it, as its definition would, at its name, a unit
/// test's `model:` naming it, and a generic test's name in a properties file
/// for the macro `test_<name>`; with DECLARATION, where it is declared
/// too: its file's start or its block, and the `name:` of each properties
/// entry declaring it in its own package.  Each leads to its id, in the
/// order of their paths.
pub fn references<'a>(lookup: &Lookup<'a, '_>, path: &Path, name: &Name, declaration: bool) -> Vec<Target> {
    let declared = resolve(lookup, path, name);
    let ids: HashSet<&str> = declared.iter().map(|found| found.id.as_str()).collect();
    let wanted = match name.kind {
        Kind::Table { .. } | Kind::Source | Kind::Dispatch => return Vec::new(),
        _ => &name.name,
    };
    // What a use at RANGE of FILE leads to, if it is NAME's.
    let leading = |file: &Path, range: Range<usize>, led: Vec<Target>| {
        let id = led.into_iter().find(|found| ids.contains(found.id.as_str()))?.id;
        Some(Target { id, path: file.to_owned(), range })
    };
    // Calls alike from one package lead alike: each is looked up once.
    let resolved: RefCell<HashMap<(&str, &Name), Option<String>>> = RefCell::default();
    let lead = |own: &'a str, called: &'a Name| {
        let found =
            || lookup.call(own, called).into_iter().find(|found| ids.contains(found.id.as_str())).map(|found| found.id);
        resolved.borrow_mut().entry((own, called)).or_insert_with(found).clone()
    };
    let calls = |file: &Path, scanned: &'a project::Scanned| -> Vec<Target> {
        let called = |call: &&scan::Call| match &call.name {
            Name { kind: Kind::Ref | Kind::Macro | Kind::Function, name: called, .. } => called == wanted,
            _ => false,
        };
        let at = |call: &scan::Call| match &call.name.kind {
            Kind::Ref | Kind::Function => call.arguments.last().cloned().unwrap_or(call.range.clone()),
            _ => call.range.end - wanted.len()..call.range.end,
        };
        let own = lookup.package_of(file);
        scanned
            .calls
            .iter()
            .filter(called)
            .filter_map(|call| Some(Target { id: lead(own, &call.name)?, path: file.to_owned(), range: at(call) }))
            .collect()
    };
    // A unit test's `model:`, a generic test, the macro `test_<name>`, and with DECLARATION an entry's
    // `name:`, those of the name only, looked up as their definitions.
    let test_name = wanted.strip_prefix("test_");
    let named = |file: &Path, scanned: &project::Scanned| -> Vec<Target> {
        let models = scanned.models.iter().filter(|(model, _)| model == wanted);
        let models = models.map(|(model, range)| (Kind::UnitTestModel, None, model, range));
        let tests = scanned.tests.iter().filter(|test| test_name == Some(test.name.as_str()));
        let tests = tests.map(|test| (Kind::Test, test.package.as_deref(), &test.name, &test.range));
        let entries = scanned.entries.iter().filter(|entry| declaration && entry.name == *wanted);
        let entries = entries.map(|entry| (Kind::Entry { label: entry.label }, None, &entry.name, &entry.name_range));
        let found = |(kind, package, named, range): (Kind, Option<&str>, &String, &Range<usize>)| {
            leading(file, range.clone(), resolve(lookup, file, &Name::new(kind, package, named)))
        };
        models.chain(tests).chain(entries).filter_map(found).collect()
    };
    let files = lookup.project.files.iter().map(|(file, scanned)| (file, &**scanned));
    let mut found: Vec<Target> =
        files.flat_map(|(file, scanned)| [calls(file, scanned), named(file, scanned)].concat()).collect();
    found.extend(declared.into_iter().filter(|_| declaration));
    found.sort_by(|one, other| (&one.path, one.range.start).cmp(&(&other.path, other.range.start)));
    found.dedup_by(|one, other| one.path == other.path && one.range == other.range);
    found
}

/// What a hover on a `doc` or a generic test, NAME, in the file at PATH,
/// shows of the first block it leads to by LOOKUP: a docs block's text
/// between its tags, as written, if any; a test's opening tag and its
/// package, in a code block and code spans when MARKDOWN.  The caller
/// checks NAME's kind: another's target may be a block too, a snapshot's.
pub fn hover(lookup: &Lookup, path: &Path, name: &Name, markdown: bool) -> Option<String> {
    let found = resolve(lookup, path, name).into_iter().next()?;
    let text = lookup.project.text(&found.path)?;
    let block = scan::blocks(text).into_iter().find(|block| block.name_range == found.range)?;
    if name.kind == Kind::Doc {
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
fn first_step(mut candidates: Vec<(&str, Target)>, steps: &[Option<&str>], all: bool) -> Vec<Target> {
    candidates.sort_by(|(_, a), (_, b)| (&a.path, a.range.start).cmp(&(&b.path, b.range.start)));
    candidates.dedup_by(|(_, a), (_, b)| a.path == b.path && a.range == b.range);
    let holds = |step: &Option<&str>, package: &str| step.is_none_or(|step| step == package);
    let step = steps.iter().find(|step| candidates.iter().any(|(package, _)| holds(step, package)));
    let found = candidates.into_iter().filter(|(package, _)| step.is_some_and(|step| holds(step, package)));
    let mut found: Vec<Target> = found.map(|(_, found)| found).collect();
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
    fn at(path: &Path, text: &str, needle: &str, delta: usize) -> Option<Name> {
        reference_at(path, &Scanned::of(path, text.to_owned()), text.find(needle).unwrap() + delta)
    }

    fn called(package: Option<&str>, name: &str) -> Option<Name> {
        let (package, name) = (package.map(str::to_owned), name.to_owned());
        Some(Name { kind: Kind::Ref, package, name, version: None })
    }

    fn named(kind: Kind, package: Option<&str>, name: &str) -> Option<Name> {
        Some(Name::new(kind, package, name))
    }

    const YML: &str = "/p/models/_models.yml";

    #[test]
    fn finds_the_calls_of_yaml_from_the_document_alone() {
        let yml = Path::new(YML);
        assert_eq!(at(yml, MODELS, "ref('customers')", 6), called(None, "customers"));
        assert_eq!(at(yml, MODELS, "ref('pkg', 'orders')", 14), called(Some("pkg"), "orders"));
        // A doc's quotes as YAML escapes them in single and double quotes.
        assert_eq!(at(yml, MODELS, "order_status''", 2), named(Kind::Doc, None, "order_status"));
        assert_eq!(at(yml, MODELS, "pkg_only", 2), named(Kind::Doc, None, "pkg_only"));
        assert_eq!(at(yml, MODELS, "'pkg', 'order_status'", 9), named(Kind::Doc, Some("pkg"), "order_status"));
        let table = Name::new(Kind::Table { source: "raw".into() }, None, "orders");
        assert_eq!(at(yml, MODELS, "'raw', 'orders'", 9), Some(table));
        // A ref naming a version, which the last parse resolves.
        let versioned = Name { kind: Kind::Ref, package: None, name: "orders".into(), version: Some("2".into()) };
        assert_eq!(at(yml, MODELS, "ref('orders', v=2)", 6), Some(versioned));
    }

    #[test]
    fn a_macro_call_in_a_yaml_comment_is_no_target() {
        let path = Path::new("/p/dbt_project.yml");
        let hooks = "name: shop\non-run-start: \"{{ log_start() }}\"\n# on-run-end: \"{{ log_end() }}\"\n";
        let macro_named = |name: &str| Some(Name::new(Kind::Macro, None, name));
        assert_eq!(at(path, hooks, "log_start", 1), macro_named("log_start"), "a hook's macro");
        assert_eq!(at(path, hooks, "log_end", 1), None, "dbt renders no comment");
    }

    #[test]
    fn finds_the_names_of_yaml_from_the_document_alone() {
        let yml = Path::new(YML);
        assert_eq!(at(yml, MODELS, "name: orders", 8), named(Kind::Entry { label: "model" }, None, "orders"));
        assert_eq!(
            at(yml, MODELS, "name: test_is_even", 8),
            named(Kind::Entry { label: "macro" }, None, "test_is_even")
        );
        assert_eq!(at(yml, MODELS, "name: a\n", 6), named(Kind::Entry { label: "analysis" }, None, "a"));
        assert_eq!(at(yml, MODELS, "name: raw\n", 7), named(Kind::Entry { label: "seed" }, None, "raw"));
        assert_eq!(at(yml, MODELS, "name: snap", 7), named(Kind::Entry { label: "snapshot" }, None, "snap"));
    }

    #[test]
    fn finds_the_tests_and_unit_test_models_of_yaml_from_the_document_alone() {
        let yml = Path::new(YML);
        assert_eq!(at(yml, MODELS, "- is_even", 4), named(Kind::Test, None, "is_even"));
        assert_eq!(at(yml, MODELS, "pkg.pkg_test", 6), named(Kind::Test, Some("pkg"), "pkg_test"));
        assert_eq!(at(yml, MODELS, "model: orders", 9), named(Kind::UnitTestModel, None, "orders"));
        // A column's or an exposure's name leads nowhere of its own.
        assert_eq!(at(yml, MODELS, "customer_id", 2), None);
        assert_eq!(at(yml, MODELS, "weekly", 2), None);
    }

    #[test]
    fn finds_a_source_a_ref_and_a_macro_call_in_sql() {
        let sql = Path::new("/p/models/customers.sql");
        let text = "select * from {{ source('raw', 'orders') }} join {{ ref('orders') }} -- {{ doc('order_status') }}";
        let table = Name::new(Kind::Table { source: "raw".into() }, None, "orders");
        assert_eq!(at(sql, text, "'orders') }} join", 2), Some(table));
        let orders = Name { kind: Kind::Ref, package: None, name: "orders".into(), version: None };
        assert_eq!(at(sql, text, "ref('orders')", 6), Some(orders));
        // The keyword: a macro call, as a project may override `ref`.
        let keyword = Name::new(Kind::Macro, None, "ref");
        assert_eq!(at(sql, text, "ref('orders')", 1), Some(keyword));
        // dbt has no doc in SQL: a call of a macro, which none defines.
        let doc = Name::new(Kind::Macro, None, "doc");
        assert_eq!(at(sql, text, "doc(", 1), Some(doc));
        assert_eq!(at(sql, text, "order_status", 2), None);
    }

    #[test]
    fn in_yaml_that_does_not_parse_a_doc_and_a_ref_are_still_named() {
        let yml = Path::new(YML);
        let text = "models:\n  - name: m\n    description: \"{{ doc('d') }}\"\n    data_tests:\n      - relationships:\n\
                    \x20         to: ref('x')\n[";
        assert!(yaml::parse(text).is_none(), "an unclosed flow sequence");
        let scanned = Scanned::of(yml, text.to_owned());
        assert_eq!(reference_at(yml, &scanned, text.find("d')").unwrap()), named(Kind::Doc, None, "d"));
        assert_eq!(reference_at(yml, &scanned, text.find("x')").unwrap()), called(None, "x"));
    }

    /// Where TARGET, named in the file REL of the project at ROOT, leads:
    /// each id with its file relative to ROOT and the text of its range.
    fn leads(root: &Path, rel: &str, target: Option<Name>) -> Vec<(String, String, String)> {
        let project = project::scan(root, &HashMap::new()).unwrap();
        let found = resolve(&Lookup::new(&project, None), &root.join(rel), &target.unwrap());
        let text = |found: &Target| project.text(&found.path).unwrap()[found.range.clone()].to_owned();
        let relative = |found: &Target| found.path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
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
        let table = Some(Name::new(Kind::Table { source: "raw".into() }, None, "orders"));
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
        let function = |package: Option<&str>, name: &str| Some(Name::new(Kind::Function, package, name));
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
        let named = |name: &str| Name { kind: Kind::Ref, package: None, name: name.into(), version: None };
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
            Some(Name { kind: Kind::Ref, package: None, name: name.into(), version: version.map(str::to_owned) })
        };
        let ids =
            |target| -> Vec<String> { leads(&root, "models/c.sql", target).into_iter().map(|(id, ..)| id).collect() };
        // As both builds resolve them: `m.sql` is the latest, version 2, as no file has its default name.
        assert_eq!(ids(reference("m", None)), ["model.r.m.v2"]);
        assert_eq!(ids(reference("m", Some("1"))), ["model.r.m.v1"]);
        assert_eq!(ids(reference("x", Some("1"))), Vec::<String>::new(), "no version of a model without versions");
        assert_eq!(ids(reference("gone", None)), Vec::<String>::new(), "a latest with no file, which dbt fails on");
        let named = |kind: Kind| Some(Name::new(kind, None, "m"));
        let each = ["model.r.m.v1", "model.r.m.v2"];
        let mut entry = ids(named(Kind::Entry { label: "model" }));
        entry.sort();
        assert_eq!(entry, each, "an entry names each version");
        let mut unit = ids(named(Kind::UnitTestModel));
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
        let files_of = |lookup: &Lookup, target: Name| -> Vec<String> {
            let found = resolve(lookup, &root.join("models/m.sql"), &target);
            found.iter().map(|found| found.path.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect()
        };
        let reference = |name: &str| Name { kind: Kind::Ref, package: None, name: name.into(), version: None };
        let parsed = Lookup::new(&project, Some(&index));
        assert_eq!(files_of(&parsed, reference("fact")), ["models/fact_v2.sql"], "the latest alone");
        let v1 = Name { kind: Kind::Ref, package: None, name: "fact".into(), version: Some("1".into()) };
        assert_eq!(files_of(&parsed, v1), ["models/fact.sql"], "the version its file holds");
        assert_eq!(files_of(&parsed, reference("twin")), ["models/a/twin.sql"], "the one dbt keeps");
        let unit = Name::new(Kind::UnitTestModel, None, "fact");
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
            let target =
                Name { kind: Kind::Ref, package: None, name: name.into(), version: version.map(str::to_owned) };
            let relative = |found: &Target| found.path.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
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
        let entry = Name::new(Kind::Entry { label: "model" }, None, "dim");
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
        let function = |package: Option<&str>| Some(Name::new(Kind::Function, package, "f"));
        assert_eq!(at(sql, text, "function('f')", 10), function(None), "at its name");
        assert_eq!(at(sql, text, "'r', 'f'", 1), function(Some("r")), "at its package");
        let keyword = Some(Name::new(Kind::Macro, None, "function"));
        assert_eq!(at(sql, text, "function('f')", 2), keyword, "its keyword a macro call, which a project may define");
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
        let file = &project.files[&path];
        let target = reference_at(&path, file, file.text.find(needle).unwrap() + delta);
        let found = references(&lookup, &path, target.as_ref().unwrap(), declaration);
        // A declaration is no use: only the calls lead back.
        for use_ in found.iter().filter(|_| !declaration) {
            let file = &project.files[&use_.path];
            let back =
                reference_at(&use_.path, file, use_.range.start).map(|target| resolve(&lookup, &use_.path, &target));
            assert!(back.unwrap_or_default().iter().any(|found| found.id == use_.id), "{use_:?} leads elsewhere");
        }
        let relative = |found: &Target| found.path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
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
        let table = Some(Name::new(Kind::Table { source: "raw".into() }, None, "orders"));
        let events = Some(Name::new(Kind::Table { source: "ext".into() }, None, "events"));
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
            let found = leads(&root, pkg_yml, named(Kind::Doc, None, "order_status"));
            assert_eq!(found[0].0, "doc.shop.order_status", "doc with the flag {value:?}");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_source_leads_to_its_one_entry_and_any_package_to_every_declaration() {
        let root = shop("navigation-sources", "name: shop\n");
        let source = Some(Name::new(Kind::Source, None, "raw"));
        // One entry for the source, however many tables it has.
        assert_eq!(leads(&root, "models/_models.yml", source), lead("source.shop.raw", "models/_sources.yml", "raw"));
        let events = Some(Name::new(Kind::Table { source: "ext".into() }, None, "events"));
        let found: Vec<String> = leads(&root, "models/_models.yml", events).into_iter().map(|(id, ..)| id).collect();
        assert_eq!(found, ["source.other.ext.events", "source.pkg.ext.events"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_doc_looks_in_the_root_first() {
        let root = shop("navigation-docs", "name: shop\n");
        let pkg_yml = "dbt_packages/pkg/models/_pkg.yml";
        let found = leads(&root, pkg_yml, named(Kind::Doc, None, "order_status"));
        assert_eq!(found, lead("doc.shop.order_status", "models/docs.md", "order_status"));
        // From the root or a package, that of the package whose name sorts first.
        assert_eq!(leads(&root, "models/_models.yml", named(Kind::Doc, None, "pkg_only"))[0].0, "doc.other.pkg_only");
        assert_eq!(leads(&root, pkg_yml, named(Kind::Doc, None, "pkg_only"))[0].0, "doc.other.pkg_only");
        assert_eq!(
            leads(&root, "models/_models.yml", named(Kind::Doc, Some("pkg"), "order_status"))[0].0,
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
            assert_eq!(leads(&root, &yml, named(Kind::Doc, None, "shared"))[0].0, "doc.aaa.shared", "from {yml}");
        }
        let yml = "dbt_packages/shop_utils/models/_models.yml";
        assert_eq!(leads(&root, yml, named(Kind::Doc, None, "both")), lead("doc.shop.both", "models/docs.md", "both"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_generic_test_is_its_test_block_in_its_file_s_package_or_the_root() {
        let root = shop("navigation-tests", "name: shop\n");
        let yml = "models/_models.yml";
        // The test block, not the macro of the same name beside it.
        assert_eq!(
            leads(&root, yml, named(Kind::Test, None, "is_even")),
            lead("macro.shop.test_is_even", "macros/tests.sql", "is_even")
        );
        // A root test overrides dbt's own; dbt's own are not in the project.
        assert_eq!(leads(&root, yml, named(Kind::Test, None, "unique"))[0].0, "macro.shop.test_unique");
        assert_eq!(leads(&root, yml, named(Kind::Test, None, "not_null")), Vec::new());
        // Never another installed package's without its name.
        assert_eq!(leads(&root, yml, named(Kind::Test, None, "pkg_test")), Vec::new());
        assert_eq!(leads(&root, yml, named(Kind::Test, Some("pkg"), "pkg_test"))[0].0, "macro.pkg.test_pkg_test");
        // A package's file finds its own package's test first.
        let pkg_yml = "dbt_packages/pkg/models/_pkg.yml";
        assert_eq!(leads(&root, pkg_yml, named(Kind::Test, None, "is_even"))[0].0, "macro.pkg.test_is_even");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hover_shows_a_doc_s_text_and_a_test_s_signature() {
        let root = shop("navigation-hover", "name: shop\n");
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let path = root.join("models/_models.yml");
        let shown =
            |target: Option<Name>, markdown| hover(&Lookup::new(&project, None), &path, &target.unwrap(), markdown);
        assert_eq!(shown(named(Kind::Doc, None, "order_status"), true).as_deref(), Some("The root's."));
        // An empty docs block shows nothing, so that the server's answer passes.
        assert_eq!(shown(named(Kind::Doc, None, "empty"), true), None);
        let signature = "```jinja\ntest is_even(model, column_name)\n```\n\nin package `shop`";
        assert_eq!(shown(named(Kind::Test, None, "is_even"), true).as_deref(), Some(signature));
        let plain = "test is_even(model, column_name)\n\nin package shop";
        assert_eq!(shown(named(Kind::Test, None, "is_even"), false).as_deref(), Some(plain));
        // A model's file holds no block.
        assert_eq!(shown(called(None, "orders"), true), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_entry_names_a_resource_of_its_own_package() {
        let root = shop("navigation-entries", "name: shop\n");
        let (yml, pkg_yml) = ("models/_models.yml", "dbt_packages/pkg/models/_pkg.yml");
        // dbt 2.0.9 lets no entry describe another package's resource.
        let expected = [
            (yml, Kind::Entry { label: "model" }, "orders", Some("model.shop.orders")),
            (pkg_yml, Kind::Entry { label: "model" }, "orders", Some("model.pkg.orders")),
            (yml, Kind::Entry { label: "model" }, "only_pkg", None),
            (yml, Kind::Entry { label: "seed" }, "raw", Some("seed.shop.raw")),
            (yml, Kind::Entry { label: "snapshot" }, "snap", Some("snapshot.shop.snap")),
            (yml, Kind::Entry { label: "macro" }, "test_is_even", Some("macro.shop.test_is_even")),
            (yml, Kind::Entry { label: "macro" }, "pkg_macro", None),
            (yml, Kind::Entry { label: "analysis" }, "a", Some("analysis.shop.a")),
            (pkg_yml, Kind::Entry { label: "analysis" }, "a", Some("analysis.pkg.a")),
            (yml, Kind::UnitTestModel, "orders", Some("model.shop.orders")),
            (yml, Kind::UnitTestModel, "only_pkg", None),
        ];
        for (file, kind, name, id) in expected {
            let found = leads(&root, file, named(kind.clone(), None, name));
            assert_eq!(found.first().map(|(found, ..)| found.as_str()), id, "{kind:?} {name} from {file}");
        }
        assert_eq!(
            leads(&root, yml, named(Kind::Entry { label: "seed" }, None, "raw")),
            lead("seed.shop.raw", "seeds/raw.csv", "")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_name_s_node_is_the_last_parse_s_node_where_its_definition_leads() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/orders.sql", "select 1"),
            ("models/customers.sql", "select 2"),
            ("models/off.sql", "select 3"),
            ("models/dim_v1.sql", "select 4"),
            ("models/dim_v2.sql", "select 5"),
            ("models/_sources.yml", "version: 2\nsources:\n  - name: raw\n    tables:\n      - name: payments\n"),
            ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
            ("dbt_packages/pkg/models/orders.sql", "select 6"),
            ("dbt_packages/pkg/models/customers.sql", "select 7"),
        ];
        let root = scratch_project("navigation-node", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        // Each node with every field a lookup reads; no file of `gone` or `fresh` exists.
        let model = |id: &str, file: &str| {
            let parts: Vec<&str> = id.split('.').collect();
            json!({"unique_id": id, "name": parts[2], "resource_type": "model", "package_name": parts[1],
                   "original_file_path": file})
        };
        let version = |number: u64| {
            let mut node = model(&format!("model.shop.dim.v{number}"), &format!("models/dim_v{number}.sql"));
            node["version"] = json!(number);
            node["latest_version"] = json!(2);
            node
        };
        let payments = json!({"unique_id": "source.shop.raw.payments", "name": "payments", "source_name": "raw",
                              "resource_type": "source", "package_name": "shop",
                              "original_file_path": "models/_sources.yml"});
        // The manifest of a parse that found the model EXTRA beside the others.
        let manifest = |extra: &str| {
            let mut nodes = json!({
                "model.shop.orders": model("model.shop.orders", "models/orders.sql"),
                "model.pkg.orders": model("model.pkg.orders", "models/orders.sql"),
                "model.shop.dim.v1": version(1),
                "model.shop.dim.v2": version(2),
                "model.pkg.customers": model("model.pkg.customers", "models/customers.sql"),
            });
            let id = format!("model.shop.{extra}");
            nodes[&id] = model(&id, &format!("models/{extra}.sql"));
            json!({"metadata": {"project_name": "shop"}, "nodes": nodes,
                   "sources": {"source.shop.raw.payments": payments},
                   "disabled": {"model.shop.customers": [model("model.shop.customers", "models/customers.sql")],
                                "model.shop.off": [model("model.shop.off", "models/off.sql")]}})
        };
        let mut index = ParseIndex::of(&manifest("gone"));
        index.overlay(ParseIndex::of(&manifest("fresh")));
        let called = |text: &str| {
            let calls = scan::calls(text).into_iter();
            calls.map(|call| call.name).find(|name| matches!(name.kind, Kind::Ref | Kind::Table { .. })).unwrap()
        };
        let id = |lookup: &Lookup, file: &str, text: &str| {
            let node = lookup.node(&root.join(file), &called(text));
            node.map(|node| node["unique_id"].as_str().unwrap().to_owned())
        };
        let lookup = Lookup::new(&project, Some(&index));
        let uses = "models/uses.sql";
        for (text, expected) in [
            ("{{ ref('dim', v=1) }}", Some("model.shop.dim.v1")),
            ("{{ ref('dim') }}", Some("model.shop.dim.v2")),
            ("{{ ref('dim', v=var('x')) }}", None),
            ("{{ ref('orders') }}", Some("model.shop.orders")),
            ("{{ ref('customers') }}", Some("model.pkg.customers")),
            ("{{ ref('off') }}", None),
            ("{{ ref('gone') }}", None),
            ("{{ ref('fresh') }}", Some("model.shop.fresh")),
            ("{{ source('raw', 'payments') }}", Some("source.shop.raw.payments")),
        ] {
            assert_eq!(id(&lookup, uses, text).as_deref(), expected, "{text}");
        }
        assert!(index.node("model.shop.gone").is_some(), "the index keeps what the last manifest lacks");
        let theirs = "dbt_packages/pkg/models/uses.sql";
        assert_eq!(id(&lookup, theirs, "{{ ref('orders') }}").as_deref(), Some("model.pkg.orders"));
        assert_eq!(id(&Lookup::new(&project, None), uses, "{{ ref('orders') }}"), None, "no index");
        std::fs::remove_dir_all(root).unwrap();
    }
}
