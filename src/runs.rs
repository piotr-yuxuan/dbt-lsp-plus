// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The failures of the user's last dbt run, from the `run_results.json` and
//! `manifest.json` it wrote, and the stale sources of its last freshness
//! check, from `sources.json`; and where each is written.  Pure.

use crate::properties::{Entry, Test};
use serde_json::Value;
use std::ops::Range;

/// The code of a failure of the last run.
pub const LAST_RUN: &str = "last-run";
/// The code of a source the last freshness check found stale.
pub const STALE_SOURCE: &str = "stale-source";

/// The commands whose results tell how the project last built: `compile`
/// and `show` write results too, of nothing built.
const BUILDING: [&str; 5] = ["build", "run", "test", "seed", "snapshot"];

/// A failure a result may show: the type of its node (any when empty), its
/// status, its severity, and the verb of its message when not dbt's own.
type Shown = (&'static str, &'static str, u8, Option<&'static str>);

/// The failures shown, as LSP's severities: 1 an error, 2 a warning.
const SHOWN: [Shown; 4] = [
    ("", "error", 1, None),
    ("test", "fail", 1, Some("failed")),
    ("test", "warn", 2, Some("warned")),
    ("unit_test", "fail", 1, None),
];

/// Where a failure is written in its file.
#[derive(Clone, Debug, PartialEq)]
pub enum Locator {
    /// The file's first line.
    File,
    /// In a properties file, under the entries of CHAIN, each a label and a
    /// name, the generic test named TEST, else the deepest entry found.
    Entry { chain: Vec<(&'static str, String)>, test: Option<String> },
}

/// A node that failed in the last run.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub id: String,
    pub package: String,
    /// Its file, relative to its package's directory.
    pub path: String,
    pub locator: Locator,
    /// LSP's: 1 an error, 2 a warning.
    pub severity: u8,
    pub message: String,
    /// `LAST_RUN` or `STALE_SOURCE`.
    pub code: &'static str,
}

/// The failures of RESULTS, placed by MANIFEST: an error of any node, and a
/// test that failed or warned.  None when RESULTS are not a building
/// command's, so that the failures known stay; a result whose node MANIFEST
/// lacks gives none.
pub fn failures(results: &Value, manifest: &Value) -> Option<Vec<Failure>> {
    let command =
        results.pointer("/args/command").or_else(|| results.pointer("/args/which")).and_then(Value::as_str)?;
    if !BUILDING.contains(&command) {
        return None;
    }
    let failure = |result: &Value| {
        let id = result["unique_id"].as_str()?;
        let kinds = ["nodes", "unit_tests", "functions"];
        let node = kinds.iter().find_map(|kind| manifest[kind].get(id))?;
        let resource = node["resource_type"].as_str().unwrap_or_default();
        let shown = |(kind, status, ..): &&Shown| (kind.is_empty() || *kind == resource) && result["status"] == *status;
        let (.., severity, verb) = SHOWN.iter().find(shown)?;
        let name = node.pointer("/test_metadata/name").or(node.get("name")).and_then(Value::as_str).unwrap_or(id);
        let rows = result["failures"].as_u64().unwrap_or_default();
        // dbt says only "Error" of a failed test or unit test.
        let said = result["message"].as_str().filter(|message| *message != "Error").map(|m| m.trim().to_owned());
        let message = verb.map(|verb| format!("{name} {verb}: {rows} rows")).or(said);
        Some(Failure {
            id: id.to_owned(),
            package: node["package_name"].as_str()?.to_owned(),
            path: node["original_file_path"].as_str()?.to_owned(),
            locator: locator(manifest, node, resource),
            severity: *severity,
            // Named after its command, as a run's result is.
            message: format!("dbt {command}: {}", message.unwrap_or_else(|| format!("{name} failed"))),
            code: LAST_RUN,
        })
    };
    Some(results["results"].as_array()?.iter().filter_map(failure).collect())
}

/// The source tables that SOURCES, the results of `dbt source freshness`,
/// find stale, placed by MANIFEST at their `name:`: a warning, an error
/// past `error_after` or when dbt reports its query failed (dbt-core's
/// `runtime error`; dbt 2.0.9 writes no result for such a table); a table
/// MANIFEST lacks gives none.
pub fn stale(sources: &Value, manifest: &Value) -> Vec<Failure> {
    let stale = |result: &Value| {
        let id = result["unique_id"].as_str()?;
        let source = manifest["sources"].get(id)?;
        let status = result["status"].as_str()?.to_lowercase();
        let (severity, criterion, word) = match status.as_str() {
            "pass" => return None,
            "warn" => (2, "warn_after", "warn"),
            _ => (1, "error_after", "error"),
        };
        let name = |key: &str| source[key].as_str().unwrap_or_default().to_owned();
        let table = format!("{}.{}", name("source_name"), name("name"));
        let plural = |count: u64| if count == 1 { "" } else { "s" };
        // The age in days, else hours, else minutes: a source due every few
        // hours is stale long before a day.
        let ago = result["max_loaded_at_time_ago_in_s"].as_f64().map(|seconds| {
            let unit =
                [(86_400.0, "day"), (3_600.0, "hour"), (60.0, "minute")].into_iter().find(|(s, _)| seconds >= *s);
            let (length, unit) = unit.unwrap_or((60.0, "minute"));
            let count = (seconds / length).floor() as u64;
            format!("{count} {unit}{}", plural(count))
        });
        let loaded = result["max_loaded_at"].as_str().zip(ago).map(|(at, ago)| format!(" last loaded {at}, {ago} ago"));
        let criterion = &result["criteria"][criterion];
        let after = criterion["period"].as_str().zip(criterion["count"].as_u64());
        let after = after.map(|(period, count)| format!(" ({word} after {count} {period}{})", plural(count)));
        // A check that failed has neither: its status says so, with dbt's error.
        let failed = || {
            let error =
                result["error"].as_str().or(result["message"].as_str()).map(|error| format!(": {}", error.trim()));
            format!(" {status}{}", error.unwrap_or_default())
        };
        let detail = loaded.map(|loaded| loaded + &after.unwrap_or_default()).unwrap_or_else(failed);
        Some(Failure {
            id: id.to_owned(),
            package: name("package_name"),
            path: name("original_file_path"),
            locator: Locator::Entry {
                chain: vec![("source", name("source_name")), ("table", name("name"))],
                test: None,
            },
            severity,
            message: format!("dbt source freshness: {table}{detail}"),
            code: STALE_SOURCE,
        })
    };
    sources["results"].as_array().map_or(&[][..], Vec::as_slice).iter().filter_map(stale).collect()
}

/// Where NODE, of RESOURCE type, is written in its file: a unit test at its
/// entry; a generic test under its node's entry, a model, seed or snapshot,
/// or a source and its table for a test on a source, then its column's;
/// any other at its file's first line.
fn locator(manifest: &Value, node: &Value, resource: &str) -> Locator {
    let name = |node: &Value, key: &str| node[key].as_str().unwrap_or_default().to_owned();
    if resource == "unit_test" {
        return Locator::Entry { chain: vec![("unit test", name(node, "name"))], test: None };
    }
    let Some(test) = node.pointer("/test_metadata/name").and_then(Value::as_str) else { return Locator::File };
    // A test on a source is attached to nothing: it depends on the source,
    // after the nodes a relationships test refers to.
    let depends = node.pointer("/depends_on/nodes").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let source = || depends.iter().filter_map(Value::as_str).find(|id| id.starts_with("source."));
    let owner = node["attached_node"].as_str().or_else(source);
    let owner = owner.and_then(|id| manifest["nodes"].get(id).or_else(|| manifest["sources"].get(id)));
    let mut chain = match owner {
        Some(owner) if owner["resource_type"] == "source" => {
            vec![("source", name(owner, "source_name")), ("table", name(owner, "name"))]
        }
        Some(owner) => {
            let label = ["model", "seed", "snapshot"].into_iter().find(|label| owner["resource_type"] == *label);
            label.map(|label| vec![(label, name(owner, "name"))]).unwrap_or_default()
        }
        None => Vec::new(),
    };
    // The adapter quotes a column written with `quote: true`.
    if let Some(column) = node["column_name"].as_str() {
        chain.push(("column", column.trim_matches(['"', '`']).to_owned()));
    }
    Locator::Entry { chain, test: Some(test.to_owned()) }
}

/// The range of LOCATOR in TEXT, a properties file's ENTRIES and TESTS read
/// once for all its locators: the first line; or the test named under the
/// deepest entry of the chain found, a test of that entry itself and not of
/// one it holds, else that entry's name, else the first line.  Of entries
/// of one name, a versioned model's columns, the one holding the test.
pub fn locate(text: &str, entries: &[Entry], tests: &[Test], locator: &Locator) -> Range<usize> {
    let first_line = 0..text.find(['\n', '\r']).unwrap_or(text.len());
    let Locator::Entry { chain, test } = locator else { return first_line };
    let inside = |outer: &Range<usize>, inner: &Range<usize>| outer.start <= inner.start && inner.end <= outer.end;
    let holds =
        |entry: &&&Entry| tests.iter().any(|t| Some(&t.name) == test.as_ref() && inside(&entry.range, &t.range));
    let mut within = 0..text.len();
    let mut deepest = None;
    for (label, name) in chain {
        let named = entries.iter().filter(|e| e.label == *label && e.name == *name && inside(&within, &e.range));
        let named: Vec<&Entry> = named.collect();
        let Some(entry) = named.iter().find(holds).or(named.first()) else { break };
        within = entry.range.clone();
        deepest = Some(entry.name_range.clone());
    }
    let held = |range: &Range<usize>| {
        entries.iter().any(|e| e.range != within && inside(&within, &e.range) && inside(&e.range, range))
    };
    let own = tests.iter().find(|t| Some(&t.name) == test.as_ref() && inside(&within, &t.range) && !held(&t.range));
    own.map(|test| test.range.clone()).or(deepest).unwrap_or(first_line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A manifest of the nodes the recorded 2.0.9 builds failed on, as dbt
    /// writes them.
    fn manifest() -> Value {
        let generic = |id: &str, path: &str, attached: Value, depends: &str, column: &str, test: &str| {
            json!({"unique_id": id, "resource_type": "test", "package_name": "shop", "original_file_path": path,
                   "attached_node": attached, "depends_on": {"nodes": [depends]}, "column_name": column,
                   "test_metadata": {"name": test}, "name": id.split('.').nth(2).unwrap()})
        };
        json!({"nodes": {
            "model.shop.broken": {"unique_id": "model.shop.broken", "resource_type": "model", "name": "broken",
                                  "package_name": "shop", "original_file_path": "models/marts/broken.sql"},
            "model.shop_utils.pkg_broken": {"unique_id": "model.shop_utils.pkg_broken", "resource_type": "model",
                                            "name": "pkg_broken", "package_name": "shop_utils",
                                            "original_file_path": "models/pkg_broken.sql"},
            "model.shop.orders": {"resource_type": "model", "name": "orders", "package_name": "shop"},
            "model.shop.vmod.v2": {"resource_type": "model", "name": "vmod", "version": 2, "package_name": "shop"},
            "test.shop.always_fails": {"unique_id": "test.shop.always_fails", "resource_type": "test",
                                       "name": "always_fails", "package_name": "shop",
                                       "original_file_path": "tests/sub/always_fails.sql", "test_metadata": null},
            "test.shop.is_even_orders_order_id.a1": generic("test.shop.is_even_orders_order_id.a1",
                "models/marts/_models.yml", json!("model.shop.orders"), "model.shop.orders", "order_id", "is_even"),
            "test.shop.is_even_vmod_v2_id.ff": generic("test.shop.is_even_vmod_v2_id.ff",
                "models/versioned/_vmod.yml", json!("model.shop.vmod.v2"), "model.shop.vmod.v2", "id", "is_even"),
            "test.shop.source_is_even_raw_orders_id.a4": generic("test.shop.source_is_even_raw_orders_id.a4",
                "models/staging/_sources.yml", Value::Null, "source.shop.raw.orders", "id", "is_even")},
            "sources": {"source.shop.raw.orders": {"resource_type": "source", "source_name": "raw", "name": "orders"}},
            "unit_tests": {"unit_test.shop.stg_orders.ut": {"unique_id": "unit_test.shop.stg_orders.ut",
                "resource_type": "unit_test", "name": "ut", "package_name": "shop",
                "original_file_path": "models/staging/_unit.yml"}}})
    }

    fn results(command: &str, results: Value) -> Value {
        json!({"args": {"command": command}, "results": results})
    }

    fn result(id: &str, status: &str, message: &str, failures: Value) -> Value {
        json!({"unique_id": id, "status": status, "message": message, "failures": failures})
    }

    #[test]
    fn a_build_gives_its_errors_and_failing_tests_where_they_are_written() {
        let built = results(
            "build",
            json!([
                result(
                    "model.shop.broken",
                    "error",
                    "Database Error in model broken\n  Catalog Error: nowhere\n",
                    json!(null)
                ),
                result("model.shop.orders", "skipped", "Skipped due to upstream failure", json!(null)),
                result("test.shop.is_even_orders_order_id.a1", "fail", "Error", json!(1)),
                result("test.shop.source_is_even_raw_orders_id.a4", "warn", "Warn", json!(2)),
                result("test.shop.always_fails", "fail", "Error", json!(1)),
                result("unit_test.shop.stg_orders.ut", "error", "Error", json!(null)),
                result("model.shop_utils.pkg_broken", "error", "Database Error in model pkg_broken", json!(null)),
                result("test.shop.gone.ff", "fail", "Error", json!(1)),
                result("test.shop.is_even_vmod_v2_id.ff", "fail", "Error", json!(3))
            ]),
        );
        let found = failures(&built, &manifest()).unwrap();
        let summary: Vec<(&str, &str, &str, u8, &str)> = found
            .iter()
            .map(|f| (f.id.as_str(), f.package.as_str(), f.path.as_str(), f.severity, f.message.as_str()))
            .collect();
        let expected = [
            (
                "model.shop.broken",
                "shop",
                "models/marts/broken.sql",
                1,
                "dbt build: Database Error in model broken\n  Catalog Error: nowhere",
            ),
            (
                "test.shop.is_even_orders_order_id.a1",
                "shop",
                "models/marts/_models.yml",
                1,
                "dbt build: is_even failed: 1 rows",
            ),
            (
                "test.shop.source_is_even_raw_orders_id.a4",
                "shop",
                "models/staging/_sources.yml",
                2,
                "dbt build: is_even warned: 2 rows",
            ),
            (
                "test.shop.always_fails",
                "shop",
                "tests/sub/always_fails.sql",
                1,
                "dbt build: always_fails failed: 1 rows",
            ),
            ("unit_test.shop.stg_orders.ut", "shop", "models/staging/_unit.yml", 1, "dbt build: ut failed"),
            (
                "model.shop_utils.pkg_broken",
                "shop_utils",
                "models/pkg_broken.sql",
                1,
                "dbt build: Database Error in model pkg_broken",
            ),
            (
                "test.shop.is_even_vmod_v2_id.ff",
                "shop",
                "models/versioned/_vmod.yml",
                1,
                "dbt build: is_even failed: 3 rows",
            ),
        ];
        assert_eq!(summary, expected);
        let locators: Vec<&Locator> = found.iter().map(|f| &f.locator).collect();
        let column = |chain: Vec<(&'static str, &str)>| Locator::Entry {
            chain: chain.into_iter().map(|(label, name)| (label, name.to_owned())).collect(),
            test: Some("is_even".into()),
        };
        assert_eq!(locators[0], &Locator::File);
        assert_eq!(locators[1], &column(vec![("model", "orders"), ("column", "order_id")]));
        assert_eq!(locators[2], &column(vec![("source", "raw"), ("table", "orders"), ("column", "id")]));
        assert_eq!(locators[3], &Locator::File);
        assert_eq!(locators[4], &Locator::Entry { chain: vec![("unit test", "ut".into())], test: None });
        // A versioned model's name, not its version's.
        assert_eq!(locators[6], &column(vec![("model", "vmod"), ("column", "id")]));
    }

    #[test]
    fn a_test_is_its_entry_s_own_and_a_version_s_column_counts() {
        let text = "\
models:
  - name: orders
    columns:
      - name: order_id
        data_tests: [unique]
    data_tests:
      - unique:
          column_name: amount * 0
  - name: vmod
    versions:
      - v: 2
        columns:
          - name: id
            data_tests: [not_null]
          - name: amount
            data_tests: [not_null]
";
        let locator = |chain: Vec<(&'static str, &str)>, test: &str| Locator::Entry {
            chain: chain.into_iter().map(|(label, name)| (label, name.to_owned())).collect(),
            test: Some(test.to_owned()),
        };
        let line = |range: Range<usize>| text[..range.start].matches('\n').count();
        // A model's own test, not its column's of the same name.
        assert_eq!(line(located(text, &locator(vec![("model", "orders")], "unique"))), 6);
        // The column of a version, by its name.
        assert_eq!(line(located(text, &locator(vec![("model", "vmod"), ("column", "amount")], "not_null"))), 15);
    }

    #[test]
    fn a_failing_unit_test_a_test_on_a_source_column_and_a_function_show() {
        let mut manifest = manifest();
        // dbt 2.0.9 puts a relationships test's target before the source it tests.
        manifest["nodes"]["test.shop.relationships_raw_orders.b"] = json!({
            "unique_id": "test.shop.relationships_raw_orders.b", "resource_type": "test", "package_name": "shop",
            "original_file_path": "models/staging/_sources.yml", "attached_node": null,
            "depends_on": {"nodes": ["model.shop.orders", "source.shop.raw.orders"]},
            "column_name": "\"Customer Id\"", "test_metadata": {"name": "relationships"}});
        manifest["functions"] = json!({"function.shop.f": {"unique_id": "function.shop.f", "resource_type": "function",
            "name": "f", "package_name": "shop", "original_file_path": "functions/f.sql"}});
        let run = results(
            "build",
            json!([
                result("unit_test.shop.stg_orders.ut", "fail", "Error", json!(null)),
                result("test.shop.relationships_raw_orders.b", "fail", "Error", json!(1)),
                result("function.shop.f", "error", "Compilation Error in function f", json!(null))
            ]),
        );
        let found = failures(&run, &manifest).unwrap();
        assert_eq!((found[0].message.as_str(), found[0].severity), ("dbt build: ut failed", 1));
        let chain =
            vec![("source", "raw".to_owned()), ("table", "orders".to_owned()), ("column", "Customer Id".to_owned())];
        assert_eq!(found[1].locator, Locator::Entry { chain, test: Some("relationships".into()) });
        assert_eq!((found[2].path.as_str(), &found[2].locator), ("functions/f.sql", &Locator::File));
    }

    #[test]
    fn a_freshness_check_gives_its_stale_tables_at_their_names() {
        let mut manifest = manifest();
        let source = |table: &str, package: &str| {
            json!({"unique_id": format!("source.{package}.raw.{table}"),
            "resource_type": "source", "source_name": "raw", "name": table, "package_name": package,
            "original_file_path": "models/staging/_sources.yml"})
        };
        manifest["sources"] = json!({"source.shop.raw.orders": source("orders", "shop"),
            "source.shop.raw.customers": source("customers", "shop"),
            "source.utils.raw.events": source("events", "utils")});
        let result = |table: &str, package: &str, status: &str, warn: Value, error: Value| {
            json!({
            "unique_id": format!("source.{package}.raw.{table}"), "status": status,
            "max_loaded_at": "2018-01-02T00:00:00Z", "max_loaded_at_time_ago_in_s": 279_704_850.0,
            "criteria": {"warn_after": warn, "error_after": error}})
        };
        let unset = json!({"count": null, "period": null});
        // As dbt 2.0.9 writes sources.json: capitalised statuses.
        let checked = json!({"results": [
            result("orders", "shop", "Warn", json!({"count": 1, "period": "day"}), unset.clone()),
            result("customers", "shop", "Error", unset.clone(), json!({"count": 12, "period": "hour"})),
            result("events", "utils", "Pass", unset.clone(), unset.clone()),
            result("gone", "shop", "Error", unset.clone(), unset.clone()),
            // dbt-core's shape of a failed query; 2.0.9 writes no result for it.
            json!({"unique_id": "source.utils.raw.events", "status": "runtime error",
                   "error": "Database Error in source events\n  column \"_loaded_at\" does not exist\n"})]});
        let found = stale(&checked, &manifest);
        let summary: Vec<(&str, u8, &str, &str)> =
            found.iter().map(|f| (f.package.as_str(), f.severity, f.message.as_str(), f.code)).collect();
        let expected = [
            (
                "shop",
                2,
                "dbt source freshness: raw.orders last loaded 2018-01-02T00:00:00Z, 3237 days ago (warn after 1 day)",
                STALE_SOURCE,
            ),
            (
                "shop",
                1,
                "dbt source freshness: raw.customers last loaded 2018-01-02T00:00:00Z, 3237 days ago \
                 (error after 12 hours)",
                STALE_SOURCE,
            ),
            (
                "utils",
                1,
                "dbt source freshness: raw.events runtime error: Database Error in source events\n  \
                 column \"_loaded_at\" does not exist",
                STALE_SOURCE,
            ),
        ];
        assert_eq!(summary, expected);
        let table =
            |name: &str| Locator::Entry { chain: vec![("source", "raw".into()), ("table", name.into())], test: None };
        assert_eq!(found[0].locator, table("orders"));
        assert_eq!(found[0].path, "models/staging/_sources.yml");
    }

    #[test]
    fn a_stale_age_is_told_in_the_unit_it_counts_in() {
        let mut manifest = manifest();
        manifest["sources"] = json!({"source.shop.raw.orders": {"unique_id": "source.shop.raw.orders",
            "source_name": "raw", "name": "orders", "package_name": "shop", "original_file_path": "x.yml"}});
        let check = |seconds: f64| {
            let result = json!({"unique_id": "source.shop.raw.orders", "status": "Warn", "max_loaded_at": "t",
                "max_loaded_at_time_ago_in_s": seconds, "criteria": {"warn_after": {"count": 6, "period": "hour"}}});
            stale(&json!({"results": [result]}), &manifest)[0].message.clone()
        };
        assert_eq!(check(25_200.0), "dbt source freshness: raw.orders last loaded t, 7 hours ago (warn after 6 hours)");
        assert_eq!(check(90.0), "dbt source freshness: raw.orders last loaded t, 1 minute ago (warn after 6 hours)");
        assert_eq!(check(30.0), "dbt source freshness: raw.orders last loaded t, 0 minutes ago (warn after 6 hours)");
        // dbt 2.0.9's file when every check failed: nothing to show.
        assert_eq!(stale(&json!({"results": []}), &manifest), Vec::new());
    }

    #[test]
    fn a_command_that_builds_nothing_says_nothing_of_the_last_build() {
        for command in ["compile", "show", "parse", "freshness"] {
            assert_eq!(failures(&results(command, json!([])), &manifest()), None, "{command}");
        }
        assert_eq!(failures(&json!({"results": []}), &manifest()), None);
        assert_eq!(failures(&results("test", json!([])), &manifest()), Some(Vec::new()));
    }

    /// The range of LOCATOR in TEXT, its entries and tests read here.
    fn located(text: &str, locator: &Locator) -> Range<usize> {
        locate(text, &crate::properties::entries(text), &crate::properties::tests(text), locator)
    }

    #[test]
    fn a_generic_test_is_found_under_its_entry_and_column() {
        let text = "\
models:
  - name: customers
    columns:
      - name: order_id
        data_tests: [is_even]
  - name: orders
    columns:
      - name: order_id
        data_tests:
          - unique
          - is_even
  - name: vmod
    versions:
      - v: 2
        columns:
          - name: id
            data_tests: [is_even]
";
        let locator = |chain: Vec<(&'static str, &str)>, test: Option<&str>| Locator::Entry {
            chain: chain.into_iter().map(|(label, name)| (label, name.to_owned())).collect(),
            test: test.map(str::to_owned),
        };
        let line = |range: Range<usize>| text[..range.start].matches('\n').count();
        let on_orders = located(text, &locator(vec![("model", "orders"), ("column", "order_id")], Some("is_even")));
        assert_eq!((line(on_orders.clone()), &text[on_orders]), (10, "is_even"));
        // A versioned model's column is under `versions:`: the model's entry holds it.
        let on_vmod = located(text, &locator(vec![("model", "vmod"), ("column", "id")], Some("is_even")));
        assert_eq!(line(on_vmod), 16);
        // A test no longer there: the entry's name; no entry: the first line.
        let gone = located(text, &locator(vec![("model", "orders")], Some("accepted_values")));
        assert_eq!((line(gone.clone()), &text[gone]), (5, "orders"));
        assert_eq!(located(text, &locator(vec![("model", "nowhere")], None)), 0..7);
        assert_eq!(located("select 1\nfrom x", &Locator::File), 0..8);
    }
}
