// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The dependency graph of a project: which resource calls which.
//!
//! The edges between nodes come from `dbt.listNodes` when the server has
//! answered, or, with no server, from the manifest of the user's `dbt
//! parse`, since only dbt resolves a `ref(var('x'))`; otherwise from the
//! scan of the files.  The server lists no macros, and a manifest's are
//! transitive, so the edges to and from macros always come from the scan.

use crate::navigation::Lookup;
use crate::project::Project;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;

/// A node of a `dbt.listNodes` answer.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerNode {
    pub id: String,
    pub name: String,
    pub resource_type: String,
    pub package: String,
    /// Relative to the root of its package.
    pub original_file_path: String,
    pub depends_on: Vec<String>,
}

/// What a `dbt.listNodes` answer says.
#[derive(Debug, PartialEq)]
pub enum Listing {
    Nodes(Vec<ServerNode>),
    /// No node matches: `"error": "No nodes found"`, or no error and no node.
    Empty,
    /// Any other error, such as a failed compilation or a cancellation, or
    /// an answer of another shape.
    Failed,
}

impl Listing {
    /// The nodes listed, none when no node matches; `None` when the listing failed.
    pub fn into_nodes(self) -> Option<Vec<ServerNode>> {
        match self {
            Listing::Nodes(nodes) => Some(nodes),
            Listing::Empty => Some(Vec::new()),
            Listing::Failed => None,
        }
    }
}

impl ServerNode {
    /// The node of NODE, as `dbt.listNodes` and a manifest write one; none
    /// when it lacks a field.
    pub fn of(node: &Value) -> Option<ServerNode> {
        let text = |key: &str| node.get(key).and_then(Value::as_str).map(str::to_owned);
        let depends_on = node
            .pointer("/depends_on/nodes")
            .and_then(Value::as_array)
            .map_or(Vec::new(), |ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect());
        Some(ServerNode {
            id: text("unique_id")?,
            name: text("name")?,
            resource_type: text("resource_type")?,
            package: text("package_name")?,
            original_file_path: text("original_file_path")?,
            depends_on,
        })
    }
}

/// The listing of a `dbt.listNodes` RESULT.  Nodes missing a field are left out.
pub fn parse_list_nodes(result: &Value) -> Listing {
    let Some(nodes) = result.get("nodes").and_then(Value::as_array) else { return Listing::Failed };
    let nodes: Vec<ServerNode> = nodes.iter().filter_map(ServerNode::of).collect();
    match (nodes.is_empty(), result.get("error").and_then(Value::as_str)) {
        (false, _) => Listing::Nodes(nodes),
        (true, None | Some("No nodes found")) => Listing::Empty,
        (true, Some(_)) => Listing::Failed,
    }
}

/// A node of the graph, placed in its file.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub resource_type: String,
    pub path: PathBuf,
    pub range: Range<usize>,
    pub name_range: Range<usize>,
}

/// FROM depends on TO; SITES are its calls of TO, in the file of FROM.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub sites: Vec<Range<usize>>,
}

pub struct Graph {
    pub items: HashMap<String, Item>,
    pub edges: Vec<Edge>,
}

impl Graph {
    /// The graph that the files of PROJECT give.
    pub fn scanned(project: &Project) -> Graph {
        let lookup = Lookup::new(project, None);
        let mut sites: BTreeMap<(String, String), Vec<Range<usize>>> = BTreeMap::new();
        for entity in &project.entities {
            for call in &entity.calls {
                for to in lookup.call(&entity.package, &call.name).iter().filter(|to| to.id != entity.id) {
                    sites.entry((entity.id.clone(), to.id.clone())).or_default().push(call.range.clone());
                }
            }
        }
        let items = project.entities.iter().map(|e| {
            let item = Item {
                id: e.id.clone(),
                name: e.shown(),
                resource_type: e.resource_type.to_owned(),
                path: e.path.clone(),
                range: e.range.clone(),
                name_range: e.name_range.clone(),
            };
            (e.id.clone(), item)
        });
        let edges = sites.into_iter().map(|((from, to), sites)| Edge { from, to, sites }).collect();
        Graph { items: items.collect(), edges }
    }

    /// This graph with the dependencies of each node listed in NODES as the
    /// server gives them, instead of the scanned ones; the scanned call sites
    /// stay.  The server lists no macro, so the scanned edges to and from
    /// macros stay, and so do those of a node it did not list.  No probe
    /// shows whether it names functions among a node's dependencies, so a
    /// scanned edge to a function stays too, as one to a macro.  A node the
    /// scan missed is placed at the start of its file in PROJECT.
    pub fn with_server(mut self, nodes: &[ServerNode], project: &Project) -> Graph {
        let listed: HashSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
        let mut scanned: HashMap<(String, String), Vec<Range<usize>>> =
            self.edges.drain(..).map(|edge| ((edge.from, edge.to), edge.sites)).collect();
        let mut edges = Vec::new();
        for node in nodes {
            for to in &node.depends_on {
                let sites = scanned.remove(&(node.id.clone(), to.clone())).unwrap_or_default();
                edges.push(Edge { from: node.id.clone(), to: to.clone(), sites });
            }
            let package_dir =
                project.packages.get(&node.package).or_else(|| project.packages.get(&project.root_package));
            let path = package_dir.map_or_else(PathBuf::new, |dir| dir.join(&node.original_file_path));
            let resource_type = node.resource_type.clone();
            // A model's version, which the listing names as its model, is shown as its id ends: `orders.v2`.
            let own = node
                .id
                .strip_prefix(&crate::node::id(&crate::node::MODEL, &node.package, ""))
                .filter(|_| resource_type == "model");
            let version = own.filter(|shown| shown.starts_with(&format!("{}.v", node.name)));
            let item = Item {
                id: node.id.clone(),
                name: version.map_or_else(|| node.name.clone(), str::to_owned),
                resource_type,
                path,
                range: 0..0,
                name_range: 0..0,
            };
            self.items.entry(node.id.clone()).or_insert(item);
        }
        let own = |to: &str| to.starts_with("macro.") || to.starts_with("function.");
        let kept = scanned.into_iter().filter(|((from, to), _)| own(to) || !listed.contains(from.as_str()));
        edges.extend(kept.map(|((from, to), sites)| Edge { from, to, sites }));
        edges.retain(|edge| self.items.contains_key(&edge.from) && self.items.contains_key(&edge.to));
        edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
        self.edges = edges;
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
        graph.edges.iter().map(|e| (e.from.as_str(), e.to.as_str())).collect()
    }

    #[test]
    fn scanned_edges_follow_refs_sources_and_macros_across_packages() {
        let graph = Graph::scanned(&shop());
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
        let graph = Graph::scanned(&project::scan(&dir, &HashMap::new()).unwrap());
        assert!(pairs(&graph).contains(&("macro.utils.date_spine", "macro.utils.helper")));
        // The unqualified date_spine() is dbt's own, not the package's: one site only.
        let spine = graph.edges.iter().find(|e| e.to == "macro.utils.date_spine").unwrap();
        assert_eq!((spine.from.as_str(), spine.sites.len()), ("model.shop.report", 1));
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
            let graph = Graph::scanned(&project::scan(&dir, &HashMap::new()).unwrap());
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
    fn an_edge_to_a_function_stays_whether_the_listing_names_it_or_not() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("functions/f.sql", "x * 2"),
            ("models/a.sql", "select {{ function('f') }}(1)"),
        ];
        let root = project::scratch_project("graph-function", &files);
        let project = project::scan(&root, &HashMap::new()).unwrap();
        let listing = |depends_on: &[&str]| {
            let answer = json!({"error": null, "nodes": [
                {"unique_id": "model.shop.a", "name": "a", "resource_type": "model", "package_name": "shop",
                 "original_file_path": "models/a.sql", "depends_on": {"nodes": depends_on}}]});
            let Listing::Nodes(nodes) = parse_list_nodes(&answer) else { panic!("no nodes") };
            nodes
        };
        for depends_on in [&[][..], &["function.shop.f"]] {
            let graph = Graph::scanned(&project).with_server(&listing(depends_on), &project);
            let edge = graph.edges.iter().filter(|e| e.from == "model.shop.a" && e.to == "function.shop.f");
            let sites: Vec<&str> = edge
                .flat_map(|e| &e.sites)
                .map(|site| &project.text(&root.join("models/a.sql")).unwrap()[site.clone()])
                .collect();
            assert_eq!(sites, ["function('f')"], "{depends_on:?}: once, at its call");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn server_edges_win_for_nodes_and_keep_the_scanned_call_sites() {
        let project = shop();
        let answer = json!({"error": null, "nodes": [
            {"unique_id": "model.shop.order_summary", "name": "order_summary", "resource_type": "model",
             "package_name": "shop", "original_file_path": "models/marts/order_summary.sql",
             "depends_on": {"nodes": ["model.shop.orders"]}},
            {"unique_id": "model.shop.orders", "name": "orders", "resource_type": "model",
             "package_name": "shop", "original_file_path": "models/marts/orders.sql",
             "depends_on": {"nodes": ["model.shop.stg_orders"]}},
            {"unique_id": "test.shop.not_null_orders_id.abc", "name": "not_null_orders_id", "resource_type": "test",
             "package_name": "shop", "original_file_path": "models/marts/_marts.yml",
             "depends_on": {"nodes": ["model.shop.orders"]}},
            {"unique_id": "model.shop.z.v3", "name": "z", "resource_type": "model",
             "package_name": "shop", "original_file_path": "models/z_v3.sql", "depends_on": {"nodes": []}},
            {"name": "no unique_id"}
        ]});
        let Listing::Nodes(nodes) = parse_list_nodes(&answer) else { panic!("no nodes") };
        assert_eq!(nodes.len(), 4);
        let graph = Graph::scanned(&project).with_server(&nodes, &project);
        let edges = pairs(&graph);
        assert!(edges.contains(&("model.shop.order_summary", "model.shop.orders")));
        assert!(edges.contains(&("test.shop.not_null_orders_id.abc", "model.shop.orders")));
        assert!(edges.contains(&("model.shop.orders", "macro.shop_utils.is_positive")));
        // The server did not list orders -> stg_customers: it is no edge any more.
        assert!(!edges.contains(&("model.shop.orders", "model.shop.stg_customers")));
        // stg_orders was not listed: its scanned dependencies stay.
        assert!(edges.contains(&("model.shop.stg_orders", "seed.shop.raw_orders")));
        let stg =
            graph.edges.iter().find(|e| e.from == "model.shop.orders" && e.to == "model.shop.stg_orders").unwrap();
        let text = project.text(&graph.items["model.shop.orders"].path).unwrap();
        assert_eq!(&text[stg.sites[0].clone()], "ref('stg_orders'");
        assert!(graph.items["test.shop.not_null_orders_id.abc"].path.ends_with("models/marts/_marts.yml"));
        assert_eq!(
            graph.items["model.shop.z.v3"].name, "z.v3",
            "a version the scan missed, as dbt's selectors write it"
        );
    }

    #[test]
    fn a_listing_keeps_the_edges_of_macros() {
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
        let nodes: Vec<ServerNode> =
            [("model.shop.report", "models/report.sql"), ("model.shop.dates", "models/dates.sql")]
                .map(|(id, path)| ServerNode {
                    id: id.into(),
                    name: id.rsplit('.').next().unwrap().into(),
                    resource_type: "model".into(),
                    package: "shop".into(),
                    original_file_path: path.into(),
                    depends_on: Vec::new(),
                })
                .to_vec();
        let graph = Graph::scanned(&project).with_server(&nodes, &project);
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
    fn tells_an_empty_listing_from_a_failed_one() {
        assert_eq!(parse_list_nodes(&json!({"error": "No nodes found", "nodes": []})), Listing::Empty);
        assert_eq!(parse_list_nodes(&json!({"error": null, "nodes": []})), Listing::Empty);
        assert_eq!(parse_list_nodes(&json!({"error": "Operation cancelled", "nodes": []})), Listing::Failed);
        assert_eq!(parse_list_nodes(&json!({"error": "Compilation failed", "nodes": []})), Listing::Failed);
        assert_eq!(parse_list_nodes(&Value::Null), Listing::Failed);
    }
}
