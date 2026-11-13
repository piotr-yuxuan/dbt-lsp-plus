// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Where a name leads, for the definitions that dbt lsp 2.0.9 does not
//! give: the target at a position, as its document alone shows it, and
//! what it names in the project, by dbt 2.0.9's lookup.  Pure.

use crate::project::{self, Project};
use crate::sources;
use crate::text::LineIndex;
use crate::yaml::{self, Node};
use crate::{properties, scan};
use regex::Regex;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// What a position names.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A `source()` argument, or a source's or table's `name:`.
    Source(sources::Target),
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
/// way round under `flags: require_ref_searches_node_package_before_root:
/// false`.
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

/// How dbt 2.0.9 looks up each kind of name with no package of its own:
/// the kind, the resource types it may name, the prefix of the name in
/// their ids, and the packages searched in turn.  A `ref`, `doc` or
/// generic test names its kind; the `name:` of an entry, the entry's label;
/// a unit test's `model:`, `unit test model`.
const LOOKUP: [(&str, &[&str], &str, &[Step]); 9] = [
    ("ref", &["model", "seed", "snapshot"], "", CALLS),
    ("doc", &["doc"], "", &[Step::Root, Step::Own, Step::Any]),
    ("test", &["macro"], "test_", &[Step::Own, Step::Root]),
    ("unit test model", &["model"], "", &[Step::Own]),
    ("model", &["model"], "", &[Step::Own, Step::Any]),
    ("seed", &["seed"], "", &[Step::Own, Step::Any]),
    ("snapshot", &["snapshot"], "", &[Step::Own, Step::Any]),
    ("analysis", &["analysis"], "", &[Step::Any]),
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
        // A keyword argument after the name, such as `v=2`, names a version.
        scan::Target::Ref { package, name }
            if call.arguments.iter().any(inside) && !text[call.range.end..].trim_start().starts_with(',') =>
        {
            Some(named("ref", package.as_deref(), &name))
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

/// What TARGET, named in the file at PATH, leads to in PROJECT, by dbt
/// 2.0.9's lookup: in the package the name gives, else as `LOOKUP` says,
/// where a file's own package is the installed package whose directory
/// holds it, else the root project.  A source is looked up as a `ref`.  The
/// first package that has something gives one result, or every
/// declaration of a source.
pub fn resolve(project: &Project, path: &Path, target: &Target) -> Vec<Found> {
    let root = project.root_package.as_str();
    let holders = project.packages.iter().filter(|(_, dir)| path.starts_with(dir));
    let own = holders.max_by_key(|(_, dir)| dir.as_os_str().len()).map_or(root, |(name, _)| name.as_str());
    let config = project.packages.get(root).and_then(|dir| project.texts.get(&dir.join("dbt_project.yml")));
    let root_first = config.and_then(|text| yaml::parse(text)).is_some_and(|config| {
        let value = config.get("flags").and_then(|flags| flags.get("require_ref_searches_node_package_before_root"));
        value.and_then(Node::as_str).is_some_and(|value| value.eq_ignore_ascii_case("false"))
    });
    let (first, second) = if root_first { (root, own) } else { (own, root) };
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
/// written; a test's opening tag in a code block, and its package.
pub fn hover(project: &Project, target: &Target, found: &Found) -> Option<String> {
    let Target::Named { kind: kind @ ("doc" | "test"), .. } = target else { return None };
    let text = project.texts.get(&found.path)?;
    let block = scan::blocks(text).into_iter().find(|block| block.name_range == found.range)?;
    if *kind == "doc" {
        return Some(text[block.inner].trim().to_owned());
    }
    let tag = text[block.range.start..block.inner.start].trim_start_matches("{%").trim_end_matches("%}");
    let package = found.id.split('.').nth(1).unwrap_or_default();
    Some(format!("```jinja\n{}\n```\n\nin package `{package}`", tag.trim_matches(['-', '+', ' ', '\t', '\n'])))
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

    const SOURCES: &str = "version: 2\nsources:\n  - name: raw\n    tables:\n      - name: orders\n";

    fn shop(name: &str, root_config: &str) -> PathBuf {
        scratch_project(
            name,
            &[
                ("dbt_project.yml", root_config),
                ("models/orders.sql", "select 1"),
                ("models/customers.sql", "select * from {{ source('raw', 'orders') }} -- {{ doc('order_status') }}"),
                ("models/_models.yml", MODELS),
                ("models/_sources.yml", SOURCES),
                ("models/docs.md", "{% docs order_status %}\nThe root's.\n{% enddocs %}\n"),
                ("analyses/a.sql", "select 1"),
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
                (
                    "dbt_packages/pkg/models/docs.md",
                    "{% docs order_status %}\nThe package's.\n{% enddocs %}\n\
                     {% docs pkg_only %}\nOnly here.\n{% enddocs %}\n",
                ),
                (
                    "dbt_packages/pkg/macros/m.sql",
                    "{% test pkg_test(model) %}\nselect 1\n{% endtest %}\n{% macro pkg_macro() %}{% endmacro %}\n",
                ),
            ],
        )
    }

    /// The target at the first occurrence of NEEDLE in TEXT, DELTA bytes in.
    fn at(path: &Path, text: &str, needle: &str, delta: usize) -> Option<Target> {
        target_at(path, text, text.find(needle).unwrap() + delta)
    }

    fn named(kind: &'static str, package: Option<&str>, name: &str) -> Option<Target> {
        Some(Target::Named { kind, package: package.map(str::to_owned), name: name.to_owned() })
    }

    const YML: &str = "/p/models/_models.yml";

    #[test]
    fn finds_the_calls_of_yaml_from_the_document_alone() {
        let yml = Path::new(YML);
        assert_eq!(at(yml, MODELS, "ref('customers')", 6), named("ref", None, "customers"));
        assert_eq!(at(yml, MODELS, "ref('pkg', 'orders')", 14), named("ref", Some("pkg"), "orders"));
        // A doc's quotes as YAML escapes them in single and double quotes.
        assert_eq!(at(yml, MODELS, "order_status''", 2), named("doc", None, "order_status"));
        assert_eq!(at(yml, MODELS, "pkg_only", 2), named("doc", None, "pkg_only"));
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
        let found = resolve(&project, &root.join(rel), &target.unwrap());
        let text = |found: &Found| project.texts[&found.path][found.range.clone()].to_owned();
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
        assert_eq!(
            leads(&root, root_yml, named("ref", None, "orders")),
            lead("model.shop.orders", "models/orders.sql", "")
        );
        assert_eq!(
            leads(&root, pkg_yml, named("ref", None, "orders")),
            lead("model.pkg.orders", "dbt_packages/pkg/models/orders.sql", "")
        );
        assert_eq!(leads(&root, root_yml, named("ref", Some("pkg"), "orders"))[0].0, "model.pkg.orders");
        assert_eq!(leads(&root, root_yml, named("ref", None, "only_pkg"))[0].0, "model.pkg.only_pkg");
        assert_eq!(leads(&root, root_yml, named("ref", Some("shop"), "only_pkg")), Vec::new());
        let table = Some(Target::Source(sources::Target::Table { source: "raw".into(), table: "orders".into() }));
        assert_eq!(
            leads(&root, root_yml, table.clone()),
            lead("source.shop.raw.orders", "models/_sources.yml", "orders")
        );
        assert_eq!(leads(&root, pkg_yml, table.clone())[0].0, "source.pkg.raw.orders");
        let flagged =
            shop("navigation-flag", "name: shop\nflags:\n  require_ref_searches_node_package_before_root: false\n");
        assert_eq!(leads(&flagged, pkg_yml, named("ref", None, "orders"))[0].0, "model.shop.orders");
        assert_eq!(leads(&flagged, pkg_yml, table)[0].0, "source.shop.raw.orders");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(flagged).unwrap();
    }

    #[test]
    fn a_doc_looks_in_the_root_first() {
        let root = shop("navigation-docs", "name: shop\n");
        let pkg_yml = "dbt_packages/pkg/models/_pkg.yml";
        let found = leads(&root, pkg_yml, named("doc", None, "order_status"));
        assert_eq!(found, lead("doc.shop.order_status", "models/docs.md", "order_status"));
        assert_eq!(leads(&root, "models/_models.yml", named("doc", None, "pkg_only"))[0].0, "doc.pkg.pkg_only");
        assert_eq!(
            leads(&root, "models/_models.yml", named("doc", Some("pkg"), "order_status"))[0].0,
            "doc.pkg.order_status"
        );
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
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hover_shows_a_doc_s_text_and_a_test_s_signature() {
        let root = shop("navigation-hover", "name: shop\n");
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let path = root.join("models/_models.yml");
        let shown = |target: Option<Target>| {
            let target = target.unwrap();
            let found = resolve(&project, &path, &target);
            hover(&project, &target, &found[0])
        };
        assert_eq!(shown(named("doc", None, "order_status")).as_deref(), Some("The root's."));
        let signature = "```jinja\ntest is_even(model, column_name)\n```\n\nin package `shop`";
        assert_eq!(shown(named("test", None, "is_even")).as_deref(), Some(signature));
        assert_eq!(shown(named("ref", None, "orders")), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_entry_names_a_resource_by_its_type_s_rule() {
        let root = shop("navigation-entries", "name: shop\n");
        let yml = "models/_models.yml";
        assert_eq!(leads(&root, yml, named("model", None, "orders"))[0].0, "model.shop.orders");
        // A model of another package, which root properties may describe.
        assert_eq!(leads(&root, yml, named("model", None, "only_pkg"))[0].0, "model.pkg.only_pkg");
        assert_eq!(leads(&root, yml, named("macro", None, "test_is_even"))[0].0, "macro.shop.test_is_even");
        assert_eq!(leads(&root, yml, named("macro", None, "pkg_macro")), Vec::new());
        assert_eq!(leads(&root, yml, named("analysis", None, "a"))[0].0, "analysis.shop.a");
        assert_eq!(leads(&root, yml, named("unit test model", None, "orders"))[0].0, "model.shop.orders");
        assert_eq!(leads(&root, yml, named("unit test model", None, "only_pkg")), Vec::new());
        std::fs::remove_dir_all(root).unwrap();
    }
}
