// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! References and renames of sources, which dbt lsp 2.0.9 answers with
//! nothing.  A source is named by `source('s', 't')` calls, in SQL inside
//! Jinja tags and in YAML without them, and declared under `sources:` in a
//! properties file.  Every function here is pure.

use crate::project::{Insertion, Project, SourceTable};
use crate::scan::{self, Call};
use crate::text::{Encoding, LineIndex};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// What a position names: a source table, or a whole source.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Table { source: String, table: String },
    Source { source: String },
}

/// A place where a source or a table is named, and whether it declares it.
#[derive(Clone, Debug, PartialEq)]
pub struct Occurrence {
    pub path: PathBuf,
    pub range: Range<usize>,
    pub declaration: bool,
}

fn is_yaml(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("yml" | "yaml"))
}

/// The calls of TEXT, the file at PATH.  A Python model's `dbt.source()`
/// reads as a call in YAML does: outside Jinja, with `#` comments.
fn file_calls(path: &Path, text: &str) -> Vec<Call> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yml" | "yaml" | "py") => scan::yaml_calls(text),
        _ => scan::calls(text),
    }
}

/// The argument of CALL that names TARGET, if CALL is a `source()` naming it.
fn argument_naming<'a>(call: &'a Call, target: &Target) -> Option<&'a Range<usize>> {
    let scan::Target::Source { source, table } = &call.target else { return None };
    match target {
        Target::Table { source: s, table: t } if s == source && t == table => call.arguments.get(1),
        Target::Source { source: s } if s == source => call.arguments.first(),
        _ => None,
    }
}

/// The source target at OFFSET of TEXT, the file at PATH: an argument of a
/// `source()` call, or a source's or table's `name:` under `sources:`.
pub fn target_at(path: &Path, text: &str, offset: usize) -> Option<Target> {
    let inside = |range: &Range<usize>| range.start <= offset && offset <= range.end;
    for call in file_calls(path, text) {
        let scan::Target::Source { source, table } = call.target else { continue };
        match (call.arguments.first().is_some_and(inside), call.arguments.get(1).is_some_and(inside)) {
            (true, _) => return Some(Target::Source { source }),
            (_, true) => return Some(Target::Table { source, table }),
            _ => {}
        }
    }
    let tables = if is_yaml(path) { crate::project::source_tables("", path, text) } else { Vec::new() };
    tables.into_iter().find_map(|declared| match (inside(&declared.source_range), inside(&declared.table_range)) {
        (true, _) => Some(Target::Source { source: declared.source }),
        (_, true) => Some(Target::Table { source: declared.source, table: declared.table }),
        _ => None,
    })
}

/// Whether DECLARED declares TARGET.
fn declares(declared: &SourceTable, target: &Target) -> bool {
    match target {
        Target::Table { source, table } => declared.source == *source && declared.table == *table,
        Target::Source { source } => declared.source == *source,
    }
}

/// Every place of PROJECT that names TARGET, ordered by file and offset.
pub fn occurrences(project: &Project, target: &Target) -> Vec<Occurrence> {
    let mut found = Vec::new();
    for (path, text) in &project.texts {
        let calls = file_calls(path, text);
        let named = calls.iter().filter_map(|call| argument_naming(call, target));
        found.extend(named.map(|range| Occurrence { path: path.clone(), range: range.clone(), declaration: false }));
    }
    for declared in project.sources.iter().filter(|declared| declares(declared, target)) {
        let range = match target {
            Target::Table { .. } => declared.table_range.clone(),
            Target::Source { .. } => declared.source_range.clone(),
        };
        found.push(Occurrence { path: declared.path.clone(), range, declaration: true });
    }
    found.sort_by(|a, b| (&a.path, a.range.start).cmp(&(&b.path, b.range.start)));
    found.dedup();
    found
}

/// The LSP range of RANGE in TEXT.
fn lsp_range(text: &str, range: &Range<usize>, encoding: Encoding) -> Value {
    LineIndex::new(text).range(range.start, range.end, encoding)
}

/// The references of TARGET, as `Location`s; its declarations too when
/// DECLARATIONS.  URI_OF names each file as the client does.
pub fn references(
    project: &Project,
    target: &Target,
    declarations: bool,
    encoding: Encoding,
    uri_of: &dyn Fn(&Path) -> String,
) -> Value {
    let found = occurrences(project, target).into_iter().filter(|o| declarations || !o.declaration);
    let locations = found.map(|o| {
        let text = project.texts.get(&o.path).map_or("", String::as_str);
        json!({"uri": uri_of(&o.path), "range": lsp_range(text, &o.range, encoding)})
    });
    Value::Array(locations.collect())
}

static PLAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A[A-Za-z_][A-Za-z0-9_]*\z").unwrap());

/// VALUE as a YAML scalar: plain when it is a plain identifier, else double-quoted.
fn yaml_scalar(value: &str) -> String {
    if PLAIN.is_match(value) {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The text that replaces the name at RANGE of TEXT with NEW, which must not
/// be empty: the name alone inside its quotes, which it must not hold; a
/// YAML scalar where unquoted.
fn replacement(text: &str, range: &Range<usize>, new: &str) -> Result<String, String> {
    match text[..range.start].chars().next_back().filter(|c| matches!(c, '\'' | '"')) {
        _ if new.is_empty() => Err("the new name is empty".into()),
        Some(quote) if new.contains([quote, '\\']) => Err(format!("the new name cannot hold {quote} or \\ here")),
        Some(_) => Ok(new.to_owned()),
        None => Ok(yaml_scalar(new)),
    }
}

/// The lines that keep the relation of TARGET once renamed: `identifier:
/// <old>` under a table without one, `schema: <old>` under a source without
/// one, as (file, offset, text).  A flow-style entry, where no line can go,
/// refuses the rename.
fn kept_relation(target: &Target, declared: &[&SourceTable]) -> Result<BTreeSet<(PathBuf, usize, String)>, String> {
    let mut lines = BTreeSet::new();
    for table in declared {
        let (missing, insertion, key, old): (bool, &Insertion, &str, &str) = match target {
            Target::Table { .. } => (!table.has_identifier, &table.table_insertion, "identifier", &table.table),
            Target::Source { .. } => (!table.has_schema, &table.source_insertion, "schema", &table.source),
        };
        match (missing, insertion.block) {
            (false, _) => {}
            (true, false) => {
                return Err(format!("{} declares {old} in flow style: add {key}: first", table.path.display()));
            }
            (true, true) => {
                let line = format!("\n{}{key}: {}", " ".repeat(insertion.indent), yaml_scalar(old));
                lines.insert((table.path.clone(), insertion.at, line));
            }
        }
    }
    Ok(lines)
}

/// The `WorkspaceEdit` that renames TARGET to NEW everywhere in PROJECT and
/// keeps the warehouse relation it names, or the reason it cannot.  URI_OF
/// names each file as the client does: the ranges are those of the text it
/// has open under that name.
pub fn rename(
    project: &Project,
    target: &Target,
    new: &str,
    encoding: Encoding,
    uri_of: &dyn Fn(&Path) -> String,
) -> Result<Value, String> {
    let declared: Vec<&SourceTable> = project.sources.iter().filter(|declared| declares(declared, target)).collect();
    let packages: Vec<&str> =
        declared.iter().map(|declared| declared.package.as_str()).collect::<BTreeSet<_>>().into_iter().collect();
    match packages.as_slice() {
        [] => return Err("this source is declared nowhere in the project".into()),
        // `dbt deps` overwrites an installed package's files; a root `overrides:` would keep the old name.
        [package] if *package != project.root_package => {
            return Err(format!("this source is declared in the installed package {package}: rename it there"));
        }
        [_] => {}
        _ => return Err(format!("this source is declared in several packages: {packages:?}")),
    }
    // Every declaration is an occurrence, so `replacement` judges NEW at least once.
    let mut edits: BTreeMap<PathBuf, Vec<(Range<usize>, String)>> = BTreeMap::new();
    for found in occurrences(project, target) {
        let text = project.texts.get(&found.path).map_or("", String::as_str);
        let new_text = replacement(text, &found.range, new)?;
        edits.entry(found.path).or_default().push((found.range, new_text));
    }
    for (path, at, line) in kept_relation(target, &declared)? {
        edits.entry(path).or_default().push((at..at, line));
    }
    let changes = edits.into_iter().map(|(path, edits)| {
        let text = project.texts.get(&path).map_or("", String::as_str);
        let edits = edits
            .iter()
            .map(|(range, new_text)| json!({"range": lsp_range(text, range, encoding), "newText": new_text}));
        (uri_of(&path), Value::Array(edits.collect()))
    });
    Ok(json!({"changes": changes.collect::<serde_json::Map<String, Value>>()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::uri;
    use crate::project;
    use std::collections::HashMap;
    use std::fs;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop")
    }

    /// A scratch project of FILES, by path, in the system's temporary
    /// directory, and its scan.
    fn scratch_project(name: &str, files: &[(&str, &str)]) -> (PathBuf, Project) {
        let dir = project::scratch_project(&format!("sources-{name}"), files);
        let project = project::scan(&dir, &HashMap::new()).unwrap();
        (dir, project)
    }

    /// The text of each file of PROJECT once EDIT is applied.
    fn apply(project: &Project, edit: &Value) -> BTreeMap<String, String> {
        let mut texts = BTreeMap::new();
        for (file, edits) in edit["changes"].as_object().unwrap() {
            let path = url::Url::parse(file).unwrap().to_file_path().unwrap();
            let mut text = project.texts[&path].clone();
            let mut edits: Vec<&Value> = edits.as_array().unwrap().iter().collect();
            edits.sort_by_key(|e| {
                std::cmp::Reverse((e["range"]["start"]["line"].as_u64(), e["range"]["start"]["character"].as_u64()))
            });
            for edit in edits {
                crate::text::apply_change(
                    &mut text,
                    &json!({"range": edit["range"], "text": edit["newText"]}),
                    Encoding::Utf16,
                );
            }
            texts.insert(path.file_name().unwrap().to_string_lossy().into_owned(), text);
        }
        texts
    }

    const PROJECT: &str = "name: p\n";
    const SOURCES: &str = "\
version: 2
sources:
  - name: raw
    tables:
      - name: customers
        description: It's the customers.
      - name: orders
        identifier: raw_orders
  - name: \"events\"
    schema: analytics
    tables:
      - name: 'page views'
";
    const MODEL: &str =
        "select * from {{ source('raw', 'customers') }} join {{ source(\"raw\", \"orders\") }} using (id)\n";
    const TESTS: &str = "\
models:
  - name: m
    columns:
      - name: id
        tests:
          - relationships:
              to: source('raw', 'customers')  # source('raw', 'other')
              field: id
";

    fn shop(name: &str) -> (PathBuf, Project) {
        let files = [
            ("dbt_project.yml", PROJECT),
            ("models/_sources.yml", SOURCES),
            ("models/m.sql", MODEL),
            ("models/_models.yml", TESTS),
        ];
        scratch_project(name, &files)
    }

    #[test]
    fn finds_the_target_on_a_call_argument_or_a_declared_name() {
        let model = Path::new("m.sql");
        let at = |needle: &str| MODEL.find(needle).unwrap() + 1;
        assert_eq!(target_at(model, MODEL, at("raw")), Some(Target::Source { source: "raw".into() }));
        let customers = Target::Table { source: "raw".into(), table: "customers".into() };
        assert_eq!(target_at(model, MODEL, at("customers")), Some(customers.clone()));
        assert_eq!(target_at(model, MODEL, at("join")), None);
        let yaml = Path::new("_sources.yml");
        assert_eq!(target_at(yaml, SOURCES, SOURCES.find("customers").unwrap()), Some(customers));
        assert_eq!(
            target_at(yaml, SOURCES, SOURCES.find("events").unwrap()),
            Some(Target::Source { source: "events".into() })
        );
        assert_eq!(
            target_at(Path::new("_models.yml"), TESTS, TESTS.find("raw").unwrap()),
            Some(Target::Source { source: "raw".into() })
        );
    }

    #[test]
    fn references_cover_sql_yaml_and_the_declaration() {
        let (dir, project) = shop("references");
        let customers = Target::Table { source: "raw".into(), table: "customers".into() };
        let found = references(&project, &customers, true, Encoding::Utf16, &uri);
        let mut files: Vec<String> = found
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["uri"].as_str().unwrap().rsplit('/').next().unwrap().to_owned())
            .collect();
        files.sort();
        assert_eq!(files, ["_models.yml", "_sources.yml", "m.sql"], "the commented call is no reference");
        let without = references(&project, &customers, false, Encoding::Utf16, &uri);
        assert_eq!(without.as_array().unwrap().len(), 2);
        let in_sql = found.as_array().unwrap().iter().find(|l| l["uri"] == uri(&dir.join("models/m.sql"))).unwrap();
        assert_eq!(
            in_sql["range"],
            json!({"start": {"line": 0, "character": 32}, "end": {"line": 0, "character": 41}})
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn renaming_a_table_keeps_its_identifier() {
        let (dir, project) = shop("rename-table");
        let customers = Target::Table { source: "raw".into(), table: "customers".into() };
        let edit = rename(&project, &customers, "clients", Encoding::Utf16, &uri).unwrap();
        let texts = apply(&project, &edit);
        assert!(texts["m.sql"].starts_with("select * from {{ source('raw', 'clients') }}"));
        assert!(texts["_models.yml"].contains("to: source('raw', 'clients')  # source('raw', 'other')"));
        assert!(
            texts["_sources.yml"]
                .contains("      - name: clients\n        identifier: customers\n        description: It's")
        );
        // orders has an identifier: renaming it adds none.
        let orders = Target::Table { source: "raw".into(), table: "orders".into() };
        let texts = apply(&project, &rename(&project, &orders, "sales", Encoding::Utf16, &uri).unwrap());
        assert!(texts["_sources.yml"].contains("      - name: sales\n        identifier: raw_orders\n"));
        assert!(texts["m.sql"].contains("source(\"raw\", \"sales\")"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn renaming_a_source_keeps_its_schema() {
        let (dir, project) = shop("rename-source");
        let texts = apply(
            &project,
            &rename(&project, &Target::Source { source: "raw".into() }, "landing", Encoding::Utf16, &uri).unwrap(),
        );
        assert!(texts["_sources.yml"].contains("  - name: landing\n    schema: raw\n    tables:"));
        assert_eq!(
            texts["_sources.yml"].matches("schema: raw").count(),
            1,
            "one line for the source, not one per table"
        );
        assert!(
            texts["m.sql"].contains("source('landing', 'customers')")
                && texts["m.sql"].contains("source(\"landing\", \"orders\")")
        );
        // events has a schema, and a quoted name that stays quoted.
        let texts = apply(
            &project,
            &rename(&project, &Target::Source { source: "events".into() }, "web", Encoding::Utf16, &uri).unwrap(),
        );
        assert!(texts["_sources.yml"].contains("  - name: \"web\"\n    schema: analytics\n"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_yaml_values_plain_or_quoted() {
        let (dir, project) = shop("yaml-values");
        let views = Target::Table { source: "events".into(), table: "page views".into() };
        let texts = apply(&project, &rename(&project, &views, "visits", Encoding::Utf16, &uri).unwrap());
        assert!(texts["_sources.yml"].contains("      - name: 'visits'\n        identifier: \"page views\"\n"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn renames_the_calls_of_dbt_project_yml() {
        let config = "name: p\nvars:\n  segment:\n    pages: \"{{ source('raw', 'customers') }}\"\n";
        let files = [("dbt_project.yml", config), ("models/_sources.yml", SOURCES)];
        let (dir, project) = scratch_project("project-calls", &files);
        let customers = Target::Table { source: "raw".into(), table: "customers".into() };
        let texts = apply(&project, &rename(&project, &customers, "clients", Encoding::Utf16, &uri).unwrap());
        assert!(texts["dbt_project.yml"].contains("pages: \"{{ source('raw', 'clients') }}\""), "{texts:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn renames_the_calls_of_a_python_model() {
        let model = "def model(dbt, session):\n    # dbt.source('raw', 'orders')\n    return dbt.source(\"raw\", \"customers\")\n";
        let files = [("dbt_project.yml", PROJECT), ("models/_sources.yml", SOURCES), ("models/py_model.py", model)];
        let (dir, project) = scratch_project("python-calls", &files);
        let customers = Target::Table { source: "raw".into(), table: "customers".into() };
        let texts = apply(&project, &rename(&project, &customers, "clients", Encoding::Utf16, &uri).unwrap());
        assert!(texts["py_model.py"].contains("return dbt.source(\"raw\", \"clients\")"), "{texts:?}");
        let orders = Target::Table { source: "raw".into(), table: "orders".into() };
        let found = occurrences(&project, &orders);
        assert!(!found.iter().any(|o| o.path.ends_with("py_model.py")), "a comment is no call");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_what_would_break_a_relation_or_a_quote() {
        let (dir, project) = shop("refusals");
        let customers = Target::Table { source: "raw".into(), table: "customers".into() };
        assert!(rename(&project, &customers, "it's", Encoding::Utf16, &uri).unwrap_err().contains("cannot hold '"));
        assert!(rename(&project, &customers, "", Encoding::Utf16, &uri).unwrap_err().contains("empty"));
        let missing = Target::Table { source: "raw".into(), table: "nowhere".into() };
        assert!(rename(&project, &missing, "x", Encoding::Utf16, &uri).unwrap_err().contains("declared nowhere"));
        let flow = [("dbt_project.yml", PROJECT), ("models/s.yml", "sources:\n  - {name: s, tables: [{name: t}]}\n")];
        let (flow_dir, project) = scratch_project("flow", &flow);
        let t = Target::Table { source: "s".into(), table: "t".into() };
        assert!(rename(&project, &t, "u", Encoding::Utf16, &uri).unwrap_err().contains("flow style"));
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&flow_dir).unwrap();
    }

    #[test]
    fn refuses_a_source_declared_in_two_packages() {
        let files = [
            ("dbt_project.yml", PROJECT),
            ("models/s.yml", "sources:\n  - name: s\n    tables:\n      - name: t\n"),
            ("dbt_packages/q/dbt_project.yml", "name: q\n"),
            ("dbt_packages/q/models/s.yml", "sources:\n  - name: s\n    tables:\n      - name: t\n"),
        ];
        let (dir, project) = scratch_project("two-packages", &files);
        let t = Target::Table { source: "s".into(), table: "t".into() };
        assert!(rename(&project, &t, "u", Encoding::Utf16, &uri).unwrap_err().contains("several packages"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_source_of_an_installed_package_even_overridden() {
        let files = [
            ("dbt_project.yml", PROJECT),
            ("models/s.yml", "sources:\n  - name: s\n    overrides: q\n    tables:\n      - name: t\n"),
            ("models/m.sql", "select * from {{ source('s', 't') }}"),
            ("dbt_packages/q/dbt_project.yml", "name: q\n"),
            ("dbt_packages/q/models/s.yml", "sources:\n  - name: s\n    tables:\n      - name: t\n"),
        ];
        let (dir, project) = scratch_project("installed-package", &files);
        let t = Target::Table { source: "s".into(), table: "t".into() };
        assert!(rename(&project, &t, "u", Encoding::Utf16, &uri).unwrap_err().contains("installed package q"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_fixture_s_sources_rename_cleanly() {
        let project = project::scan(&fixture(), &HashMap::new()).unwrap();
        let views = Target::Table { source: "events".into(), table: "page_views".into() };
        let texts = apply(&project, &rename(&project, &views, "visits", Encoding::Utf16, &uri).unwrap());
        assert_eq!(texts["stg_page_views.sql"], "select * from {{ source('events', 'visits') }}\n");
        assert!(texts["_sources.yml"].contains("      - name: \"visits\"\n        identifier: page_views\n"));
        // customers has an identifier already.
        let customers = Target::Table { source: "raw".into(), table: "customers".into() };
        let texts = apply(&project, &rename(&project, &customers, "clients", Encoding::Utf16, &uri).unwrap());
        assert!(texts["_sources.yml"].contains("      - name: clients\n        identifier: raw_customers\n"));
    }
}
