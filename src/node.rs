// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! What the proxy knows of each resource type of dbt's, and how a node's
//! unique id is written.  Pure.

/// A resource type of dbt's and what the proxy does with its nodes.
#[derive(Debug, PartialEq, Eq)]
pub struct Type {
    /// dbt's `resource_type`, the first word of its unique ids; empty for
    /// `OTHER`.
    pub name: &'static str,
    /// The label of its entries in a properties file, as `properties`
    /// gives it; none for a doc, which no entry declares.
    pub label: Option<&'static str>,
    /// Whether a `ref` names it.
    pub refable: bool,
    /// Whether the relation dbt writes for one of its nodes is one in the
    /// warehouse, which dbt builds, or loads for a source, and so shown:
    /// not an analysis, compiled and never built, nor a type that has
    /// none.  `OTHER`, a type the table lacks such as a hook, keeps the
    /// relation as dbt writes it.
    pub built: bool,
    /// LSP's `SymbolKind` of its nodes, wherever the proxy shows one:
    /// workspace symbols, the call hierarchy, the outline.  3 Namespace,
    /// 5 Class, 6 Method, 11 Interface, 12 Function, 15 String, 16 Number,
    /// 19 Object, 23 Struct, 24 Event.
    pub symbol: u32,
}

pub const MODEL: Type = Type { name: "model", label: Some("model"), refable: true, built: true, symbol: 5 };
pub const SEED: Type = Type { name: "seed", label: Some("seed"), refable: true, built: true, symbol: 23 };
pub const SNAPSHOT: Type = Type { name: "snapshot", label: Some("snapshot"), refable: true, built: true, symbol: 5 };
pub const ANALYSIS: Type = Type { name: "analysis", label: Some("analysis"), refable: false, built: false, symbol: 5 };
pub const TEST: Type = Type { name: "test", label: Some("data test"), refable: false, built: true, symbol: 24 };
pub const UNIT_TEST: Type =
    Type { name: "unit_test", label: Some("unit test"), refable: false, built: false, symbol: 24 };
pub const MACRO: Type = Type { name: "macro", label: Some("macro"), refable: false, built: false, symbol: 12 };
pub const FUNCTION: Type = Type { name: "function", label: Some("function"), refable: false, built: true, symbol: 12 };
pub const DOC: Type = Type { name: "doc", label: None, refable: false, built: false, symbol: 15 };
/// A source table: dbt's `source` nodes are its tables.
pub const SOURCE: Type = Type { name: "source", label: Some("table"), refable: false, built: true, symbol: 11 };
pub const EXPOSURE: Type = Type { name: "exposure", label: Some("exposure"), refable: false, built: false, symbol: 19 };
pub const METRIC: Type = Type { name: "metric", label: Some("metric"), refable: false, built: false, symbol: 16 };
pub const SEMANTIC_MODEL: Type =
    Type { name: "semantic_model", label: Some("semantic model"), refable: false, built: false, symbol: 5 };
pub const SAVED_QUERY: Type =
    Type { name: "saved_query", label: Some("saved query"), refable: false, built: false, symbol: 6 };
pub const GROUP: Type = Type { name: "group", label: Some("group"), refable: false, built: false, symbol: 3 };
/// A type the table lacks, one a newer dbt adds or a hook (`operation`):
/// a Class, its relation kept.
pub const OTHER: Type = Type { name: "", label: None, refable: false, built: true, symbol: 5 };

/// Every resource type the proxy knows, `OTHER` aside.
pub static TYPES: [Type; 15] = [
    MODEL,
    SEED,
    SNAPSHOT,
    ANALYSIS,
    TEST,
    UNIT_TEST,
    MACRO,
    FUNCTION,
    DOC,
    SOURCE,
    EXPOSURE,
    METRIC,
    SEMANTIC_MODEL,
    SAVED_QUERY,
    GROUP,
];

impl Type {
    /// The row of RESOURCE_TYPE, dbt's word for it, else `OTHER`.
    pub fn named(resource_type: &str) -> &'static Type {
        TYPES.iter().find(|row| row.name == resource_type).unwrap_or(&OTHER)
    }
}

/// The unique id of the node NAME of PACKAGE, of type KIND, as dbt writes
/// it for every node but a model's version and a source table:
/// `<type>.<package>.<name>`; a generic test's block is the macro
/// `test_<name>`.  With an empty NAME, the prefix of every id of that type
/// in PACKAGE.  `OTHER` has no name and builds no id: no caller passes it.
pub fn id(kind: &Type, package: &str, name: &str) -> String {
    format!("{}.{package}.{name}", kind.name)
}

/// `model.<package>.<model>.v<version>`, VERSION as its properties file
/// writes it; with an empty VERSION, the prefix of every version's id.
pub fn version_id(package: &str, model: &str, version: &str) -> String {
    format!("{}.v{version}", id(&MODEL, package, model))
}

/// `source.<package>.<source>`: a source, the group of its tables, which
/// is no node of dbt's.
pub fn group_id(package: &str, source: &str) -> String {
    id(&SOURCE, package, source)
}

/// `source.<package>.<source>.<table>`.
pub fn table_id(package: &str, source: &str, table: &str) -> String {
    format!("{}.{table}", group_id(package, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_written_as_dbt_writes_them() {
        // As the manifest of the fixture writes them.
        assert_eq!(id(&MODEL, "shop", "orders"), "model.shop.orders");
        assert_eq!(version_id("shop", "dim", "2"), "model.shop.dim.v2");
        assert_eq!(table_id("shop", "raw", "customers"), "source.shop.raw.customers");
        assert_eq!(group_id("shop", "raw"), "source.shop.raw");
        assert_eq!(id(&MACRO, "shop", "test_positive"), "macro.shop.test_positive");
        // The prefixes of every macro of a package and of every version of a model.
        assert_eq!(id(&MACRO, "shop", ""), "macro.shop.");
        assert_eq!(version_id("shop", "dim", ""), "model.shop.dim.v");
    }

    #[test]
    fn a_type_is_named_once_and_one_dbt_adds_is_shown_as_before() {
        let names: HashSet<&str> = TYPES.iter().map(|row| row.name).collect();
        assert_eq!(names.len(), TYPES.len(), "a resource type named twice");
        let labels: Vec<&str> = TYPES.iter().filter_map(|row| row.label).collect();
        assert_eq!(labels.iter().collect::<HashSet<_>>().len(), labels.len(), "a label given twice");
        // dbt's docs: a `ref` names a model, a seed or a snapshot.
        let refable: HashSet<&str> = TYPES.iter().filter(|row| row.refable).map(|row| row.name).collect();
        assert_eq!(refable, HashSet::from(["model", "seed", "snapshot"]));
        // Of the types the manifest's `nodes` and `sources` hold, dbt builds all but an analysis.
        let held = ["model", "seed", "snapshot", "analysis", "test", "source", "function"];
        let unbuilt: Vec<&str> = held.into_iter().filter(|name| !Type::named(name).built).collect();
        assert_eq!(unbuilt, ["analysis"]);
        for name in ["operation", ""] {
            let row = Type::named(name);
            assert_eq!(row, &OTHER, "{name:?}");
            assert_eq!((row.built, row.symbol, row.refable), (true, 5, false), "{name:?}: built, a Class, no ref");
        }
    }
}
