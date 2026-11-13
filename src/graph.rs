// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The dependency graph of a project: which resource calls which.
//!
//! The edges between nodes come from `dbt.listNodes` when the server has
//! answered, since only dbt resolves a `ref(var('x'))`; otherwise from the
//! scan of the files.  The server lists no macros, so the edges to and from
//! macros always come from the scan.

use crate::project::{Entity, Project};
use crate::scan::Target;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// The listing of a `dbt.listNodes` RESULT.  Nodes missing a field are left out.
pub fn parse_list_nodes(result: &Value) -> Listing {
    let text = |node: &Value, key: &str| node.get(key).and_then(Value::as_str).map(str::to_owned);
    let Some(nodes) = result.get("nodes").and_then(Value::as_array) else { return Listing::Failed };
    let nodes: Vec<ServerNode> = nodes
        .iter()
        .filter_map(|node| {
            let depends_on = node
                .pointer("/depends_on/nodes")
                .and_then(Value::as_array)
                .map_or(Vec::new(), |ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect());
            Some(ServerNode {
                id: text(node, "unique_id")?,
                name: text(node, "name")?,
                resource_type: text(node, "resource_type")?,
                package: text(node, "package_name")?,
                original_file_path: text(node, "original_file_path")?,
                depends_on,
            })
        })
        .collect();
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

/// The ids of the resources that a call of TARGET from ENTITY names, by
/// dbt's lookup: a package named in the call, else the entity's own package,
/// then the packages of SEARCH, the root project first; an unqualified macro
/// only in the entity's package and the root project, since dbt keeps the
/// others' macros behind their package's name.  A ref, source or macro names
/// the first that exists; a dispatch names every implementation (`*__name`)
/// of the first package that has one.
fn resolve(known: &BTreeSet<&str>, search: &[&str], entity: &Entity, target: &Target) -> Vec<String> {
    let packages: Vec<&str> = match target {
        Target::Ref { package: Some(package), .. }
        | Target::Macro { package: Some(package), .. }
        | Target::Dispatch { package: Some(package), .. } => vec![package],
        Target::Macro { package: None, .. } => {
            vec![entity.package.as_str(), search.first().copied().unwrap_or_default()]
        }
        _ => std::iter::once(entity.package.as_str()).chain(search.iter().copied()).collect(),
    };
    let candidates = packages.into_iter().map(|p| match target {
        Target::Ref { name, .. } => ["model", "seed", "snapshot"].map(|t| format!("{t}.{p}.{name}")).to_vec(),
        Target::Source { source, table } => vec![format!("source.{p}.{source}.{table}")],
        Target::Macro { name, .. } => vec![format!("macro.{p}.{name}")],
        Target::Dispatch { name, .. } => {
            let prefix = format!("macro.{p}.");
            let in_package = known.range(prefix.as_str()..).take_while(|id| id.starts_with(&prefix));
            in_package.filter(|id| id.ends_with(&format!("__{name}"))).map(|id| (*id).to_owned()).collect()
        }
    });
    let mut found = candidates
        .map(|ids| ids.into_iter().filter(|id| known.contains(id.as_str()) && *id != entity.id).collect::<Vec<_>>())
        .find(|ids| !ids.is_empty())
        .unwrap_or_default();
    if !matches!(target, Target::Dispatch { .. }) {
        found.truncate(1);
    }
    found
}

impl Graph {
    /// The graph that the files of PROJECT give.
    pub fn scanned(project: &Project) -> Graph {
        let known: BTreeSet<&str> = project.entities.iter().map(|e| e.id.as_str()).collect();
        let mut search: Vec<&str> = project.packages.keys().map(String::as_str).collect();
        search.sort_by_key(|package| (*package != project.root_package, *package));
        let mut sites: BTreeMap<(String, String), Vec<Range<usize>>> = BTreeMap::new();
        for entity in &project.entities {
            for call in &entity.calls {
                for to in resolve(&known, &search, entity, &call.target) {
                    sites.entry((entity.id.clone(), to)).or_default().push(call.range.clone());
                }
            }
        }
        let items = project.entities.iter().map(|e| {
            let item = Item {
                id: e.id.clone(),
                name: e.name.clone(),
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
    /// macros stay, and so do those of a node it did not list.  A node the
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
            let item = Item {
                id: node.id.clone(),
                name: node.name.clone(),
                resource_type,
                path,
                range: 0..0,
                name_range: 0..0,
            };
            self.items.entry(node.id.clone()).or_insert(item);
        }
        let kept =
            scanned.into_iter().filter(|((from, to), _)| to.starts_with("macro.") || !listed.contains(from.as_str()));
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
            {"name": "no unique_id"}
        ]});
        let Listing::Nodes(nodes) = parse_list_nodes(&answer) else { panic!("no nodes") };
        assert_eq!(nodes.len(), 3);
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
        let text = &project.texts[&graph.items["model.shop.orders"].path];
        assert_eq!(&text[stg.sites[0].clone()], "ref('stg_orders'");
        assert!(graph.items["test.shop.not_null_orders_id.abc"].path.ends_with("models/marts/_marts.yml"));
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
