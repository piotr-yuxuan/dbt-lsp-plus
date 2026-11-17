// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The resources of a dbt project and of its installed packages, read from
//! their files: what dbt itself would parse, without dbt.

use crate::packages;
use crate::properties::{self, Entry, Test};
use crate::scan::{self, Call, Kind, Symbol};
use crate::sources;
use crate::text::{self, Encoding, LineIndex};
use crate::yaml::{self, Node};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

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
    /// Each file of the resource paths, and each package's
    /// `dbt_project.yml`, as the proxy read it: its text, empty for a seed,
    /// and what the text gives.
    pub files: HashMap<PathBuf, Arc<Scanned>>,
}

impl Project {
    /// The text of the file at PATH, as the proxy read it.
    pub fn text(&self, path: &Path) -> Option<&str> {
        self.files.get(path).map(|file| file.text.as_str())
    }
}

/// What the text of one file alone gives, whatever its package or the
/// directory it is found in, so that it can be kept while the file stays
/// as it is.
#[derive(Debug, Default)]
pub struct Scanned {
    pub text: String,
    /// Its Jinja blocks, in SQL and Markdown.
    pub blocks: Vec<Symbol>,
    /// Its calls, as `sources::file_calls` reads them.
    pub calls: Vec<Call>,
    /// The source tables of a properties file, their package left empty.
    pub tables: Vec<SourceTable>,
    /// The entries of a properties file that declare a resource, which a
    /// reference's declaration names.
    pub entries: Vec<Entry>,
    /// The models of a properties file's unit tests, as written.
    pub models: Vec<(String, Range<usize>)>,
    /// The generic tests that a properties file names.
    pub tests: Vec<Test>,
}

impl Scanned {
    /// What TEXT, the content of the file at PATH, gives.
    pub fn of(path: &Path, text: String) -> Scanned {
        let mut calls = sources::file_calls(path, &text);
        if !sources::is_yaml(path) {
            return Scanned { blocks: scan::blocks(&text), calls, text, ..Scanned::default() };
        }
        // Its macro calls too, inside the Jinja of its values, such as hooks, but in its comments.
        let macros = |call: &Call| matches!(call.target, scan::Target::Macro { .. } | scan::Target::Dispatch { .. });
        calls.extend(scan::calls(&scan::without_yaml_comments(&text)).into_iter().filter(macros));
        calls.sort_by_key(|call| call.range.start);
        let Some(root) = yaml::parse(&text) else { return Scanned { calls, text, ..Scanned::default() } };
        let resource = |entry: &Entry| ["model", "seed", "snapshot", "analysis", "macro"].contains(&entry.label);
        Scanned {
            tables: source_tables_in("", path, &text, &root),
            entries: properties::entries_in(&text, &root, false).into_iter().filter(resource).collect(),
            models: unit_test_models(&text, &root),
            tests: properties::tests_in(&text, &root),
            calls,
            text,
            ..Scanned::default()
        }
    }

    /// The file at PATH as it is now, OPEN being its text if the editor has
    /// it open, and what that gives.  A seed's text is not read: its entity
    /// needs none, and a seed can be large.  A file that is not UTF-8, such
    /// as a `Thumbs.db`, gives nothing.  None when the file cannot be read.
    pub fn read(path: &Path, open: Option<&str>) -> Option<Scanned> {
        let seed = path.extension().is_some_and(|extension| extension == "csv");
        let text = match open {
            _ if seed => String::new(),
            Some(text) => text.to_owned(),
            None => text::decoded(fs::read(path).ok()?).unwrap_or_default(),
        };
        Some(Scanned::of(path, text))
    }
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
/// directory of LAYOUT, from what its text gives, SCANNED.
fn file_entities(package: &str, layout: &Layout, path: &Path, scanned: &Scanned) -> Vec<Entity> {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let calls = &scanned.calls;
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
    for block in &scanned.blocks {
        let layout_block = layout.blocks.iter().find(|(kind, ext, ..)| *kind == block.kind && *ext == extension);
        if let Some((_, _, resource_type, prefix)) = layout_block {
            let id = format!("{resource_type}.{package}.{prefix}{}", block.name);
            found.push(entity(resource_type, id, &block.name, block.range.clone(), block.name_range.clone()));
        }
    }
    if let Some((ext, resource_type)) = layout.file
        && ext == extension
        && found.iter().all(|e| e.resource_type == "doc")
    {
        let whole = 0..scanned.text.len();
        found.push(entity(resource_type, format!("{resource_type}.{package}.{stem}"), stem, whole, 0..0));
    }
    found
}

/// The models of the unit tests of the properties file TEXT, whose tree is
/// ROOT: each `model:` as written.
pub fn unit_test_models(text: &str, root: &Node) -> Vec<(String, Range<usize>)> {
    let index = LineIndex::new(text);
    let models = root.get("unit_tests").map_or(&[][..], Node::items).iter().filter_map(|unit| unit.get("model"));
    models.filter_map(|model| written(text, &index, model)).collect()
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
    yaml::parse(text).map_or_else(Vec::new, |root| source_tables_in(package, path, text, &root))
}

/// The tables of the `sources:` of the properties file TEXT of PACKAGE,
/// whose tree is ROOT, as `source_tables` gives them.
pub fn source_tables_in(package: &str, path: &Path, text: &str, root: &Node) -> Vec<SourceTable> {
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

/// Every file below DIR with its metadata, in a stable order, through
/// symbolic links but once per directory; nothing when DIR does not exist.
/// A file whose name starts with a dot is left out, as dbt leaves it out,
/// but not the files of a directory whose name does.
fn files(dir: &Path) -> Vec<(PathBuf, fs::Metadata)> {
    let mut pending = vec![dir.to_owned()];
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    let hidden = |path: &Path| path.file_name().is_some_and(|name| name.as_encoded_bytes().starts_with(b"."));
    while let Some(dir) = pending.pop() {
        if !seen.insert(fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone())) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for path in entries.flatten().map(|entry| entry.path()) {
            match fs::metadata(&path) {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind) if kind.is_file() && !hidden(&path) => found.push((path, kind)),
                _ => {}
            }
        }
    }
    found.sort_by(|(one, _), (other, _)| one.cmp(other));
    found
}

/// The name and configuration in a package's `dbt_project.yml`.
pub fn package_config(dir: &Path) -> Option<(String, Node)> {
    let config = yaml::parse(&text::read(&dir.join("dbt_project.yml"))?)?;
    let name = config.get("name")?.as_str()?.to_owned();
    Some((name, config))
}

/// A file of a project as a scan finds it: its path, its metadata, read
/// before its content, its package's index, and the layout of the directory
/// it was found in, none for a package's `dbt_project.yml`.
#[derive(Debug)]
pub struct Found {
    pub path: PathBuf,
    pub metadata: Option<fs::Metadata>,
    pub package: usize,
    pub layout: Option<usize>,
}

/// What a scan finds of a project: its packages, its own first, by name and
/// directory, and their files in the order of the scan.
#[derive(Debug)]
pub struct Listing {
    pub packages: Vec<(String, PathBuf)>,
    pub found: Vec<Found>,
}

/// What a scan finds of the project at ROOT: each package's resource paths,
/// layout by layout, then its `dbt_project.yml`, whose vars and hooks hold
/// calls too.  Its packages are those installed, then the local packages
/// that its packages files name and that are not.  None when ROOT has no
/// named `dbt_project.yml`.
pub fn find(root: &Path) -> Option<Listing> {
    let (root_package, root_config) = package_config(root)?;
    let install = root_config.get("packages-install-path").and_then(Node::as_str).unwrap_or("dbt_packages");
    let mut installed: Vec<PathBuf> =
        fs::read_dir(root.join(install)).into_iter().flatten().flatten().map(|e| e.path()).collect();
    installed.sort();
    let others = installed.into_iter().filter_map(|dir| package_config(&dir).map(|(name, config)| (name, dir, config)));
    let mut packages: Vec<(String, PathBuf, Node)> =
        std::iter::once((root_package, root.to_owned(), root_config)).chain(others).collect();
    // A local package not installed yet, as in a fresh clone, where its packages file names it.
    for dir in packages::local_dirs(root) {
        if let Some((name, config)) = package_config(&dir)
            && packages.iter().all(|(known, ..)| *known != name)
        {
            packages.push((name, dir, config));
        }
    }
    let mut found = Vec::new();
    for (package, (_, dir, config)) in packages.iter().enumerate() {
        for (at, layout) in LAYOUT.iter().enumerate() {
            let configured: Vec<&str> = config
                .get(layout.key)
                .map_or(Vec::new(), |paths| paths.items().iter().filter_map(Node::as_str).collect());
            let dirs = if configured.is_empty() { vec![layout.default] } else { configured };
            let listed = dirs.iter().flat_map(|sub| files(&dir.join(sub)));
            found.extend(listed.map(|(path, metadata)| Found {
                path,
                metadata: Some(metadata),
                package,
                layout: Some(at),
            }));
        }
        let path = dir.join("dbt_project.yml");
        found.push(Found { metadata: fs::metadata(&path).ok(), path, package, layout: None });
    }
    Some(Listing { packages: packages.into_iter().map(|(name, dir, _)| (name, dir)).collect(), found })
}

/// How a scan reads a file it found: its text and what the text gives.
pub type Read<'a> = dyn FnMut(&Found) -> Arc<Scanned> + 'a;

/// The project that LISTING finds, each file as READ gives it.
pub fn assemble(listing: &Listing, read: &mut Read) -> Project {
    let packages = &listing.packages;
    let mut project = Project {
        root_package: packages.first().map(|(name, _)| name.clone()).unwrap_or_default(),
        packages: HashMap::new(),
        entities: Vec::new(),
        sources: Vec::new(),
        files: HashMap::new(),
    };
    for file in &listing.found {
        let scanned = read(file);
        let name = &packages[file.package].0;
        let yaml = file.path.extension().is_some_and(|extension| extension == "yml" || extension == "yaml");
        match file.layout {
            Some(_) if yaml => {
                let package = |table: &SourceTable| SourceTable { package: name.clone(), ..table.clone() };
                let tables: Vec<SourceTable> = scanned.tables.iter().map(package).collect();
                project.entities.extend(tables.iter().map(Entity::from));
                project.sources.extend(tables);
            }
            Some(layout) => project.entities.extend(file_entities(name, &LAYOUT[layout], &file.path, &scanned)),
            None => {}
        }
        project.files.insert(file.path.clone(), scanned);
    }
    project.packages.extend(packages.iter().cloned());
    project
}

/// The project at ROOT and the packages installed in it, or `None` when ROOT
/// has no named `dbt_project.yml`.  OPEN holds the text of the documents the
/// editor has open, which wins over the file on disk.
pub fn scan(root: &Path, open: &HashMap<PathBuf, String>) -> Option<Project> {
    let listing = find(root)?;
    let mut read = |file: &Found| {
        Arc::new(Scanned::read(&file.path, open.get(&file.path).map(String::as_str)).unwrap_or_default())
    };
    Some(assemble(&listing, &mut read))
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
    fn a_properties_file_keeps_its_resources_unit_tests_and_macro_calls() {
        let text = "version: 2\nmodels:\n  - name: orders\n    description: '{{ doc(\"d\") }} {{ my_macro() }}'\n\
                    \x20   columns:\n      - name: id\nunit_tests:\n  - name: t\n    model: orders\n";
        let scanned = Scanned::of(Path::new("/p/models/_m.yml"), text.into());
        let entries: Vec<(&str, &str)> =
            scanned.entries.iter().map(|entry| (entry.label, entry.name.as_str())).collect();
        assert_eq!(entries, [("model", "orders")], "a column is no resource");
        let models: Vec<(&str, &str)> =
            scanned.models.iter().map(|(name, range)| (name.as_str(), &text[range.clone()])).collect();
        assert_eq!(models, [("orders", "orders")]);
        let macro_named = |calls: &[Call], wanted: &str| {
            calls.iter().any(|call| matches!(&call.target, scan::Target::Macro { name, .. } if name == wanted))
        };
        assert!(macro_named(&scanned.calls, "my_macro"));
        let hooks = "name: shop\non-run-start: \"{{ log_start() }}\"\n# on-run-end: \"{{ log_end() }}\"\n";
        let hooks = Scanned::of(Path::new("/p/dbt_project.yml"), hooks.into());
        assert!(macro_named(&hooks.calls, "log_start"), "a hook's macro");
        assert!(!macro_named(&hooks.calls, "log_end"), "dbt renders no comment");
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
    fn a_file_whose_name_starts_with_a_dot_is_no_resource() {
        let source = "sources:\n  - name: hidden\n    tables:\n      - name: t\n";
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/.hidden.sql", "select 1"),
            ("models/.p.yml", source),
            ("models/.dir/inner.sql", "select 1"),
            ("macros/.m.sql", "{% macro m() %}1{% endmacro %}"),
            ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
            ("dbt_packages/pkg/models/.ph.sql", "select 1"),
        ];
        let root = scratch_project("project-dot-files", &files);
        let project = scan(&root, &HashMap::new()).unwrap();
        let ids: Vec<&str> = project.entities.iter().map(|e| e.id.as_str()).collect();
        // As both builds parse it: the file in a dotted directory alone.
        assert_eq!(ids, ["model.shop.inner"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_local_package_not_installed_is_read_where_its_packages_file_names_it() {
        let files = [
            ("proj/dbt_project.yml", "name: r\n"),
            ("proj/packages.yml", "packages:\n  - local: shared\n  - local: ../beside\n"),
            ("proj/models/c.sql", "select * from {{ ref('s_m') }}"),
            ("proj/shared/dbt_project.yml", "name: shared\n"),
            ("proj/shared/models/s_m.sql", "select 1"),
            ("beside/dbt_project.yml", "name: bes\n"),
            ("beside/models/b_m.sql", "select 1"),
            // A local package's own local package is read from the root, as dbt reads it.
            ("beside/packages.yml", "packages:\n  - local: inner\n"),
            ("proj/inner/dbt_project.yml", "name: inner\n"),
            ("proj/inner/models/i_m.sql", "select 1"),
            ("deps/dbt_project.yml", "name: r\n"),
            ("deps/dependencies.yml", "packages:\n  - local: shared\n"),
            ("deps/shared/dbt_project.yml", "name: shared\n"),
            ("deps/shared/models/s_m.sql", "select 1"),
            ("deps/dbt_packages/shared/dbt_project.yml", "name: shared\n"),
            ("deps/dbt_packages/shared/models/s_m.sql", "select 1"),
            ("tab/dbt_project.yml", "name: r\n"),
            ("tab/packages.yml", "packages:\n  - local:\tshared\n"),
            ("tab/shared/dbt_project.yml", "name: shared\n"),
            ("tab/shared/models/s_m.sql", "select 1"),
        ];
        let dir = scratch_project("project-local-packages", &files);
        let ids = |root: &str| -> Vec<(String, PathBuf)> {
            let project = scan(&dir.join(root), &HashMap::new()).unwrap();
            let mut ids: Vec<(String, PathBuf)> = project
                .entities
                .iter()
                .map(|e| (e.id.clone(), e.path.strip_prefix(&project.packages[&e.package]).unwrap().to_owned()))
                .collect();
            ids.sort();
            ids
        };
        let at = |id: &str, path: &str| (id.to_owned(), PathBuf::from(path));
        // The ids dbt gives once it installs them: each package's project name, not its directory's.
        let expected = [
            at("model.bes.b_m", "models/b_m.sql"),
            at("model.inner.i_m", "models/i_m.sql"),
            at("model.r.c", "models/c.sql"),
            at("model.shared.s_m", "models/s_m.sql"),
        ];
        assert_eq!(ids("proj"), expected);
        let deps = scan(&dir.join("deps"), &HashMap::new()).unwrap();
        assert_eq!(deps.packages["shared"], dir.join("deps/dbt_packages/shared"), "the installed one, once");
        assert_eq!(ids("deps").len(), 1);
        assert_eq!(ids("tab"), Vec::new(), "a packages file dbt-lsp-plus cannot read names none");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn places_a_source_table_at_its_name_even_quoted() {
        let project = scan(&fixture(), &HashMap::new()).unwrap();
        for id in ["source.shop.raw.orders", "source.shop.events.page_views"] {
            let table = project.entities.iter().find(|e| e.id == id).unwrap();
            let text = project.text(&table.path).unwrap();
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
                &scan::Target::Ref { package: None, name: "customers".into(), version: None },
                &scan::Target::Macro { package: None, name: "ref".into() }
            ]
        );
    }

    #[test]
    fn a_seed_is_found_but_not_read() {
        let project = scan(&fixture(), &HashMap::new()).unwrap();
        let seed = project.entities.iter().find(|e| e.id == "seed.shop.raw_orders").unwrap();
        assert_eq!(project.text(&seed.path).unwrap(), "");
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
        assert_eq!(project.text(&bommed.path).unwrap(), macro_text);
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
        let text = project.text(&orders.path).unwrap();
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
