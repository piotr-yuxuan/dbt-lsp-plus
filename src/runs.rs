// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The failures of the user's last dbt run, from the `run_results.json` and
//! `manifest.json` it wrote, and where each is written.  Pure.

use crate::properties;
use serde_json::Value;
use std::ops::Range;

/// The commands whose results tell how the project last built: `compile`
/// and `show` write results too, of nothing built.
const BUILDING: [&str; 5] = ["build", "run", "test", "seed", "snapshot"];

/// A failure a result may show: the type of its node (any when empty), its
/// status, its severity, and the verb of its message when not dbt's own.
type Shown = (&'static str, &'static str, u8, Option<&'static str>);

/// The failures shown, as LSP's severities: 1 an error, 2 a warning.
const SHOWN: [Shown; 3] =
    [("", "error", 1, None), ("test", "fail", 1, Some("failed")), ("test", "warn", 2, Some("warned"))];

/// Where a failure is written in its file.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
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
    pub place: Place,
    /// LSP's: 1 an error, 2 a warning.
    pub severity: u8,
    pub message: String,
}

/// The failures of RESULTS, placed by MANIFEST: an error of any node, and a
/// test that failed or warned.  None when RESULTS are not a building
/// command's, so that the failures known stay; a result whose node MANIFEST
/// lacks gives none.
pub fn failures(results: &Value, manifest: &Value) -> Option<Vec<Failure>> {
    let command = results.pointer("/args/command").and_then(Value::as_str)?;
    if !BUILDING.contains(&command) {
        return None;
    }
    let failure = |result: &Value| {
        let id = result["unique_id"].as_str()?;
        let node = manifest["nodes"].get(id).or_else(|| manifest["unit_tests"].get(id))?;
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
            place: place(manifest, node, resource),
            severity: *severity,
            message: message.unwrap_or_else(|| format!("{name} failed")),
        })
    };
    Some(results["results"].as_array()?.iter().filter_map(failure).collect())
}

/// Where NODE, of RESOURCE type, is written in its file: a unit test at its
/// entry; a generic test under its node's entry, a model, seed or snapshot,
/// or a source and its table for a test on a source, then its column's;
/// any other at its file's first line.
fn place(manifest: &Value, node: &Value, resource: &str) -> Place {
    let name = |node: &Value, key: &str| node[key].as_str().unwrap_or_default().to_owned();
    if resource == "unit_test" {
        return Place::Entry { chain: vec![("unit test", name(node, "name"))], test: None };
    }
    let Some(test) = node.pointer("/test_metadata/name").and_then(Value::as_str) else { return Place::File };
    // A test on a source is attached to nothing: it depends on the source.
    let owner = node["attached_node"].as_str().or_else(|| node.pointer("/depends_on/nodes/0").and_then(Value::as_str));
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
    if let Some(column) = node["column_name"].as_str() {
        chain.push(("column", column.to_owned()));
    }
    Place::Entry { chain, test: Some(test.to_owned()) }
}

/// The range of PLACE in TEXT: the first line; or, in a properties file,
/// the test named under the deepest entry of the chain found, else that
/// entry's name, else the first line.
pub fn locate(text: &str, place: &Place) -> Range<usize> {
    let first_line = 0..text.find(['\n', '\r']).unwrap_or(text.len());
    let Place::Entry { chain, test } = place else { return first_line };
    let entries = properties::entries(text);
    let mut within = 0..text.len();
    let mut deepest = None;
    for (label, name) in chain {
        let inside = |range: &Range<usize>| within.start <= range.start && range.end <= within.end;
        let Some(entry) = entries.iter().find(|e| e.label == *label && e.name == *name && inside(&e.range)) else {
            break;
        };
        within = entry.range.clone();
        deepest = Some(entry.name_range.clone());
    }
    let tests = properties::tests(text);
    let named = test.as_ref().and_then(|test| {
        tests.iter().find(|t| t.name == *test && within.start <= t.range.start && t.range.end <= within.end)
    });
    named.map(|test| test.range.clone()).or(deepest).unwrap_or(first_line)
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
                result("test.shop.is_even_vmod_v2_id.ff", "pass", "Pass", json!(0))
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
                "Database Error in model broken\n  Catalog Error: nowhere",
            ),
            ("test.shop.is_even_orders_order_id.a1", "shop", "models/marts/_models.yml", 1, "is_even failed: 1 rows"),
            (
                "test.shop.source_is_even_raw_orders_id.a4",
                "shop",
                "models/staging/_sources.yml",
                2,
                "is_even warned: 2 rows",
            ),
            ("test.shop.always_fails", "shop", "tests/sub/always_fails.sql", 1, "always_fails failed: 1 rows"),
            ("unit_test.shop.stg_orders.ut", "shop", "models/staging/_unit.yml", 1, "ut failed"),
            (
                "model.shop_utils.pkg_broken",
                "shop_utils",
                "models/pkg_broken.sql",
                1,
                "Database Error in model pkg_broken",
            ),
        ];
        assert_eq!(summary, expected);
        let places: Vec<&Place> = found.iter().map(|f| &f.place).collect();
        let column = |chain: Vec<(&'static str, &str)>| Place::Entry {
            chain: chain.into_iter().map(|(label, name)| (label, name.to_owned())).collect(),
            test: Some("is_even".into()),
        };
        assert_eq!(places[0], &Place::File);
        assert_eq!(places[1], &column(vec![("model", "orders"), ("column", "order_id")]));
        assert_eq!(places[2], &column(vec![("source", "raw"), ("table", "orders"), ("column", "id")]));
        assert_eq!(places[3], &Place::File);
        assert_eq!(places[4], &Place::Entry { chain: vec![("unit test", "ut".into())], test: None });
    }

    #[test]
    fn a_command_that_builds_nothing_says_nothing_of_the_last_build() {
        for command in ["compile", "show", "parse"] {
            assert_eq!(failures(&results(command, json!([])), &manifest()), None, "{command}");
        }
        assert_eq!(failures(&json!({"results": []}), &manifest()), None);
        assert_eq!(failures(&results("test", json!([])), &manifest()), Some(Vec::new()));
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
        let place = |chain: Vec<(&'static str, &str)>, test: Option<&str>| Place::Entry {
            chain: chain.into_iter().map(|(label, name)| (label, name.to_owned())).collect(),
            test: test.map(str::to_owned),
        };
        let line = |range: Range<usize>| text[..range.start].matches('\n').count();
        let on_orders = locate(text, &place(vec![("model", "orders"), ("column", "order_id")], Some("is_even")));
        assert_eq!((line(on_orders.clone()), &text[on_orders]), (10, "is_even"));
        // A versioned model's column is under `versions:`: the model's entry holds it.
        let on_vmod = locate(text, &place(vec![("model", "vmod"), ("column", "id")], Some("is_even")));
        assert_eq!(line(on_vmod), 16);
        // A test no longer there: the entry's name; no entry: the first line.
        let gone = locate(text, &place(vec![("model", "orders")], Some("accepted_values")));
        assert_eq!((line(gone.clone()), &text[gone]), (5, "orders"));
        assert_eq!(locate(text, &place(vec![("model", "nowhere")], None)), 0..7);
        assert_eq!(locate("select 1\nfrom x", &Place::File), 0..8);
    }
}
