// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The dependency graph of a project: which resource calls which.
//!
//! The edges between nodes come from the index of the last `dbt parse`
//! once there is one, since only dbt resolves a `ref(var('x'))`; before,
//! from the scan of the files, each call looked up by `navigation::Lookup`.
//! The index's edges to macros are transitive, so the edges to and from
//! macros always come from the scan.

use crate::config::ParseIndex;
use crate::navigation::Lookup;
use crate::project::{self, Entity, Project};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;

/// The resource types that `dbt list` lists, `*` and `source:*`: the
/// nodes whose dependencies the index gives.
const LISTED: [&str; 6] = ["model", "seed", "snapshot", "analysis", "test", "source"];

pub struct Graph {
    /// Each item without its calls, which the edges hold: no second copy of
    /// them.
    pub items: HashMap<String, Entity>,
    /// FROM depends on TO, keyed (FROM, TO); the value holds its calls of TO,
    /// in the file of FROM.
    pub edges: BTreeMap<(String, String), Vec<Range<usize>>>,
}

impl Graph {
    /// The graph that the files of PROJECT give, each call looked up with
    /// INDEX, the last parse's, if any: an edge only to what a file holds,
    /// since an edge's ends are items.
    pub fn scanned(project: &Project, index: Option<&ParseIndex>) -> Graph {
        let lookup = Lookup::new(project, index);
        let mut edges: BTreeMap<(String, String), Vec<Range<usize>>> = BTreeMap::new();
        for entity in &project.entities {
            for call in &entity.calls {
                for to in lookup.call(&entity.package, &call.name).iter().filter(|to| to.id != entity.id) {
                    edges.entry((entity.id.clone(), to.id.clone())).or_default().push(call.range.clone());
                }
            }
        }
        let item = |entity: &Entity| (entity.id.clone(), Entity { calls: Vec::new(), ..entity.clone() });
        let items: HashMap<String, Entity> = project.entities.iter().map(item).collect();
        edges.retain(|(_, to), _| items.contains_key(to));
        Graph { items, edges }
    }

    /// This graph with the dependencies that INDEX gives each node of the
    /// kinds of `LISTED` instead of the scanned ones; the scanned call sites
    /// stay.  The index holds no macro, so the scanned edges to and from
    /// macros stay, and so do those of a node it lacks.  A scanned edge to a
    /// function stays too, as one to a macro.  A node the scan missed is
    /// placed at the start of its file in PROJECT.
    pub fn with_index(mut self, index: &ParseIndex, project: &Project) -> Graph {
        let mut scanned = std::mem::take(&mut self.edges);
        let root = project.packages.get(&project.root_package);
        let mut listed = HashSet::new();
        for (node, parents) in index
            .dependencies()
            .filter(|(node, _)| LISTED.contains(&node["resource_type"].as_str().unwrap_or_default()))
        {
            let text = |key: &str| node[key].as_str().unwrap_or_default().to_owned();
            let (id, package) = (text("unique_id"), text("package_name"));
            for to in parents {
                let sites = scanned.remove(&(id.clone(), to.to_owned())).unwrap_or_default();
                self.edges.insert((id.clone(), to.to_owned()), sites);
            }
            let path =
                project.packages.get(&package).or(root).unwrap_or(&PathBuf::new()).join(text("original_file_path"));
            // A model's version, which the manifest names as its model, is shown as its id ends: `orders.v2`.
            let version = id.strip_prefix(&project::version_id(&package, &text("name"), "")).map(str::to_owned);
            let resource_type = project::ResourceType::named(&text("resource_type")).name;
            let entity = Entity {
                id: id.clone(),
                name: text("name"),
                version,
                resource_type,
                package,
                path,
                ..Entity::default()
            };
            self.items.entry(id.clone()).or_insert(entity);
            listed.insert(id);
        }
        for ((from, to), sites) in scanned {
            if matches!(to.split('.').next(), Some("macro" | "function")) || !listed.contains(&from) {
                self.edges.insert((from, to), sites);
            }
        }
        let items = &self.items;
        self.edges.retain(|(from, to), _| items.contains_key(from) && items.contains_key(to));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project;
    use serde_json::json;
    use std::fs;
    use std::path::Path;

    fn shop() -> Project {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        project::scan(&root, &HashMap::new()).unwrap()
    }

    fn pairs(graph: &Graph) -> Vec<(&str, &str)> {
        graph.edges.keys().map(|(from, to)| (from.as_str(), to.as_str())).collect()
    }

    /// The index of a manifest whose NODES are each a unique id, its
    /// `original_file_path` and the ids it depends on, of the package `shop`.
    fn index(nodes: &[(&str, &str, &[&str])]) -> ParseIndex {
        let node = |(id, path, depends_on): &(&str, &str, &[&str])| {
            let parts: Vec<&str> = id.split('.').collect();
            let node = json!({"unique_id": id, "name": parts[2], "resource_type": parts[0], "package_name": "shop",
                              "original_file_path": path, "depends_on": {"nodes": depends_on}});
            (id.to_string(), node)
        };
        let nodes: serde_json::Map<String, serde_json::Value> = nodes.iter().map(node).collect();
        ParseIndex::of(&json!({"metadata": {"project_name": "shop"}, "nodes": nodes}))
    }

    #[test]
    fn scanned_edges_follow_refs_sources_and_macros_across_packages() {
        let project = shop();
        let graph = Graph::scanned(&project, None);
        let edges = pairs(&graph);
        for expected in [
            ("model.shop.orders", "model.shop.stg_orders"),
            ("model.shop.orders", "macro.shop_utils.is_positive"),
            ("model.shop.stg_orders", "macro.shop.cents_to_dollars"),
            ("model.shop.stg_orders", "seed.shop.raw_orders"),
            ("model.shop.stg_customers", "source.shop.raw.customers"),
            ("snapshot.shop.orders_snapshot", "model.shop.stg_orders"),
            ("macro.shop.safe_add", "macro.shop.default__safe_add"),
            ("macro.shop.safe_add", "macro.shop.duckdb__safe_add"),
        ] {
            assert!(edges.contains(&expected), "{expected:?} missing from {edges:?}");
        }
        // The commented ref('raw_customers') of stg_customers is no edge; the dynamic ref is unknown.
        assert!(!edges.contains(&("model.shop.stg_customers", "seed.shop.raw_customers")));
        assert!(!edges.iter().any(|(from, _)| *from == "model.shop.order_summary"));
        // The calls are the edges' alone: no item keeps a copy.
        assert!(project.entities.iter().any(|entity| !entity.calls.is_empty()));
        assert!(graph.items.values().all(|item| item.calls.is_empty()));
    }

    #[test]
    fn an_unqualified_macro_is_looked_up_in_its_package_and_the_root_only() {
        let dir = project::scratch_project(
            "unqualified-macro",
            &[
                ("dbt_project.yml", "name: shop\n"),
                ("models/report.sql", "{{ date_spine() }} {{ utils.date_spine() }}"),
                ("dbt_packages/utils/dbt_project.yml", "name: utils\n"),
                ("dbt_packages/utils/macros/spine.sql", "{% macro date_spine() %}{{ helper() }}{% endmacro %}"),
                ("dbt_packages/utils/macros/helper.sql", "{% macro helper() %}1{% endmacro %}"),
            ],
        );
        let graph = Graph::scanned(&project::scan(&dir, &HashMap::new()).unwrap(), None);
        assert!(pairs(&graph).contains(&("macro.utils.date_spine", "macro.utils.helper")));
        // The unqualified date_spine() is dbt's own, not the package's: one site only.
        let ((from, _), sites) = graph.edges.iter().find(|((_, to), _)| to == "macro.utils.date_spine").unwrap();
        assert_eq!((from.as_str(), sites.len()), ("model.shop.report", 1));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_ref_follows_the_lookup_flag_and_an_ambiguous_one_names_every_candidate() {
        let files = |flags: &str| {
            [
                ("dbt_project.yml".to_owned(), format!("name: shop\n{flags}")),
                ("models/orders.sql".into(), "select 1".into()),
                ("models/report.sql".into(), "{{ ref('twice') }}".into()),
                ("dbt_packages/pkg/dbt_project.yml".into(), "name: pkg\n".into()),
                ("dbt_packages/pkg/models/orders.sql".into(), "select 2".into()),
                ("dbt_packages/pkg/models/uses.sql".into(), "{{ ref('orders') }}".into()),
                ("dbt_packages/pkg/models/twice.sql".into(), "select 3".into()),
                ("dbt_packages/other/dbt_project.yml".into(), "name: other\n".into()),
                ("dbt_packages/other/models/twice.sql".into(), "select 4".into()),
            ]
        };
        let graph = |name: &str, flags: &str| {
            let files = files(flags);
            let borrowed: Vec<(&str, &str)> = files.iter().map(|(path, text)| (path.as_str(), text.as_str())).collect();
            let dir = project::scratch_project(name, &borrowed);
            let graph = Graph::scanned(&project::scan(&dir, &HashMap::new()).unwrap(), None);
            fs::remove_dir_all(&dir).unwrap();
            graph
        };
        let own = graph("graph-flag-own", "");
        assert!(pairs(&own).contains(&("model.pkg.uses", "model.pkg.orders")));
        let flag = "flags:\n  require_ref_searches_node_package_before_root: false\n";
        let root_first = graph("graph-flag-root", flag);
        assert!(pairs(&root_first).contains(&("model.pkg.uses", "model.shop.orders")));
        // dbt fails on a name that two packages but the caller's and the root define (dbt1005).
        let to: Vec<&str> =
            pairs(&own).into_iter().filter(|(from, _)| *from == "model.shop.report").map(|(_, to)| to).collect();
        assert_eq!(to, ["model.other.twice", "model.pkg.twice"]);
    }

    #[test]
    fn an_edge_to_a_function_stays_whether_the_index_names_it_or_not() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("functions/f.sql", "x * 2"),
            ("models/a.sql", "select {{ function('f') }}(1)"),
        ];
        let root = project::scratch_project("graph-function", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        for depends_on in [&[][..], &["function.shop.f"]] {
            let index = index(&[("model.shop.a", "models/a.sql", depends_on)]);
            let graph = Graph::scanned(&project, None).with_index(&index, &project);
            let edge = graph.edges.get(&("model.shop.a".to_owned(), "function.shop.f".to_owned()));
            let sites: Vec<&str> = edge
                .into_iter()
                .flatten()
                .map(|site| &project.text(&root.join("models/a.sql")).unwrap()[site.clone()])
                .collect();
            assert_eq!(sites, ["function('f')"], "{depends_on:?}: once, at its call");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_index_s_edges_win_for_its_nodes_and_keep_the_scanned_call_sites() {
        let project = shop();
        let index = index(&[
            ("model.shop.order_summary", "models/marts/order_summary.sql", &["model.shop.orders"]),
            ("model.shop.orders", "models/marts/orders.sql", &["model.shop.stg_orders"]),
            ("test.shop.not_null_orders_id.abc", "models/marts/_marts.yml", &["model.shop.orders"]),
            // A hook, of a kind that `dbt list` does not list.
            ("operation.shop.shop-on-run-start-0", "./dbt_project.yml", &["model.shop.orders"]),
        ]);
        let graph = Graph::scanned(&project, None).with_index(&index, &project);
        let edges = pairs(&graph);
        let hook = "operation.shop.shop-on-run-start-0";
        assert!(!graph.items.contains_key(hook) && !edges.iter().any(|(from, _)| *from == hook), "no hook");
        assert!(edges.contains(&("model.shop.order_summary", "model.shop.orders")));
        assert!(edges.contains(&("test.shop.not_null_orders_id.abc", "model.shop.orders")));
        assert!(edges.contains(&("model.shop.orders", "macro.shop_utils.is_positive")));
        // The index does not give orders -> stg_customers: it is no edge any more.
        assert!(!edges.contains(&("model.shop.orders", "model.shop.stg_customers")));
        // stg_orders is not in the index: its scanned dependencies stay.
        assert!(edges.contains(&("model.shop.stg_orders", "seed.shop.raw_orders")));
        let stg = &graph.edges[&("model.shop.orders".to_owned(), "model.shop.stg_orders".to_owned())];
        let text = project.text(&graph.items["model.shop.orders"].path).unwrap();
        assert_eq!(&text[stg[0].clone()], "ref('stg_orders'");
        assert!(graph.items["test.shop.not_null_orders_id.abc"].path.ends_with("models/marts/_marts.yml"));
        // An item the index gives too keeps no calls, which its edges hold.
        let scanned = project.entities.iter().find(|entity| entity.id == "model.shop.orders").unwrap();
        assert!(!scanned.calls.is_empty() && graph.items["model.shop.orders"].calls.is_empty());
    }

    #[test]
    fn a_node_the_scan_missed_is_placed_at_its_file_s_start_as_its_version() {
        let project = shop();
        let index = ParseIndex::of(&json!({"metadata": {"project_name": "shop"}, "nodes": {"model.shop.orders.v2": {
            "unique_id": "model.shop.orders.v2", "name": "orders", "resource_type": "model", "package_name": "shop",
            "original_file_path": "models/marts/orders_v2.sql", "depends_on": {"nodes": ["model.shop.stg_orders"]}}}}));
        let graph = Graph::scanned(&project, None).with_index(&index, &project);
        let item = &graph.items["model.shop.orders.v2"];
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop/models/marts/orders_v2.sql");
        let placed = (item.shown(), &item.path, &item.range, &item.name_range, item.calls.is_empty());
        assert_eq!(placed, ("orders.v2".to_owned(), &file, &(0..0), &(0..0), true));
        assert!(pairs(&graph).contains(&("model.shop.orders.v2", "model.shop.stg_orders")));
    }

    #[test]
    fn the_index_keeps_the_scanned_edges_of_macros() {
        let dir = project::scratch_project(
            "macro-edges",
            &[
                ("dbt_project.yml", "name: shop\n"),
                ("models/_sources.yml", "sources:\n  - name: raw\n    tables:\n      - name: events\n"),
                ("models/dates.sql", "select 1"),
                ("models/report.sql", "{{ calendar() }}"),
                (
                    "macros/calendar.sql",
                    "{% macro calendar() %}{{ ref('dates') }} {{ source('raw', 'events') }}{% endmacro %}",
                ),
            ],
        );
        let project = project::scan(&dir, &HashMap::new()).unwrap();
        let index =
            index(&[("model.shop.report", "models/report.sql", &[]), ("model.shop.dates", "models/dates.sql", &[])]);
        let graph = Graph::scanned(&project, None).with_index(&index, &project);
        let edges = pairs(&graph);
        for expected in [
            ("macro.shop.calendar", "model.shop.dates"),
            ("macro.shop.calendar", "source.shop.raw.events"),
            ("model.shop.report", "macro.shop.calendar"),
        ] {
            assert!(edges.contains(&expected), "{expected:?} missing from {edges:?}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn once_a_parse_has_run_a_call_leads_where_its_definition_does_and_keeps_its_site() {
        const REPORT: &str =
            "select * from {{ ref('customers') }} join {{ ref('ghost') }} join {{ source('raw', 'payments') }}";
        const SOURCES: &str = "version: 2\nsources:\n  - name: raw\n    tables:\n      - name: payments\n";
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/customers.sql", "select 1"),
            ("models/report.sql", REPORT),
            ("models/_sources.yml", SOURCES),
            ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
            ("dbt_packages/pkg/models/customers.sql", "select 2"),
            ("dbt_packages/pkg/models/_sources.yml", SOURCES),
        ];
        let root = project::scratch_project("graph-parsed", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        // The root disables its own `customers` and `raw.payments`, which `pkg` has too; no file holds `ghost`.
        let model = |id: &str, file: &str, depends_on: &[&str]| {
            let parts: Vec<&str> = id.split('.').collect();
            json!({"unique_id": id, "name": parts[2], "resource_type": "model", "package_name": parts[1],
                   "original_file_path": file, "depends_on": {"nodes": depends_on}})
        };
        let payments = |package: &str| {
            json!({"unique_id": format!("source.{package}.raw.payments"), "name": "payments", "source_name": "raw",
                   "resource_type": "source", "package_name": package, "original_file_path": "models/_sources.yml"})
        };
        let dependencies = ["model.pkg.customers", "source.pkg.raw.payments"];
        let manifest = json!({"metadata": {"project_name": "shop"},
            "nodes": {"model.pkg.customers": model("model.pkg.customers", "models/customers.sql", &[]),
                      "model.shop.ghost": model("model.shop.ghost", "models/ghost.sql", &[]),
                      "model.shop.report": model("model.shop.report", "models/report.sql", &dependencies)},
            "sources": {"source.pkg.raw.payments": payments("pkg")},
            "disabled": {"model.shop.customers": [model("model.shop.customers", "models/customers.sql", &[])],
                         "source.shop.raw.payments": [payments("shop")]}});
        let index = ParseIndex::of(&manifest);
        let site = |call: &str| {
            let start = REPORT.find(call).unwrap();
            start..start + call.len()
        };
        let (customers, source) = (site("ref('customers'"), site("source('raw', 'payments')"));
        // The edges from `report` to a customers model and a payments source, at SITES, if any.
        let edges = |model: &str, table: &str, sites: bool| {
            let edge = |to: &str, at: &Range<usize>| {
                (("model.shop.report".to_owned(), to.to_owned()), if sites { vec![at.clone()] } else { Vec::new() })
            };
            BTreeMap::from([edge(model, &customers), edge(table, &source)])
        };
        let scanned = Graph::scanned(&project, None);
        assert_eq!(scanned.edges, edges("model.shop.customers", "source.shop.raw.payments", true), "before a parse");
        let parsed = Graph::scanned(&project, Some(&index));
        let dbt_s = edges("model.pkg.customers", "source.pkg.raw.payments", true);
        assert_eq!(parsed.edges, dbt_s, "dbt's, and none to ghost");
        assert_eq!(parsed.with_index(&index, &project).edges, dbt_s);
        let unsited = edges("model.pkg.customers", "source.pkg.raw.payments", false);
        assert_eq!(scanned.with_index(&index, &project).edges, unsited, "no site");
        fs::remove_dir_all(&root).unwrap();
    }
}
