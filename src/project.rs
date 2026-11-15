// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The resources of a dbt project and of its installed packages, read from
//! their files: what dbt itself would parse, without dbt.

use crate::scan::{self, Call, Kind};
use crate::text::{self, Encoding, LineIndex};
use crate::yaml::{self, Node};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A resource: a model, seed, snapshot, analysis, singular test, macro,
/// source table or docs block, with the calls of its body.
#[derive(Clone, Debug)]
pub struct Entity {
    /// dbt's `unique_id`, such as `model.shop.orders`.
    pub id: String,
    pub name: String,
    pub resource_type: &'static str,
    pub package: String,
    pub path: PathBuf,
    pub range: Range<usize>,
    pub name_range: Range<usize>,
    pub calls: Vec<Call>,
}

pub struct Project {
    pub root_package: String,
    /// Package name to its directory, the root project included.
    pub packages: HashMap<String, PathBuf>,
    pub entities: Vec<Entity>,
    /// The source tables, as their properties files declare them.
    pub sources: Vec<SourceTable>,
    /// The text of each file of the resource paths, and of each package's
    /// `dbt_project.yml`, as the proxy read it; empty for a seed.
    pub texts: HashMap<PathBuf, String>,
}

/// Where a line goes right after a `name:` line: the end of that line, the
/// line break that ends it, the indentation of the key, and whether the
/// entry is in block style, the only style a line can go in.
#[derive(Clone, Debug, PartialEq)]
pub struct Insertion {
    pub at: usize,
    pub newline: &'static str,
    pub indent: usize,
    pub block: bool,
}

/// A source table as its properties file declares it: each name as written,
/// and what a rename must keep.
#[derive(Clone, Debug)]
pub struct SourceTable {
    pub package: String,
    pub path: PathBuf,
    pub source: String,
    pub source_range: Range<usize>,
    pub source_insertion: Insertion,
    /// Without `schema:`, dbt takes the source's name for its schema.
    pub has_schema: bool,
    pub table: String,
    pub table_range: Range<usize>,
    pub table_insertion: Insertion,
    /// Without `identifier:`, dbt takes the table's name for its identifier.
    pub has_identifier: bool,
    /// Whether its file shares YAML through an alias or a merge key, so that
    /// a name, a schema or an identifier may come from elsewhere in it.
    pub anchored: bool,
}

impl From<&SourceTable> for Entity {
    fn from(table: &SourceTable) -> Entity {
        Entity {
            id: format!("source.{}.{}.{}", table.package, table.source, table.table),
            name: format!("{}.{}", table.source, table.table),
            resource_type: "source",
            package: table.package.clone(),
            path: table.path.clone(),
            range: table.table_range.clone(),
            name_range: table.table_range.clone(),
            calls: Vec::new(),
        }
    }
}

/// A kind of resource directory: its key in `dbt_project.yml` and dbt's
/// default, the resource each file of an extension is, and the blocks it
/// holds as (block, extension, resource type, prefix of the name in the id).
struct Layout {
    key: &'static str,
    default: &'static str,
    file: Option<(&'static str, &'static str)>,
    blocks: &'static [(Kind, &'static str, &'static str, &'static str)],
}

const DOCS: (Kind, &str, &str, &str) = (Kind::Docs, "md", "doc", "");
const GENERIC_TEST: (Kind, &str, &str, &str) = (Kind::Test, "sql", "macro", "test_");

const LAYOUT: [Layout; 6] = [
    Layout { key: "model-paths", default: "models", file: Some(("sql", "model")), blocks: &[DOCS] },
    Layout { key: "seed-paths", default: "seeds", file: Some(("csv", "seed")), blocks: &[DOCS] },
    Layout {
        key: "snapshot-paths",
        default: "snapshots",
        file: None,
        blocks: &[(Kind::Snapshot, "sql", "snapshot", ""), DOCS],
    },
    Layout { key: "analysis-paths", default: "analyses", file: Some(("sql", "analysis")), blocks: &[DOCS] },
    // A file of generic tests is no singular test: see `file_entities`.
    Layout { key: "test-paths", default: "tests", file: Some(("sql", "test")), blocks: &[GENERIC_TEST, DOCS] },
    Layout {
        key: "macro-paths",
        default: "macros",
        file: None,
        blocks: &[(Kind::Macro, "sql", "macro", ""), GENERIC_TEST, DOCS],
    },
];

/// The entities of one file of SQL, Markdown or CSV of PACKAGE, found in a
/// directory of LAYOUT.
fn file_entities(package: &str, layout: &Layout, path: &Path, text: &str) -> Vec<Entity> {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let calls = scan::calls(text);
    let entity = |resource_type: &'static str, id: String, name: &str, range: Range<usize>, name_range| Entity {
        id,
        name: name.to_owned(),
        resource_type,
        package: package.to_owned(),
        path: path.to_owned(),
        calls: calls
            .iter()
            .filter(|call| range.start <= call.range.start && call.range.end <= range.end)
            .cloned()
            .collect(),
        range,
        name_range,
    };
    let mut found = Vec::new();
    for block in scan::blocks(text) {
        let layout_block = layout.blocks.iter().find(|(kind, ext, ..)| *kind == block.kind && *ext == extension);
        if let Some((_, _, resource_type, prefix)) = layout_block {
            let id = format!("{resource_type}.{package}.{prefix}{}", block.name);
            found.push(entity(resource_type, id, &block.name, block.range, block.name_range));
        }
    }
    if let Some((ext, resource_type)) = layout.file
        && ext == extension
        && found.iter().all(|e| e.resource_type == "doc")
    {
        found.push(entity(resource_type, format!("{resource_type}.{package}.{stem}"), stem, 0..text.len(), 0..0));
    }
    found
}

/// The value of the scalar NODE of TEXT and its range as written: inside
/// its quotes, escapes included, else as long as its value, else to the end
/// of its line.
pub fn written(text: &str, index: &LineIndex, node: &Node) -> Option<(String, Range<usize>)> {
    let Node::Scalar { value, line, column, .. } = node else { return None };
    let at = index.offset(*line as u64, *column as u64, Encoding::Utf32);
    let quoted = [&*DOUBLE_QUOTED, &*SINGLE_QUOTED].into_iter().find_map(|quote| quote.find(&text[at..]));
    let line_end = text[at..].find(['\n', '\r']).map_or(text.len(), |end| at + end);
    let range = match quoted {
        Some(quoted) => at + 1..at + quoted.end() - 1,
        None if text[at..].starts_with(value.as_str()) => at..at + value.len(),
        None => at..line_end,
    };
    Some((value.clone(), range))
}

/// The value of a `name:` entry, KEY and VALUE, as written in TEXT: its
/// range, and where a line goes after it.
fn placed(text: &str, index: &LineIndex, key: &Node, value: &Node) -> Option<(String, Range<usize>, Insertion)> {
    let Node::Scalar { line: key_line, column: key_column, .. } = key else { return None };
    let (value, range) = written(text, index, value)?;
    let line_end = text[range.end..].find(['\n', '\r']).map_or(text.len(), |end| range.end + end);
    let key_line_start = index.offset(*key_line as u64, 0, Encoding::Utf32);
    let before_key =
        text[key_line_start..index.offset(*key_line as u64, *key_column as u64, Encoding::Utf32)].trim_start();
    // A document in flow style, such as JSON, spans lines that look like blocks.
    let flow_document = text.trim_start().starts_with(['{', '[']);
    let block = !flow_document && before_key.strip_prefix('-').unwrap_or(before_key).trim().is_empty();
    let newline = ["\r\n", "\r"].into_iter().find(|newline| text[line_end..].starts_with(newline)).unwrap_or("\n");
    Some((value, range, Insertion { at: line_end, newline, indent: *key_column, block }))
}

/// The tables of the `sources:` of a properties file of PACKAGE.  A file
/// that is not valid YAML has none, and a source that `overrides` a
/// package's has none of its own: dbt patches the package's.
pub fn source_tables(package: &str, path: &Path, text: &str) -> Vec<SourceTable> {
    let Some(root) = yaml::parse(text) else { return Vec::new() };
    let index = LineIndex::new(text);
    // An anchor anywhere in the file may hold a name, a schema or an identifier.
    let anchored = root.shares();
    let mut found = Vec::new();
    for source in root.get("sources").map_or(&[][..], Node::items) {
        let own = source.entry("name").filter(|_| source.get("overrides").is_none());
        let Some((key, value)) = own else { continue };
        let Some((source_name, source_range, source_insertion)) = placed(text, &index, key, value) else { continue };
        for table in source.get("tables").map_or(&[][..], Node::items) {
            let Some((key, value)) = table.entry("name") else { continue };
            let Some((table_name, table_range, table_insertion)) = placed(text, &index, key, value) else { continue };
            found.push(SourceTable {
                package: package.to_owned(),
                path: path.to_owned(),
                source: source_name.clone(),
                source_range: source_range.clone(),
                source_insertion: source_insertion.clone(),
                has_schema: source.get("schema").is_some(),
                table: table_name,
                table_range,
                table_insertion,
                has_identifier: table.get("identifier").is_some(),
                anchored,
            });
        }
    }
    found
}

static DOUBLE_QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\A"(?:[^"\\]|\\.)*""#).unwrap());
static SINGLE_QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A'(?:[^']|'')*'").unwrap());

/// Every file below DIR, in a stable order, through symbolic links but once
/// per directory; nothing when DIR does not exist.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut pending = vec![dir.to_owned()];
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        if !seen.insert(fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone())) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for path in entries.flatten().map(|entry| entry.path()) {
            match fs::metadata(&path) {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind) if kind.is_file() => found.push(path),
                _ => {}
            }
        }
    }
    found.sort();
    found
}

/// The name and configuration in a package's `dbt_project.yml`.
fn package_config(dir: &Path) -> Option<(String, Node)> {
    let config = yaml::parse(&text::read(&dir.join("dbt_project.yml"))?)?;
    let name = config.get("name")?.as_str()?.to_owned();
    Some((name, config))
}

/// Adds to PROJECT the entities of the package NAME at DIR, configured by
/// CONFIG.  A seed's content is not read: its entity needs none, and a seed
/// can be large.
fn scan_package(project: &mut Project, dir: &Path, name: String, config: &Node, open: &HashMap<PathBuf, String>) {
    for layout in &LAYOUT {
        let configured: Vec<&str> =
            config.get(layout.key).map_or(Vec::new(), |paths| paths.items().iter().filter_map(Node::as_str).collect());
        let dirs = if configured.is_empty() { vec![layout.default] } else { configured };
        for path in dirs.iter().flat_map(|sub| files(&dir.join(sub))) {
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
            let text = match extension {
                "csv" => String::new(),
                _ => open.get(&path).cloned().or_else(|| text::read(&path)).unwrap_or_default(),
            };
            let (found, tables) = match extension {
                "yml" | "yaml" => {
                    let tables = source_tables(&name, &path, &text);
                    (tables.iter().map(Entity::from).collect(), tables)
                }
                _ => (file_entities(&name, layout, &path, &text), Vec::new()),
            };
            project.texts.insert(path, text);
            project.entities.extend(found);
            project.sources.extend(tables);
        }
    }
    // Its vars and hooks hold calls too.
    let config = dir.join("dbt_project.yml");
    let text = open.get(&config).cloned().or_else(|| text::read(&config)).unwrap_or_default();
    project.texts.insert(config, text);
    project.packages.insert(name, dir.to_owned());
}

/// The project at ROOT and the packages installed in it, or `None` when ROOT
/// has no named `dbt_project.yml`.  OPEN holds the text of the documents the
/// editor has open, which wins over the file on disk.
pub fn scan(root: &Path, open: &HashMap<PathBuf, String>) -> Option<Project> {
    let (root_package, root_config) = package_config(root)?;
    let mut project = Project {
        root_package: root_package.clone(),
        packages: HashMap::new(),
        entities: Vec::new(),
        sources: Vec::new(),
        texts: HashMap::new(),
    };
    scan_package(&mut project, root, root_package, &root_config, open);
    let install = root_config.get("packages-install-path").and_then(Node::as_str).unwrap_or("dbt_packages");
    let mut installed: Vec<PathBuf> =
        fs::read_dir(root.join(install)).into_iter().flatten().flatten().map(|e| e.path()).collect();
    installed.sort();
    for dir in installed {
        if let Some((name, config)) = package_config(&dir) {
            scan_package(&mut project, &dir, name, &config, open);
        }
    }
    Some(project)
}

/// A project of FILES, each a path under it and its text, in a fresh
/// directory of the system's for test NAME; the caller removes it.
#[cfg(test)]
pub(crate) fn scratch_project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dbt-lsp-plus-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for (path, text) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop")
    }

    #[test]
    fn finds_every_resource_of_the_project_and_its_packages() {
        let project = scan(&fixture(), &HashMap::new()).unwrap();
        let mut ids: Vec<&str> = project.entities.iter().map(|e| e.id.as_str()).collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                "analysis.shop.order_counts",
                "doc.shop.order_status",
                "macro.shop.cents_to_dollars",
                "macro.shop.default__safe_add",
                "macro.shop.dollars_to_cents",
                "macro.shop.duckdb__safe_add",
                "macro.shop.safe_add",
                "macro.shop.test_is_even",
                "macro.shop_utils.is_positive",
                "model.shop.customers",
                "model.shop.order_summary",
                "model.shop.orders",
                "model.shop.stg_customers",
                "model.shop.stg_orders",
                "model.shop.stg_page_views",
                "seed.shop.raw_customers",
                "seed.shop.raw_orders",
                "snapshot.shop.orders_snapshot",
                "source.shop.events.page_views",
                "source.shop.raw.customers",
                "source.shop.raw.orders",
                "test.shop.assert_positive_amount",
            ]
        );
        assert_eq!(project.packages["shop_utils"], fixture().join("dbt_packages/shop_utils"));
    }

    #[test]
    fn places_a_source_table_at_its_name_even_quoted() {
        let project = scan(&fixture(), &HashMap::new()).unwrap();
        for id in ["source.shop.raw.orders", "source.shop.events.page_views"] {
            let table = project.entities.iter().find(|e| e.id == id).unwrap();
            let text = &project.texts[&table.path];
            assert_eq!(&text[table.name_range.clone()], id.rsplit('.').next().unwrap());
        }
    }

    #[test]
    fn prefers_the_open_text_of_a_document() {
        let path = fixture().join("models/marts/orders.sql");
        let open = HashMap::from([(path.clone(), "select * from {{ ref('customers') }}".to_owned())]);
        let project = scan(&fixture(), &open).unwrap();
        let orders = project.entities.iter().find(|e| e.id == "model.shop.orders").unwrap();
        let targets: Vec<_> = orders.calls.iter().map(|call| &call.target).collect();
        assert_eq!(
            targets,
            [
                &scan::Target::Ref { package: None, name: "customers".into() },
                &scan::Target::Macro { package: None, name: "ref".into() }
            ]
        );
    }

    #[test]
    fn a_seed_is_found_but_not_read() {
        let project = scan(&fixture(), &HashMap::new()).unwrap();
        let seed = project.entities.iter().find(|e| e.id == "seed.shop.raw_orders").unwrap();
        assert_eq!(project.texts[&seed.path], "");
    }

    #[test]
    fn a_quoted_table_name_is_placed_as_written_escapes_included() {
        let text = "sources:\n  - name: raw\n    tables:\n      - name: \"a\\tb\\\"é\"\n      - name: 'it''s'\n";
        let tables = source_tables("shop", Path::new("s.yml"), text);
        let written: Vec<&str> = tables.iter().map(|t| &text[t.table_range.clone()]).collect();
        assert_eq!(written, ["a\\tb\\\"é", "it''s"]);
        assert_eq!(Entity::from(&tables[0]).id, "source.shop.raw.a\tb\"é");
        let index = LineIndex::new(text);
        let end = index.position(tables[0].table_range.end, Encoding::Utf16);
        assert_eq!(end, serde_json::json!({"line": 3, "character": 22}));
    }

    fn sorted_ids(project: &Project) -> Vec<&str> {
        let mut ids: Vec<&str> = project.entities.iter().map(|e| e.id.as_str()).collect();
        ids.sort();
        ids
    }

    #[test]
    fn follows_symbolic_links_once_and_reads_sources_in_every_resource_path() {
        let dir = scratch_project(
            "symlinks",
            &[
                ("dbt_project.yml", "name: linked\n"),
                ("models/real/a.sql", "select 1"),
                ("elsewhere/b.sql", "select 2"),
                ("seeds/_sources.yml", "sources:\n  - name: s\n    tables:\n      - name: t\n"),
            ],
        );
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("models/link")).unwrap();
        std::os::unix::fs::symlink(dir.join("models"), dir.join("models/real/loop")).unwrap();
        let project = scan(&dir, &HashMap::new()).unwrap();
        assert_eq!(sorted_ids(&project), ["model.linked.a", "model.linked.b", "source.linked.s.t"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_the_configured_resource_paths_and_packages_install_path() {
        let dir = scratch_project(
            "configured",
            &[
                (
                    "dbt_project.yml",
                    "name: p\nmodel-paths: [transform]\nmacro-paths: [lib]\npackages-install-path: deps\n",
                ),
                ("transform/a.sql", "select 1"),
                ("models/ignored.sql", "select 2"),
                ("lib/m.sql", "{% macro m() %}1{% endmacro %}"),
                ("deps/q/dbt_project.yml", "name: q\n"),
                ("deps/q/models/b.sql", "select 3"),
                ("dbt_packages/r/dbt_project.yml", "name: r\n"),
                ("dbt_packages/r/models/c.sql", "select 4"),
            ],
        );
        let project = scan(&dir, &HashMap::new()).unwrap();
        assert_eq!(sorted_ids(&project), ["macro.p.m", "model.p.a", "model.q.b"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_byte_order_mark_counts_in_no_position() {
        let macro_text = "{% macro bommed() %}1{% endmacro %}";
        let dir = scratch_project(
            "bom",
            &[("dbt_project.yml", "\u{feff}name: bom\n"), ("macros/bom.sql", &format!("\u{feff}{macro_text}"))],
        );
        let project = scan(&dir, &HashMap::new()).unwrap();
        let bommed = project.entities.iter().find(|e| e.id == "macro.bom.bommed").unwrap();
        assert_eq!(project.texts[&bommed.path], macro_text);
        assert_eq!(bommed.name_range, 9..15);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_source_that_overrides_a_package_s_is_the_package_s() {
        let dir = scratch_project(
            "overrides",
            &[
                ("dbt_project.yml", "name: p\n"),
                (
                    "models/_sources.yml",
                    "sources:\n  - name: stripe\n    overrides: q\n    tables:\n      - name: charge\n",
                ),
                ("dbt_packages/q/dbt_project.yml", "name: q\n"),
                (
                    "dbt_packages/q/models/_sources.yml",
                    "sources:\n  - name: stripe\n    tables:\n      - name: charge\n",
                ),
            ],
        );
        let project = scan(&dir, &HashMap::new()).unwrap();
        assert_eq!(sorted_ids(&project), ["source.q.stripe.charge"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_what_a_rename_of_a_source_needs() {
        let project = scan(&fixture(), &HashMap::new()).unwrap();
        let table = |name: &str| project.sources.iter().find(|t| t.table == name).unwrap();
        let (orders, views) = (table("orders"), table("page_views"));
        assert!(orders.has_identifier && orders.has_schema);
        assert!(!views.has_identifier && !views.has_schema);
        let text = &project.texts[&orders.path];
        assert_eq!((&text[orders.source_range.clone()], &text[views.source_range.clone()]), ("raw", "events"));
        // `      - name: orders`: the key `name` is at column 8.
        assert_eq!(orders.table_insertion.indent, 8);
        assert_eq!(&text[orders.table_insertion.at..orders.table_insertion.at + 1], "\n");
        assert!(text[..orders.table_insertion.at].ends_with("- name: orders"));
        assert!(orders.table_insertion.block);
        assert_eq!(views.source_insertion.indent, 4);
        let flow = source_tables("p", Path::new("f.yml"), "sources:\n  - {name: s, tables: [{name: t}]}\n");
        assert!(!flow[0].table_insertion.block && !flow[0].source_insertion.block);
    }

    #[test]
    fn a_directory_without_a_project_is_none() {
        assert!(scan(&fixture().join("models"), &HashMap::new()).is_none());
    }
}
