// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The shadow: what the proxy would answer to a definition or references
//! that dbt lsp answers, compared location by location, counted for the
//! session and logged on the user's disk, never sent anywhere: the evidence
//! on which a method's shipped mode may change.

use crate::config::{self, Index};
use crate::modes::Table;
use crate::navigation::Target;
use crate::project::Project;
use crate::{scan, sources};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The command that answers the session's counts, with the shadow on.
pub const SUMMARY: &str = "dbt-lsp-plus.shadowSummary";
/// The unmatched locations of each side that a line shows, at most.
const SHOWN: usize = 20;
/// The size below which a line stays.
const LINE_MAX: usize = 8 * 1024;
/// A log of this size is renamed `.jsonl.1` before the next line.
const ROTATE_AT: u64 = 1024 * 1024;
/// A shadow file that nothing wrote for this long is removed at start.
const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);

/// A location as the shadow compares it: its file, and its range from a
/// (line, character) to another.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location {
    pub path: PathBuf,
    pub start: (u64, u64),
    pub end: (u64, u64),
}

/// How the proxy's answer and dbt lsp's stand, or why they were not compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// Each location of either side matches one of the other's.
    Equal,
    /// Each of dbt lsp's matches one of the proxy's, which has more.
    OursAdds,
    /// Each of the proxy's matches one of dbt lsp's, which has more.
    OursMisses,
    /// Some match, and each side has others.
    Overlap,
    /// None match.
    Disjoint,
    OursEmpty,
    DbtEmpty,
    BothEmpty,
    /// Not compared: dbt lsp gave a location in no file on disk, as for
    /// dbt's own macros, or one the shadow cannot read.
    DbtMissingFile,
    /// Not compared: dbt lsp answered an error.
    DbtError,
    /// Not compared: the client edited a document before dbt lsp answered.
    Changed,
}

impl Relation {
    /// Its name in the log and the counts: `equal`, `oursAdds`...
    pub fn name(self) -> String {
        let name = format!("{self:?}");
        name[..1].to_lowercase() + &name[1..]
    }
}

/// dbt lsp's answer as the shadow reads it: its locations, or why it is
/// not compared.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Locations(Vec<Location>),
    Apart(Relation),
}

/// The entries of RESULT, an answer to a definition or references: none
/// for null, the one location or link, or each of a list.
fn entries(result: &Value) -> Vec<&Value> {
    match result {
        Value::Null => Vec::new(),
        Value::Array(entries) => entries.iter().collect(),
        entry => vec![entry],
    }
}

/// The location of ENTRY, a `Location` or a `LocationLink` by its target's
/// name, its URI read as a path by PATH_OF; none for one it cannot read.
fn location(entry: &Value, path_of: &dyn Fn(&str) -> Option<PathBuf>) -> Option<Location> {
    let uri = entry.get("uri").or_else(|| entry.get("targetUri"))?.as_str()?;
    let range = entry.get("range").or_else(|| entry.get("targetSelectionRange"))?;
    let position = |at: &str| Some((range[at]["line"].as_u64()?, range[at]["character"].as_u64()?));
    Some(Location { path: path_of(uri)?, start: position("start")?, end: position("end")? })
}

/// The locations of RESULT, URIs read as paths by PATH_OF, sorted, once
/// each; those it cannot read left out.
pub fn locations(result: &Value, path_of: &dyn Fn(&str) -> Option<PathBuf>) -> Vec<Location> {
    let mut locations: Vec<Location> = entries(result).into_iter().filter_map(|e| location(e, path_of)).collect();
    locations.sort();
    locations.dedup();
    locations
}

/// dbt lsp's answer MESSAGE, CHANGED when the client edited a document
/// since the request, URIs read as paths by PATH_OF: apart for an error,
/// then a change, then a location it cannot read or whose file EXISTS
/// denies; else its `locations`.
pub fn read(
    message: &Value,
    changed: bool,
    path_of: &dyn Fn(&str) -> Option<PathBuf>,
    exists: &dyn Fn(&Path) -> bool,
) -> Answer {
    if message.get("error").is_some() {
        return Answer::Apart(Relation::DbtError);
    }
    if changed {
        return Answer::Apart(Relation::Changed);
    }
    let result = &message["result"];
    let on_disk = |entry: &&Value| location(entry, path_of).is_some_and(|location| exists(&location.path));
    match entries(result).iter().all(on_disk) {
        true => Answer::Locations(locations(result, path_of)),
        false => Answer::Apart(Relation::DbtMissingFile),
    }
}

/// What the comparison of two answers found: their relation, the locations
/// that only one side has, and how many of the proxy's match dbt lsp's.
#[derive(Debug, PartialEq)]
pub struct Compared {
    pub relation: Relation,
    pub ours_only: Vec<Location>,
    pub dbt_only: Vec<Location>,
    pub both: usize,
}

/// The proxy's locations OURS against dbt lsp's DBT, each sorted, as
/// `locations` gives them: two match when they are in one file and the
/// range of one holds the other's.  `both` counts the proxy's that match.
pub fn compare(ours: &[Location], dbt: &[Location]) -> Compared {
    let holds = |outer: &Location, inner: &Location| outer.start <= inner.start && inner.end <= outer.end;
    // A location is looked for among the other side's of its file alone.
    let unmatched = |side: &[Location], other: &[Location]| -> Vec<Location> {
        let matched = |location: &&Location| {
            let first = other.partition_point(|candidate| candidate.path < location.path);
            let in_file = other[first..].iter().take_while(|candidate| candidate.path == location.path);
            in_file.into_iter().any(|candidate| holds(location, candidate) || holds(candidate, location))
        };
        side.iter().filter(|location| !matched(location)).cloned().collect()
    };
    let (ours_only, dbt_only) = (unmatched(ours, dbt), unmatched(dbt, ours));
    let both = ours.len() - ours_only.len();
    let relation = match (ours.is_empty(), dbt.is_empty(), ours_only.is_empty(), dbt_only.is_empty(), both > 0) {
        (true, true, ..) => Relation::BothEmpty,
        (true, false, ..) => Relation::OursEmpty,
        (false, true, ..) => Relation::DbtEmpty,
        (_, _, true, true, _) => Relation::Equal,
        (_, _, false, true, _) => Relation::OursAdds,
        (_, _, true, false, _) => Relation::OursMisses,
        (.., true) => Relation::Overlap,
        (.., false) => Relation::Disjoint,
    };
    Compared { relation, ours_only, dbt_only, both }
}

/// TARGET as the log shows it: its kind, package, name and version.
pub fn target(target: &Target) -> Value {
    let (package, name, version) = match target {
        Target::Source(sources::Target::Table { source, table })
        | Target::Call(scan::Target::Source { source, table }) => (None, format!("{source}.{table}"), None),
        Target::Source(sources::Target::Source { source }) => (None, source.clone(), None),
        Target::Call(scan::Target::Ref { package, name, version }) => {
            (package.as_ref(), name.clone(), version.as_ref())
        }
        Target::Call(
            scan::Target::Macro { package, name }
            | scan::Target::Dispatch { package, name }
            | scan::Target::Function { package, name },
        )
        | Target::Named { package, name, .. } => (package.as_ref(), name.clone(), None),
    };
    json!({"kind": target.kind(), "package": package, "name": name, "version": version})
}

/// The ids and files of what dbt resolved a call to.
pub type Oracle = Vec<(String, Option<PathBuf>)>;

/// dbt's own resolution of TARGET, read in the file at RELATIVE, a path from
/// the root, as INDEX holds it from the last parse: for a `ref` or a
/// `function`, the ids among those the file's one node depends on that are
/// of the call's kind and bear its name, in the package and at the version
/// the call names, if it names them; each with its file in PROJECT, the
/// node's own in its package, else, for a function, which the index does
/// not hold, its id read and the scan's file.  None for any other target,
/// or for a file with no one node.
pub fn oracle(index: &Index, project: &Project, relative: &str, target: &Target) -> Option<Oracle> {
    let (kinds, package, name, version): (&[&str], _, _, _) = match target {
        Target::Call(scan::Target::Ref { package, name, version }) => {
            (&["model", "seed", "snapshot"], package, name, version.as_deref().filter(|v| !v.is_empty()))
        }
        Target::Call(scan::Target::Function { package, name }) => (&["function"], package, name, None),
        _ => return None,
    };
    let in_package = |named: Option<&str>| package.as_deref().is_none_or(|package| named == Some(package));
    let resolved = |node: &Value| {
        let at = node["version"].as_str().map_or_else(|| node["version"].to_string(), str::to_owned);
        kinds.iter().any(|kind| node["resource_type"] == *kind)
            && node["name"] == name.as_str()
            && in_package(node["package_name"].as_str())
            && version.is_none_or(|version| at == version)
    };
    // `KIND.PACKAGE.NAME`, as a function's id is.
    let by_id = |id: &str| {
        let parts: Vec<&str> = id.splitn(3, '.').collect();
        kinds.contains(&parts[0]) && parts.get(2) == Some(&name.as_str()) && in_package(parts.get(1).copied())
    };
    let file = |id: &str| match index.node(id) {
        Some(node) => {
            Some(project.packages.get(node["package_name"].as_str()?)?.join(node["original_file_path"].as_str()?))
        }
        None => project.entities.iter().find(|entity| entity.id == id).map(|entity| entity.path.clone()),
    };
    let node = index.of_file(relative)?;
    let depends_on = node.pointer("/depends_on/nodes").and_then(Value::as_array).into_iter().flatten();
    let named = depends_on.filter_map(Value::as_str).filter(|id| index.node(id).map_or_else(|| by_id(id), resolved));
    Some(named.map(|id| (id.to_owned(), file(id))).collect())
}

/// A shadowed request, as its line logs it and the counts count it.
#[derive(Debug)]
pub struct Record {
    /// `definition` or `references`.
    pub method: String,
    /// The file that asked, as the root maps it.
    pub file: Option<PathBuf>,
    /// The kind of the target as `experimental.dbtLspPlus` names it, or
    /// `none` where the proxy reads none.
    pub kind: &'static str,
    /// `sql`, `yaml` or `python`, by the asking file's extension, else `other`.
    pub place: &'static str,
    pub compared: Compared,
    /// The target as read, as `target` shows it, or null.
    pub target: Value,
    /// The state of the parse's index.
    pub index: Value,
    /// dbt's own resolution, as `oracle` gives it.
    pub oracle: Option<Oracle>,
}

impl Record {
    /// Its line, paths relative to ROOT: under `LINE_MAX`, with at most
    /// `SHOWN` unmatched locations of each side, fewer as the line needs,
    /// and with neither names nor paths where even none is too long.
    pub fn line(&self, root: &Path) -> String {
        let relative = |path: &Path| path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
        let shown = |locations: &[Location], at_most: usize| -> Value {
            let shown = |l: &Location| json!({"path": relative(&l.path), "range": [[l.start.0, l.start.1], [l.end.0, l.end.1]]});
            locations.iter().take(at_most).map(shown).collect()
        };
        let oracle = self.oracle.as_ref().map(|oracle| {
            let entry =
                |(id, file): &(String, Option<PathBuf>)| json!({"id": id, "file": file.as_deref().map(relative)});
            oracle.iter().map(entry).collect::<Vec<_>>()
        });
        let compared = &self.compared;
        let counts =
            json!({"oursOnly": compared.ours_only.len(), "dbtOnly": compared.dbt_only.len(), "both": compared.both});
        let mut line = json!({"type": "request", "pid": std::process::id(), "method": self.method,
                              "file": self.file.as_deref().map(relative), "kind": self.kind, "place": self.place,
                              "relation": compared.relation.name(), "counts": counts, "target": self.target,
                              "index": self.index, "oracle": oracle});
        for at_most in [SHOWN, 10, 5, 2, 1, 0] {
            line["oursOnly"] = shown(&compared.ours_only, at_most);
            line["dbtOnly"] = shown(&compared.dbt_only, at_most);
            let text = line.to_string();
            if text.len() < LINE_MAX {
                return text;
            }
        }
        ["file", "target", "oracle"].into_iter().for_each(|long| line[long] = Value::Null);
        line.to_string()
    }
}

/// The session's first line: when it began, the proxy's version, dbt lsp's
/// SERVER_INFO, unless too long, and the modes of the requests that the
/// shadow follows, from TABLE.
pub fn start(server_info: &Value, table: &Table) -> String {
    let time = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let mode = |method: &str| format!("{:?}", table.mode(method)).to_lowercase();
    let modes = json!({"definition": mode("textDocument/definition"), "references": mode("textDocument/references")});
    let mut line = json!({"type": "start", "pid": std::process::id(), "time": time, "proxy": env!("CARGO_PKG_VERSION"),
                          "server": server_info, "modes": modes});
    if line.to_string().len() >= LINE_MAX {
        line["server"] = Value::Null;
    }
    line.to_string()
}

/// The session's shadowed requests, counted by method, kind of target,
/// place and relation.
#[derive(Debug, Default)]
pub struct Counts(BTreeMap<[String; 4], u64>);

impl Counts {
    pub fn add(&mut self, record: &Record) {
        let key = [record.method.clone(), record.kind.into(), record.place.into(), record.compared.relation.name()];
        *self.0.entry(key).or_default() += 1;
    }

    /// The number of requests, and each count under its method, kind, place
    /// and relation.
    pub fn summary(&self) -> Value {
        let mut counts = json!({});
        for ([method, kind, place, relation], count) in &self.0 {
            counts[method][kind][place][relation] = json!(count);
        }
        json!({"requests": self.0.values().sum::<u64>(), "counts": counts})
    }

    /// The session's last line: its `summary`.
    pub fn end(&self) -> String {
        let mut line = self.summary();
        (line["type"], line["pid"]) = (json!("end"), json!(std::process::id()));
        line.to_string()
    }
}

/// The shadow's log of a root: each line appended in one write to a file
/// created owner-only, renamed `.jsonl.1` once it reaches `ROTATE_AT`.
#[derive(Debug)]
pub struct Log {
    pub path: PathBuf,
}

impl Log {
    /// The log of ROOT under BASE, the user's cache directory, in a directory
    /// made owner-only; the logs there, current or renamed, that nothing
    /// wrote for `STALE_AFTER` are removed first, and nothing else.
    pub fn open(base: &Path, root: &Path) -> Log {
        let dir = base.join("dbt-lsp-plus").join("shadow");
        let log = |entry: &fs::DirEntry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && (name.ends_with(".jsonl") || name.ends_with(".jsonl.1"))
        };
        let age = |entry: &fs::DirEntry| entry.metadata().and_then(|meta| meta.modified()).ok()?.elapsed().ok();
        let logs = fs::read_dir(&dir).into_iter().flatten().flatten().filter(log);
        for stale in logs.filter(|entry| age(entry).is_some_and(|age| age > STALE_AFTER)) {
            let _ = fs::remove_file(stale.path());
        }
        let _ = DirBuilder::new().recursive(true).mode(0o700).create(&dir);
        Log { path: dir.join(format!("{}.jsonl", config::hash([root.to_string_lossy().into_owned()]))) }
    }

    /// Appends LINE and a newline in one write.  A rename that fails, as when
    /// another proxy on the root renamed the file first, leaves it to grow.
    pub fn append(&self, line: &str) -> io::Result<()> {
        if fs::metadata(&self.path).is_ok_and(|meta| meta.len() >= ROTATE_AT) {
            let _ = fs::rename(&self.path, self.path.with_extension("jsonl.1"));
        }
        let mut file = OpenOptions::new().create(true).append(true).mode(0o600).open(&self.path)?;
        file.write_all(format!("{line}\n").as_bytes())
    }
}

/// A session's shadow: its counts, and its log where there is a place for
/// it, the first failure of which is said.
#[derive(Debug, Default)]
pub struct Shadow {
    pub counts: Counts,
    log: Option<Log>,
    failed: bool,
}

impl Shadow {
    pub fn new(log: Option<Log>) -> Shadow {
        Shadow { log, ..Shadow::default() }
    }

    /// Appends LINE to the log; what to say of the session's first failure,
    /// or of its having no place for a log.
    pub fn write(&mut self, line: &str) -> Option<String> {
        let failure = match &self.log {
            Some(log) => {
                log.append(line).err().map(|error| format!("cannot be written at {} ({error})", log.path.display()))
            }
            None => Some("has no place, with neither XDG_CACHE_HOME nor HOME absolute".to_owned()),
        };
        let first = failure.is_some() && !std::mem::replace(&mut self.failed, true);
        failure.filter(|_| first).map(|why| format!("dbt-lsp-plus's shadow log {why}; the shadow goes on counting"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::scratch_project;
    use std::os::unix::fs::PermissionsExt;

    fn at(path: &str, start: (u64, u64), end: (u64, u64)) -> Location {
        Location { path: PathBuf::from(path), start, end }
    }

    fn relation(ours: &[Location], dbt: &[Location]) -> Relation {
        compare(ours, dbt).relation
    }

    /// A `Location` of LSP in the file at PATH.
    fn lsp(path: &str, start: (u64, u64), end: (u64, u64)) -> Value {
        json!({"uri": format!("file://{path}"), "range": {"start": {"line": start.0, "character": start.1},
                                                         "end": {"line": end.0, "character": end.1}}})
    }

    fn path_of(uri: &str) -> Option<PathBuf> {
        uri.strip_prefix("file://").map(PathBuf::from)
    }

    #[test]
    fn two_locations_match_in_one_file_when_one_range_holds_the_other() {
        let block = at("/p/m.sql", (0, 0), (5, 0));
        let name = at("/p/m.sql", (1, 2), (1, 9));
        let (block, name) = (std::slice::from_ref(&block), std::slice::from_ref(&name));
        assert_eq!(relation(block, name), Relation::Equal, "the proxy's range holds dbt's");
        assert_eq!(relation(name, block), Relation::Equal, "dbt's range holds the proxy's");
        let crossing = at("/p/m.sql", (4, 0), (7, 0));
        assert_eq!(relation(block, &[crossing]), Relation::Disjoint, "ranges that only cross");
        assert_eq!(relation(block, &[at("/p/n.sql", (1, 2), (1, 9))]), Relation::Disjoint, "another file");
        assert_eq!(compare(&[], &[]).both, 0);
    }

    #[test]
    fn each_relation_is_told_apart_with_what_only_one_side_has() {
        let (x, y, z) =
            (at("/p/x.sql", (0, 0), (0, 0)), at("/p/y.sql", (0, 0), (0, 0)), at("/p/z.sql", (0, 0), (0, 0)));
        let adds = compare(&[x.clone(), y.clone()], std::slice::from_ref(&x));
        assert_eq!(
            (adds.relation, adds.ours_only, adds.dbt_only, adds.both),
            (Relation::OursAdds, vec![y.clone()], vec![], 1)
        );
        let misses = compare(std::slice::from_ref(&x), &[x.clone(), y.clone()]);
        assert_eq!((misses.relation, misses.dbt_only, misses.both), (Relation::OursMisses, vec![y.clone()], 1));
        let overlap = compare(&[x.clone(), y.clone()], &[x.clone(), z.clone()]);
        assert_eq!(
            (overlap.relation, overlap.ours_only, overlap.dbt_only),
            (Relation::Overlap, vec![y.clone()], vec![z.clone()])
        );
        assert_eq!(relation(std::slice::from_ref(&y), std::slice::from_ref(&z)), Relation::Disjoint);
        assert_eq!(relation(&[], std::slice::from_ref(&x)), Relation::OursEmpty);
        assert_eq!(relation(std::slice::from_ref(&x), &[]), Relation::DbtEmpty);
        assert_eq!(relation(&[], &[]), Relation::BothEmpty);
        let names: Vec<String> =
            [Relation::OursAdds, Relation::DbtMissingFile, Relation::Equal].map(Relation::name).into();
        assert_eq!(names, ["oursAdds", "dbtMissingFile", "equal"]);
    }

    #[test]
    fn a_location_is_matched_by_any_of_the_other_side_s_in_its_file() {
        let (one, nine) = (at("/p/a.sql", (1, 0), (1, 4)), at("/p/a.sql", (9, 0), (9, 4)));
        let adds = compare(&[one.clone(), nine.clone()], std::slice::from_ref(&nine));
        assert_eq!((adds.relation, adds.ours_only, adds.both), (Relation::OursAdds, vec![one], 1), "not the first");
        let block = at("/p/a.sql", (0, 0), (20, 0));
        let held = compare(std::slice::from_ref(&block), &[at("/p/a.sql", (1, 0), (1, 4)), nine]);
        assert_eq!((held.relation, held.both), (Relation::Equal, 1), "both counts the proxy's that match");
    }

    #[test]
    fn every_relation_has_its_name() {
        let relations = [
            Relation::Equal,
            Relation::OursAdds,
            Relation::OursMisses,
            Relation::Overlap,
            Relation::Disjoint,
            Relation::OursEmpty,
            Relation::DbtEmpty,
            Relation::BothEmpty,
            Relation::DbtMissingFile,
            Relation::DbtError,
            Relation::Changed,
        ];
        let names = [
            "equal",
            "oursAdds",
            "oursMisses",
            "overlap",
            "disjoint",
            "oursEmpty",
            "dbtEmpty",
            "bothEmpty",
            "dbtMissingFile",
            "dbtError",
            "changed",
        ];
        assert_eq!(relations.map(Relation::name), names.map(str::to_owned));
    }

    #[test]
    fn the_order_and_repetitions_of_an_answer_do_not_count() {
        let (a, b) = (lsp("/p/a.sql", (0, 0), (0, 0)), lsp("/p/b.sql", (2, 0), (2, 4)));
        let one = locations(&json!([a, b, a]), &path_of);
        assert_eq!(one, locations(&json!([b, a]), &path_of));
        assert_eq!(one.len(), 2);
        assert_eq!(relation(&one, &locations(&json!([b, a]), &path_of)), Relation::Equal);
        assert_eq!(locations(&a, &path_of).len(), 1, "a location alone");
        assert_eq!(locations(&Value::Null, &path_of), vec![]);
        let link = json!({"targetUri": "file:///p/b.sql", "targetRange": {}, "targetSelectionRange":
                          {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 4}}});
        assert_eq!(locations(&json!([link]), &path_of), locations(&b, &path_of), "a link by its target's name");
    }

    #[test]
    fn an_error_a_change_or_a_file_not_on_disk_is_not_compared() {
        let on_disk = |path: &Path| path != Path::new("/p/gone.sql");
        let read = |message: &Value, changed: bool| super::read(message, changed, &path_of, &on_disk);
        let error = json!({"id": 1, "error": {"code": -32603, "message": "no"}});
        assert_eq!(read(&error, true), Answer::Apart(Relation::DbtError), "an error first");
        let found = json!({"id": 1, "result": lsp("/p/a.sql", (0, 0), (0, 0))});
        assert_eq!(read(&found, true), Answer::Apart(Relation::Changed));
        assert_eq!(read(&found, false), Answer::Locations(vec![at("/p/a.sql", (0, 0), (0, 0))]));
        let gone = json!({"id": 1, "result": [lsp("/p/a.sql", (0, 0), (0, 0)), lsp("/p/gone.sql", (0, 0), (0, 0))]});
        assert_eq!(read(&gone, false), Answer::Apart(Relation::DbtMissingFile));
        assert_eq!(read(&gone, true), Answer::Apart(Relation::Changed), "a change before a missing file");
        let elsewhere = json!({"id": 1, "result": {"uri": "untitled:1", "range": {}}});
        assert_eq!(read(&elsewhere, false), Answer::Apart(Relation::DbtMissingFile), "no file at all");
        assert_eq!(read(&json!({"id": 1, "result": null}), false), Answer::Locations(vec![]));
    }

    fn index() -> Index {
        let node = |id: &str, name: &str, path: &str, depends_on: &[&str]| {
            let mut parts = id.split('.');
            json!({"unique_id": id, "name": name, "resource_type": parts.next(), "package_name": parts.next(),
                   "original_file_path": path, "depends_on": {"nodes": depends_on}})
        };
        let versioned = |version: Value| {
            let mut node =
                node(&format!("model.shop.dim.v{version}"), "dim", &format!("models/dim_v{version}.sql"), &[]);
            node["version"] = version;
            node
        };
        let orders = [
            "model.shop.stg_orders",
            "model.utils.stg_orders",
            "source.shop.raw.stg_orders",
            "model.shop.dim.v1",
            "model.shop.dim.v2",
            "model.shop.other",
            "function.shop.f",
        ];
        Index::of(&json!({"metadata": {"project_name": "shop"}, "nodes": {
            "model.shop.orders": node("model.shop.orders", "orders", "models/orders.sql", &orders),
            "model.shop.stg_orders": node("model.shop.stg_orders", "stg_orders", "models/stg_orders.sql", &[]),
            "model.utils.stg_orders": node("model.utils.stg_orders", "stg_orders", "models/stg.sql", &[]),
            "model.shop.dim.v1": versioned(json!(1)), "model.shop.dim.v2": versioned(json!(2)),
            "model.shop.other": node("model.shop.other", "other", "models/other.sql", &[])},
            "sources": {"source.shop.raw.stg_orders": node("source.shop.raw.stg_orders", "stg_orders", "models/s.yml", &[])}}))
    }

    /// The project at /p, with the package utils and the function f, which
    /// only the scan holds.
    fn project() -> Project {
        let f = crate::project::Entity {
            id: "function.shop.f".into(),
            name: "f".into(),
            version: None,
            latest: true,
            resource_type: "function",
            package: "shop".into(),
            path: PathBuf::from("/p/functions/f.sql"),
            range: 0..0,
            name_range: 0..0,
            calls: Vec::new(),
        };
        let packages = [("shop", "/p"), ("utils", "/p/dbt_packages/utils")];
        Project {
            root_package: "shop".into(),
            entities: vec![f],
            sources: Vec::new(),
            files: Default::default(),
            packages: packages.into_iter().map(|(name, dir)| (name.into(), PathBuf::from(dir))).collect(),
        }
    }

    fn call(target: scan::Target) -> Target {
        Target::Call(target)
    }

    fn resolved(target: scan::Target) -> Option<Vec<(String, Option<PathBuf>)>> {
        oracle(&index(), &project(), "models/orders.sql", &call(target))
    }

    fn found(entries: &[(&str, &str)]) -> Option<Vec<(String, Option<PathBuf>)>> {
        Some(entries.iter().map(|(id, file)| ((*id).to_owned(), Some(PathBuf::from(file)))).collect())
    }

    #[test]
    fn the_oracle_is_what_the_file_s_node_depends_on_of_the_call_s_kind_and_name() {
        let named = |package: Option<&str>, name: &str, version: Option<&str>| scan::Target::Ref {
            package: package.map(str::to_owned),
            name: name.into(),
            version: version.map(str::to_owned),
        };
        let both = [
            ("model.shop.stg_orders", "/p/models/stg_orders.sql"),
            ("model.utils.stg_orders", "/p/dbt_packages/utils/models/stg.sql"),
        ];
        assert_eq!(resolved(named(None, "stg_orders", None)), found(&both), "in their packages, no source");
        assert_eq!(resolved(named(Some("utils"), "stg_orders", None)), found(&both[1..]), "the package named");
        assert_eq!(resolved(named(None, "dim", Some("1"))), found(&[("model.shop.dim.v1", "/p/models/dim_v1.sql")]));
        assert_eq!(resolved(named(None, "dim", Some(""))).map(|found| found.len()), Some(2), "a version no literal");
        let f = scan::Target::Function { package: None, name: "f".into() };
        assert_eq!(resolved(f), found(&[("function.shop.f", "/p/functions/f.sql")]), "by its id, the scan's file");
        let other = scan::Target::Function { package: Some("utils".into()), name: "f".into() };
        assert_eq!(resolved(other), Some(Vec::new()), "another package's function");
        assert_eq!(resolved(scan::Target::Macro { package: None, name: "is_positive".into() }), None, "a macro");
        let stg = call(named(None, "stg_orders", None));
        assert_eq!(oracle(&index(), &project(), "models/none.sql", &stg), None, "a file with no node");
    }

    #[test]
    fn a_target_shows_its_kind_package_name_and_version() {
        let versioned =
            call(scan::Target::Ref { package: Some("p".into()), name: "m".into(), version: Some("2".into()) });
        assert_eq!(target(&versioned), json!({"kind": "ref", "package": "p", "name": "m", "version": "2"}));
        let table = Target::Source(sources::Target::Table { source: "raw".into(), table: "orders".into() });
        assert_eq!(target(&table), json!({"kind": "source", "package": null, "name": "raw.orders", "version": null}));
    }

    fn record(compared: Compared) -> Record {
        Record {
            method: "references".into(),
            file: Some(PathBuf::from("/p/models/orders.sql")),
            kind: "ref",
            place: "sql",
            compared,
            target: json!({"name": "stg_orders"}),
            index: json!({"present": true}),
            oracle: None,
        }
    }

    #[test]
    fn a_line_holds_the_counts_and_at_most_twenty_locations_a_side_under_eight_kilobytes() {
        let long = format!("/p/{}", "deep/".repeat(40));
        let many =
            |side: &str| (0..3000).map(|n| at(&format!("{long}{side}{n}.sql"), (n, 0), (n, 9))).collect::<Vec<_>>();
        let compared = compare(&many("ours"), &many("dbt"));
        let line: Value = serde_json::from_str(&record(compared).line(Path::new("/p"))).unwrap();
        assert!(line.to_string().len() < LINE_MAX);
        assert_eq!(line["counts"], json!({"oursOnly": 3000, "dbtOnly": 3000, "both": 0}));
        assert_eq!(line["relation"], "disjoint");
        let shown = line["oursOnly"].as_array().unwrap();
        assert!(!shown.is_empty() && shown.len() <= SHOWN, "{}", shown.len());
        assert!(shown[0]["path"].as_str().unwrap().starts_with("deep/"), "relative to the root");
        let few = compare(&[at("/p/a.sql", (0, 0), (0, 0))], &[at("/p/b.sql", (1, 0), (1, 2))]);
        let line: Value = serde_json::from_str(&record(few).line(Path::new("/p"))).unwrap();
        assert_eq!(line["dbtOnly"], json!([{"path": "b.sql", "range": [[1, 0], [1, 2]]}]));
        assert_eq!(
            (&line["file"], &line["method"], &line["type"]),
            (&json!("models/orders.sql"), &json!("references"), &json!("request"))
        );
    }

    #[test]
    fn a_line_whose_names_alone_are_too_long_keeps_its_counts() {
        let mut long = record(compare(&[], &[]));
        long.target = json!({"name": "x".repeat(LINE_MAX)});
        let line: Value = serde_json::from_str(&long.line(Path::new("/p"))).unwrap();
        assert!(line.to_string().len() < LINE_MAX);
        assert_eq!((&line["target"], &line["relation"]), (&Value::Null, &json!("bothEmpty")));
    }

    #[test]
    fn the_counts_are_kept_by_method_kind_place_and_relation() {
        let mut counts = Counts::default();
        let equal = || compare(&[at("/p/a.sql", (0, 0), (0, 0))], &[at("/p/a.sql", (0, 0), (0, 0))]);
        counts.add(&record(equal()));
        counts.add(&record(equal()));
        counts.add(&record(compare(&[], &[])));
        let summary = counts.summary();
        assert_eq!(
            summary,
            json!({"requests": 3, "counts": {"references": {"ref": {"sql": {"equal": 2, "bothEmpty": 1}}}}})
        );
        let end: Value = serde_json::from_str(&counts.end()).unwrap();
        assert_eq!((&end["type"], &end["requests"], &end["counts"]), (&json!("end"), &json!(3), &summary["counts"]));
    }

    #[test]
    fn the_log_is_owner_only_and_renamed_once_it_reaches_a_megabyte() {
        let base = scratch_project("shadow-log", &[]);
        let log = Log::open(&base, Path::new("/some/root"));
        assert_eq!(log.path.parent().unwrap(), base.join("dbt-lsp-plus/shadow"));
        assert_ne!(log.path, Log::open(&base, Path::new("/another/root")).path, "one log per root");
        log.append("{\"a\": 1}").unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&log.path), mode(log.path.parent().unwrap())), (0o600, 0o700));
        let line = "x".repeat(1000);
        fs::write(&log.path, format!("{line}\n").repeat(1047)).unwrap();
        let before = fs::metadata(&log.path).unwrap().len();
        assert!(before < ROTATE_AT && before + 1001 >= ROTATE_AT, "{before}");
        (0..3).for_each(|_| log.append(&line).unwrap());
        let older = log.path.with_extension("jsonl.1");
        let size = |path: &Path| fs::metadata(path).unwrap().len();
        assert!(size(&older) >= ROTATE_AT && size(&older) <= ROTATE_AT + 1001, "{}", size(&older));
        assert_eq!(size(&log.path), 2002, "a new file begun");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn shadow_files_unwritten_for_thirty_days_are_removed_at_start() {
        let base = scratch_project(
            "shadow-sweep",
            &[("dbt-lsp-plus/shadow/old.jsonl", "{}\n"), ("dbt-lsp-plus/shadow/new.jsonl", "{}\n")],
        );
        let dir = base.join("dbt-lsp-plus/shadow");
        let month_ago = SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
        fs::File::options().write(true).open(dir.join("old.jsonl")).unwrap().set_modified(month_ago).unwrap();
        Log::open(&base, Path::new("/root"));
        assert!(!dir.join("old.jsonl").exists() && dir.join("new.jsonl").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_log_that_cannot_be_written_or_has_no_place_is_said_once() {
        let base = scratch_project("shadow-unwritable", &[]);
        let log = Log { path: base.join("missing/dir/x.jsonl") };
        let mut shadow = Shadow::new(Some(log));
        assert!(shadow.write("{}").is_some_and(|said| said.contains("cannot be written")));
        assert_eq!(shadow.write("{}"), None, "said once");
        let mut nowhere = Shadow::new(None);
        assert!(nowhere.write("{}").is_some_and(|said| said.contains("no place")), "no place, said");
        assert_eq!(nowhere.write("{}"), None, "once");
        assert!(!base.exists(), "nothing made");
    }
}
