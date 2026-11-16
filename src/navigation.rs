// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Where a name leads, for the definitions that dbt lsp 2.0.9 does not
//! give: the target at a position, as its document alone shows it, and
//! what it names in the project, by dbt 2.0.9's lookup.  Pure.

use crate::project::{self, Entity, Project};
use crate::sources;
use crate::text::LineIndex;
use crate::yaml::{self, Node};
use crate::{properties, scan};
use regex::Regex;
use std::collections::BTreeMap;
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
const LOOKUP: [(&str, &[&str], &str, &[Step]); 8] = [
    ("doc", &["doc"], "", &[Step::Root, Step::Any]),
    ("test", &["macro"], "test_", &[Step::Own, Step::Root]),
    ("unit test model", &["model"], "", &[Step::Own]),
    ("model", &["model"], "", &[Step::Own]),
    ("seed", &["seed"], "", &[Step::Own]),
    ("snapshot", &["snapshot"], "", &[Step::Own]),
    ("analysis", &["analysis"], "", &[Step::Own]),
    ("macro", &["macro"], "", &[Step::Own]),
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
/// text alone: a `source` anywhere; in YAML, a `ref` that names no version
/// (whose file the scan does not know), a `doc`, the `name:` of a
/// navigable entry, a generic test's name, or a unit test's `model:`.
pub fn target_at(path: &Path, text: &str, offset: usize) -> Option<Target> {
    let inside = |range: &Range<usize>| range.start <= offset && offset <= range.end;
    let source = sources::target_at(path, text, offset).map(Target::Source);
    if source.is_some() || !sources::is_yaml(path) {
        return source;
    }
    let named = |kind: &'static str, package: Option<&str>, name: &str| Target::Named {
        kind,
        package: package.map(str::to_owned),
        name: name.to_owned(),
    };
    let reference = scan::yaml_calls(text).into_iter().find_map(|call| match call.target {
        scan::Target::Ref { package, name, version: None } if call.arguments.iter().any(inside) => {
            Some(Target::Call(scan::Target::Ref { package, name, version: None }))
        }
        _ => None,
    });
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
        let index = LineIndex::new(text);
        let models = root.get("unit_tests").map_or(&[][..], Node::items).iter().filter_map(|unit| unit.get("model"));
        let (name, _) =
            models.filter_map(|model| project::written(text, &index, model)).find(|(_, range)| inside(range))?;
        Some(named("unit test model", None, &name))
    };
    reference.or_else(doc).or_else(entry).or_else(test).or_else(model)
}

/// dbt 2.0.9's lookup over a project, made once for a request or a graph:
/// the lookup flag read once, and the resources by id.
pub struct Lookup<'a> {
    project: &'a Project,
    /// Each resource by id, in order, so that a package's macros read as a
    /// range.
    by_id: BTreeMap<&'a str, &'a Entity>,
    /// The packages, the root first, then the others in the byte order of
    /// their names, which is the order of their paths, since dbt installs
    /// each at `dbt_packages/<name>`.
    packages: Vec<&'a str>,
    /// Whether the lookup flag puts the root project before the calling
    /// file's package (see `Step`).
    root_first: bool,
}

impl<'a> Lookup<'a> {
    /// The lookup of PROJECT.
    pub fn new(project: &'a Project) -> Lookup<'a> {
        let root = project.root_package.as_str();
        let config = project.packages.get(root).and_then(|dir| project.text(&dir.join("dbt_project.yml")));
        let root_first = config.and_then(yaml::parse).is_some_and(|config| {
            let value =
                config.get("flags").and_then(|flags| flags.get("require_ref_searches_node_package_before_root"));
            // As dbt reads the flag: neither `fAlse` nor a quoted `'false'`.
            let yaml_false = ["false", "False", "FALSE"];
            matches!(value, Some(Node::Scalar { value, plain: true, .. }) if yaml_false.contains(&value.as_str()))
        });
        let mut packages: Vec<&str> = project.packages.keys().map(String::as_str).collect();
        packages.sort_by_key(|package| (*package != root, *package));
        let by_id = project.entities.iter().map(|entity| (entity.id.as_str(), entity)).collect();
        Lookup { project, by_id, packages, root_first }
    }

    /// The package of the file at PATH: the installed package whose
    /// directory holds it, else the root project.
    pub fn package_of(&self, path: &Path) -> &'a str {
        let holders = self.project.packages.iter().filter(|(_, dir)| path.starts_with(dir));
        let deepest = holders.max_by_key(|(_, dir)| dir.as_os_str().len());
        deepest.map_or(self.project.root_package.as_str(), |(name, _)| name.as_str())
    }

    /// What a call of TARGET from the package OWN names, by dbt 2.0.9's
    /// lookup: in the package the call names; else a `ref`, a `source` or a
    /// dispatch in OWN and the root, in the flag's order, then in any
    /// package; an unqualified macro in OWN, then the root only, since dbt
    /// keeps another package's macros behind its name.  Every resource of
    /// the first of these that has some, in the order of their paths: one,
    /// but where dbt fails on several (dbt1005), and a dispatch's every
    /// implementation (`*__name`).
    pub fn call(&self, own: &str, target: &scan::Target) -> Vec<&'a Entity> {
        let root = self.project.root_package.as_str();
        let (first, second) = if self.root_first { (root, own) } else { (own, root) };
        let steps = match target {
            scan::Target::Ref { package: Some(package), .. }
            | scan::Target::Macro { package: Some(package), .. }
            | scan::Target::Dispatch { package: Some(package), .. } => vec![Some(package.as_str())],
            scan::Target::Macro { package: None, .. } => vec![Some(own), Some(root)],
            _ => vec![Some(first), Some(second), None],
        };
        let get = |id: String| self.by_id.get(id.as_str()).copied();
        let in_package = |package: &str| -> Vec<&'a Entity> {
            match target {
                scan::Target::Ref { name, .. } => ["model", "seed", "snapshot"]
                    .iter()
                    .filter_map(|kind| get(format!("{kind}.{package}.{name}")))
                    .collect(),
                scan::Target::Source { source, table } => {
                    get(format!("source.{package}.{source}.{table}")).into_iter().collect()
                }
                scan::Target::Macro { name, .. } => get(format!("macro.{package}.{name}")).into_iter().collect(),
                scan::Target::Dispatch { name, .. } => {
                    let (prefix, suffix) = (format!("macro.{package}."), format!("__{name}"));
                    let in_package = self.by_id.range(prefix.as_str()..).take_while(|(id, _)| id.starts_with(&prefix));
                    in_package.filter(|(id, _)| id.ends_with(&suffix)).map(|(_, entity)| *entity).collect()
                }
            }
        };
        let in_step = |step: &Option<&str>| {
            let mut found = match step {
                Some(package) => in_package(package),
                None => self.packages.iter().flat_map(|package| in_package(package)).collect(),
            };
            found.sort_by(|one, other| (&one.path, one.range.start).cmp(&(&other.path, other.range.start)));
            found
        };
        steps.iter().map(in_step).find(|found| !found.is_empty()).unwrap_or_default()
    }
}

/// What TARGET, named in the file at PATH, leads to by LOOKUP, dbt 2.0.9's:
/// a call as `Lookup::call` resolves it; a source looked up as a `ref`,
/// every declaration of the first package that has some; any other name in
/// the package it gives, else as `LOOKUP` says, the first that exists.
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
        Target::Call(call) => {
            let found = |entity: &&Entity| Found {
                id: entity.id.clone(),
                path: entity.path.clone(),
                range: entity.name_range.clone(),
            };
            return lookup.call(own, call).iter().map(found).collect();
        }
        Target::Named { kind, package, name } => (kind, package, name),
    };
    let Some((_, types, prefix, steps)) = LOOKUP.iter().find(|(row, ..)| row == kind) else { return Vec::new() };
    let steps = package.as_deref().map_or_else(|| packages(steps), |package| vec![Some(package)]);
    let matching = project.entities.iter().filter(|entity| {
        types.contains(&entity.resource_type)
            && entity.id == format!("{}.{}.{prefix}{name}", entity.resource_type, entity.package)
    });
    let found = matching.map(|entity| {
        let found = Found { id: entity.id.clone(), path: entity.path.clone(), range: entity.name_range.clone() };
        (entity.package.as_str(), found)
    });
    first_step(found.collect(), &steps, false)
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
        // A ref naming a version: the scan knows no versions' files.
        assert_eq!(at(yml, MODELS, "ref('orders', v=2)", 6), None);
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
    fn finds_only_a_source_in_sql() {
        // The server answers a ref there, and dbt has no doc in SQL.
        let sql = Path::new("/p/models/customers.sql");
        let text = "select * from {{ source('raw', 'orders') }} join {{ ref('orders') }} -- {{ doc('order_status') }}";
        let table = sources::Target::Table { source: "raw".into(), table: "orders".into() };
        assert_eq!(at(sql, text, "'orders') }} join", 2), Some(Target::Source(table)));
        assert_eq!(at(sql, text, "ref('orders')", 6), None);
        assert_eq!(at(sql, text, "order_status", 2), None);
    }

    /// Where TARGET, named in the file REL of the project at ROOT, leads:
    /// each id with its file relative to ROOT and the text of its range.
    fn leads(root: &Path, rel: &str, target: Option<Target>) -> Vec<(String, String, String)> {
        let project = project::scan(root, &HashMap::new()).unwrap();
        let found = resolve(&Lookup::new(&project), &root.join(rel), &target.unwrap());
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
            let found = resolve(&Lookup::new(&project), &path, &target);
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
