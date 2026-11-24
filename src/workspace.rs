// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The core as the front ends see it, the model's Workspace: the session's
//! project (one, plan 049), its dbt, the open texts, the scan kept between
//! requests and its first fill, the last parse's index and the parse under
//! way, the call hierarchy's graph, and when the index is stale; and what
//! a request gets, found, empty, or unanswered with its reasons, from one
//! closed set whose words live here.  It speaks no protocol: no message, no
//! position, no URI.

use crate::graph::Graph;
use crate::project::{self, Project, Scanned};
use crate::{cache, config, locate, navigation, packages, reference, run, scan, text};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

/// The project's files that a thread reads before the first request needs
/// them, as `cache::fill` gives them: a name that clippy's type complexity
/// asks for, the field that holds them being three types deep.
type Filled = Vec<(PathBuf, cache::Key, Arc<Scanned>)>;

/// A place in a document: its path, its text as the front end holds it,
/// open or on disk, and a byte offset in it.
pub type Place = (PathBuf, String, usize);

#[derive(Debug, PartialEq)]
pub enum Which {
    Id(String),
    At(Place),
}

/// What a session sends for one request: the model's Answer.  `Empty` is
/// "nothing to look up": the request's position names no target.  What a
/// lookup gave is `Found`, its nothing included: a target that resolves to
/// nothing is a definition's null, references' `[]`, as the handlers
/// answer; nodeInfo by an id that names no node is `Empty`.  LSP renders
/// both alike; a rendering that tells them apart reads only `Empty` as
/// nothing to look up.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// A result, an empty one included.
    Found(Value),
    /// The method's default nothing, `capability::empty`: null, or `[]` for
    /// a list, where there is nothing to look up.
    Empty,
    /// The server could not answer, for these reasons.
    Unanswered(Vec<Reason>),
}

/// Why the server could not answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Reason {
    /// The root's `dbt_project.yml` does not read as a project.
    Unreadable(PathBuf),
    /// No index answers yet: why, as the parse stands.
    Unindexed(String),
    /// A rename refused, as `sources` and `document` write it.
    Refused(String),
    /// The request's document lies in the dbt project at PROJECT, not in
    /// the one this session serves, at SERVED.
    Elsewhere { document: PathBuf, project: PathBuf, served: PathBuf },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Unreadable(root) => write!(
                f,
                "no dbt project at {}: its dbt_project.yml is missing, is not YAML, or names no project",
                root.display()
            ),
            Reason::Unindexed(why) => write!(f, "dbt parse has not finished: {why}"),
            Reason::Refused(why) => f.write_str(why),
            Reason::Elsewhere { document, project, served } => write!(
                f,
                "{} lies in the dbt project at {}, and this session serves only the one at {}",
                document.display(),
                project.display(),
                served.display()
            ),
        }
    }
}

impl Answer {
    /// A rename's answer: its edit, or its refusal as the reason.
    pub fn of_rename(edit: Result<Value, String>) -> Answer {
        edit.map_or_else(|why| Answer::Unanswered(vec![Reason::Refused(why)]), Answer::Found)
    }
}

/// The core's state of the session's project.  A front end owes it: `epoch`
/// bumped at each of its events, the texts of its open documents in `open`,
/// `fresh` set false at a change told, `parse` asked to look on behalf of a
/// request, each outcome a parse reports handed to `parsed`, and `stop`
/// called last, once the runs that write in its cache have stopped.  Only the Workspace writes `index`,
/// `failure` and `running`.
#[derive(Default)]
pub struct Workspace {
    /// The project directory: the server command's `--project-dir`, spelt
    /// as the client spells it when the client names the same directory;
    /// else the client's folder, when it holds a project; else the project
    /// of the first document opened in one; none before.
    pub root: Option<PathBuf>,
    /// The user's dbt, from which every run of dbt is made: the server
    /// command, and where dbt's processes run.
    pub dbt: config::Dbt,
    /// The front end's count of its events: the scan looks at the files once an epoch.
    pub epoch: u64,
    /// The documents the front end has open, by path: the text of each,
    /// which wins over its file.
    pub open: HashMap<PathBuf, String>,
    /// The last parse's index: `parsed` alone writes it, as it moves the
    /// graph's generation with it.
    pub index: Option<config::ParseIndex>,
    /// Whether no change was told, nor found on disk by a `parse` that
    /// looked, since the last parse started.
    pub fresh: bool,
    /// Why the last parse failed, or none could start, as logged.
    pub failure: Option<String>,
    /// The parse under way: `parse` alone starts it, and `parsed` or `stop`
    /// ends it; dropped instead, dbt would run on to its deadline.
    pub running: Option<run::Running>,
    /// The scan of the project, kept between requests, and refreshed once
    /// an epoch: one a request or event.
    cache: RefCell<cache::Cache>,
    /// The first fill of the cache, under way on a thread, taken once it is
    /// done: nothing waits for it, and a request that comes before reads
    /// the files itself.
    filling: RefCell<Option<Receiver<Filled>>>,
    /// What the graph was made of, the project's generation
    /// (`Cache::generation`) then the index's (`generation`), and the graph.
    graph: RefCell<Option<(u64, u64, Rc<Graph>)>>,
    /// How many times the index changed: the graph alone holds while this stays.
    generation: u64,
    cache_dir: Option<PathBuf>,
    /// The stamp of the files on disk (`cache::disk`) when the last parse started; none before any.
    stamp: Option<u64>,
}

impl Workspace {
    /// The project as its files and the open documents say, if there is
    /// one: the cache's, looked at once an epoch.
    pub fn project(&self) -> Option<Rc<Project>> {
        let mut cache = self.cache.borrow_mut();
        let filling = self.filling.take();
        match filling.as_ref().map(Receiver::try_recv) {
            Some(Ok(filled)) => cache.adopt(filled),
            Some(Err(TryRecvError::Empty)) => drop(self.filling.replace(filling)),
            Some(Err(TryRecvError::Disconnected)) | None => {}
        }
        cache.project(self.root.as_deref()?, self.epoch, &self.open, self.dbt.command())
    }

    /// The text of the document at PATH: open, else on disk.
    pub fn text(&self, path: &Path) -> Option<String> {
        self.open.get(path).cloned().or_else(|| text::read(path))
    }

    /// The first scan of the project, read on a thread meanwhile, so that
    /// the first request needs none.
    pub fn warm(&self) {
        if let Some(root) = self.root.clone() {
            let ((filled, filling), server) = (mpsc::channel(), self.dbt.command().to_vec());
            thread::spawn(move || drop(filled.send(cache::fill(&root, &server))));
            self.filling.replace(Some(filling));
        }
    }

    /// The graph of PROJECT with the last parse's nodes, made again only
    /// when the project or the index changed, as the call hierarchy asks for
    /// it at each step.  Its calls are looked up with the last parse's index.
    pub fn graph(&self, project: &Project) -> Rc<Graph> {
        let made_of = (self.cache.borrow().generation, self.generation);
        if let Some((.., graph)) =
            self.graph.borrow().as_ref().filter(|(project, index, _)| (*project, *index) == made_of)
        {
            return graph.clone();
        }
        let scanned = Graph::scanned(project, self.index.as_ref());
        let graph = Rc::new(match &self.index {
            Some(index) => scanned.with_index(index, project),
            None => scanned,
        });
        *self.graph.borrow_mut() = Some((made_of.0, made_of.1, graph.clone()));
        graph
    }

    pub fn unindexed(&self) -> Reason {
        let why = match (self.running.is_some(), &self.failure) {
            (true, _) => "it is running",
            (false, Some(failure)) => failure,
            (false, None) => "it has not run, or left no manifest",
        };
        Reason::Unindexed(why.to_owned())
    }

    /// Whether PLACE names a node, and which node of the index: that of the
    /// `ref` or `source` there, or of the entry whose `name:` is there, as
    /// `navigation::Lookup::node` finds it, else, when FILE, the one node its
    /// document defines, but never in a YAML file: there the manifest places
    /// the generic tests a file declares beside its node, a YAML snapshot's
    /// included, so neither the scan nor `of_file` can name one node.  None
    /// for no PLACE, or when it names nothing; `Some(None)` when it names a
    /// node that the lookup does not find: without an index, any that the
    /// scan tells is named but cannot give (a `ref`, a `source`, an entry's
    /// `name:`, a file's one node); with one, a node the index lacks, or a
    /// `ref`, a `source` or an entry's `name:` over a project that does not
    /// read, which `Workspace::node` tells from a node the index lacks by
    /// asking for the project again.
    pub fn node_at(&self, place: Option<&Place>, file: bool) -> Option<Option<&Value>> {
        let (path, text, offset) = place?;
        let index = self.index.as_ref();
        let scanned = Scanned::of(path, text.clone());
        let yaml = path.extension().is_some_and(|extension| extension == "yml" || extension == "yaml");
        // The calls of `sources::file_calls`, which leave out the function calls that a YAML file's scan holds too.
        let inside = |call: &&scan::Call| {
            call.arguments.iter().any(|range| range.start <= *offset && *offset <= range.end)
                && !(yaml && call.name.kind == reference::Kind::Function)
        };
        if let Some(call) = scanned.calls.iter().find(inside) {
            let named = matches!(call.name.kind, reference::Kind::Ref | reference::Kind::Table { .. });
            // Without an index there is no node to find: the project is not read for none.
            return named.then(|| {
                let project = index.and_then(|_| self.project())?;
                navigation::Lookup::new(&project, index).node(path, &call.name)
            });
        }
        // An entry's `name:` stands for its node, looked up as dbt 2.0.9 does.
        let entry = navigation::reference_at(path, &scanned, *offset).filter(|name| {
            matches!(
                name.kind,
                reference::Kind::Entry { label: "model" | "seed" | "snapshot" | "analysis" }
                    | reference::Kind::Table { .. }
            )
        });
        if let Some(name) = entry {
            let Some(project) = self.project() else { return Some(None) };
            return Some(navigation::Lookup::new(&project, index).node(path, &name));
        }
        let relative = path.strip_prefix(self.root.as_ref()?).ok().filter(|_| file && !yaml)?;
        if let Some(index) = index {
            return index.of_file(&relative.to_string_lossy()).map(Some);
        }
        // As `ParseIndex::of_file` reads the manifest: the kinds of resources it holds as nodes.
        let project = self.project()?;
        let defines = |entity: &&project::Entity| {
            entity.path == *path
                && entity.package == project.root_package
                && matches!(entity.resource_type, "model" | "seed" | "snapshot" | "analysis" | "test")
        };
        (project.entities.iter().filter(defines).count() == 1).then_some(None)
    }

    /// The node that WHICH names, as nodeInfo, the subcommand `node` and MCP
    /// give it: the manifest's node with its summary and `children`; else
    /// why none: a place in another project's document, no index yet, or a
    /// project that does not read where the place names a node; `Empty` for
    /// no such node.
    pub fn node(&self, which: &Which) -> Answer {
        if let (Which::At((document, ..)), Some(root)) = (which, &self.root)
            && let Some(project) = locate::elsewhere(document, root, self.dbt.command())
        {
            let elsewhere = Reason::Elsewhere { document: document.clone(), project, served: root.clone() };
            return Answer::Unanswered(vec![elsewhere]);
        }
        let Some(index) = &self.index else { return Answer::Unanswered(vec![self.unindexed()]) };
        let named = match which {
            Which::Id(id) => index.node(id).map(Some),
            Which::At(place) => self.node_at(Some(place), true),
        };
        let described = |node: &Value| {
            let mut described = node.clone();
            described["summary"] = index.summary(node).into();
            described["children"] = index.children(node["unique_id"].as_str().unwrap_or_default()).into();
            described
        };
        named.flatten().map(described).map_or_else(|| self.why(named), Answer::Found)
    }

    /// Why a place that NAMED, as `node_at` gives it, gives no node: no
    /// index yet, where it names one; a project that does not read, where it
    /// names one the lookup does not find; else nothing to look up.
    pub fn why(&self, named: Option<Option<&Value>>) -> Answer {
        match (named, &self.index) {
            (Some(_), None) => Answer::Unanswered(vec![self.unindexed()]),
            (Some(None), Some(_)) if self.project().is_none() => Answer::Unanswered(vec![self.unreadable()]),
            _ => Answer::Empty,
        }
    }

    /// Why the core cannot answer from the project: the root's
    /// `dbt_project.yml` does not read, which the scan needs.
    pub fn unreadable(&self) -> Reason {
        Reason::Unreadable(self.root.clone().unwrap_or_default())
    }

    /// Starts a parse when the index is stale and dbt can parse: no parse
    /// under way, packages that dbt would not install, a server command with
    /// `lsp`; REPORT gets its outcome.  When LOOK, on behalf of a request,
    /// it looks at the files on disk first: one that may hold a resource and
    /// that changed since the last parse started makes the index stale, as
    /// a change told does; nothing to compare before a parse.  Why none
    /// could start, when it is not the last parse's failure already, for the
    /// front end to tell once.
    pub fn parse(&mut self, look: bool, report: config::ReportFn) -> Option<String> {
        if let (true, true, Some(root), Some(stamp)) = (look, self.fresh, &self.root, self.stamp) {
            self.fresh = cache::disk(root, self.dbt.command()) == stamp;
        }
        let root = self.root.clone().filter(|_| !self.fresh && self.running.is_none())?;
        if let Some(why) = packages::refused(&root, self.dbt.command()) {
            return self.failed(Some(why));
        }
        let cache = self.cache_dir(&root);
        let parse = config::PARSE.map(str::to_owned);
        let Some(run) = self.dbt.run(&parse, &root, cache, config::PARSE_LOG) else {
            return self.failed(Some("the server command has no lsp, so dbt-lsp-plus runs no dbt parse".into()));
        };
        (self.fresh, self.stamp) = (true, Some(cache::disk(&root, self.dbt.command())));
        self.running = Some(config::start(run, report));
        None
    }

    /// A parse's end, its INDEX and its FAILURE: its run stopped; its index
    /// replaces the last, or, from a parse that failed part of the way, lays
    /// its entries over the last; its failure kept as `failed` keeps it, and
    /// cleared by a parse that did not fail.
    pub fn parsed(&mut self, index: Option<config::ParseIndex>, failure: Option<String>) -> Option<String> {
        if let Some(running) = self.running.take() {
            running.stop();
        }
        match (index, self.index.as_mut()) {
            (Some(index), Some(last)) if failure.is_some() => {
                last.overlay(index);
                self.generation += 1;
            }
            (Some(index), _) => {
                self.index = Some(index);
                self.generation += 1;
            }
            (None, _) => {}
        }
        self.failed(failure.map(|failure| format!("dbt parse failed: {failure}")))
    }

    /// Keeps WHY as the last parse's failure, none to clear it; WHY again
    /// when it is not the last, for the front end to tell once.
    fn failed(&mut self, why: Option<String>) -> Option<String> {
        let new = why != self.failure;
        self.failure = why;
        self.failure.clone().filter(|_| new)
    }

    /// Stops the parse under way and removes this process's cache, `run/`
    /// with it: its owner calls it at the latest when it ends, once the runs
    /// that write there have stopped.
    pub fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop();
        }
        if let Some(cache) = self.cache_dir.take() {
            let _ = fs::remove_dir_all(cache);
        }
    }

    /// This process's cache of ROOT, under the user's, made at its first
    /// use, the caches beside it that no parse wrote for a day removed then.
    pub fn cache_dir(&mut self, root: &Path) -> PathBuf {
        let server = self.dbt.command();
        let cache = self.cache_dir.get_or_insert_with(|| {
            let cache = config::cache_dir(&config::cache_base(), root, server);
            config::remove_stale(&cache);
            cache
        });
        cache.clone()
    }
}

/// The node and the error that a front end with fields gives (the
/// subcommand `node`, MCP): ANSWER's node and FAILURE, the last parse's;
/// for `Empty`, FAILURE, else `<ASKED> names no node`; for `Unanswered`,
/// its reasons joined by `; `, FAILURE in place of `Unindexed`.
pub fn reply(answer: Answer, failure: Option<&str>, asked: &str) -> (Value, Option<String>) {
    match answer {
        Answer::Found(node) => (node, failure.map(str::to_owned)),
        Answer::Empty => (Value::Null, Some(failure.map_or_else(|| format!("{asked} names no node"), str::to_owned))),
        Answer::Unanswered(reasons) => {
            let said = |reason: &Reason| match (reason, failure) {
                (Reason::Unindexed(_), Some(failure)) => failure.to_owned(),
                _ => reason.to_string(),
            };
            (Value::Null, Some(reasons.iter().map(said).collect::<Vec<_>>().join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn a_node_by_its_id_or_a_place_with_its_children_or_why_none() {
        let (b_sql, a_yml) = ("select * from {{ ref('a') }}", "version: 2\nmodels:\n  - name: a\n");
        let root = project::scratch_project(
            "workspace-node",
            &[
                ("dbt_project.yml", "name: shop\n"),
                ("models/a.sql", "select 1"),
                ("models/b.sql", b_sql),
                ("models/_a.yml", a_yml),
            ],
        );
        let other = project::scratch_project(
            "workspace-node-other",
            &[("dbt_project.yml", "name: other\n"), ("models/m.sql", b_sql)],
        );
        let mut workspace = Workspace { root: Some(root.clone()), ..Workspace::default() };
        let id = |id: &str| Which::Id(id.to_owned());
        let at = |path: PathBuf, offset: usize| Which::At((path.clone(), fs::read_to_string(&path).unwrap(), offset));
        let (in_ref, in_name) = (b_sql.find("'a'").unwrap() + 1, a_yml.find("name: a").unwrap() + 6);
        let m = other.join("models/m.sql");
        let elsewhere = Reason::Elsewhere { document: m.clone(), project: other.clone(), served: root.clone() };
        // Each step, what the query gave, as a node's id, else as the Answer.
        let mut seen = Vec::new();
        let mut look = |step: &str, workspace: &Workspace, which: Which| {
            let answer = match workspace.node(&which) {
                Answer::Found(node) => Answer::Found(node["unique_id"].clone()),
                other => other,
            };
            seen.push((step.to_owned(), answer));
        };
        look("an id before an index", &workspace, id("model.shop.a"));
        look("another project's document before an index", &workspace, at(m.clone(), in_ref));
        let node = |name: &str, depends_on: &[&str]| {
            json!({"unique_id": format!("model.shop.{name}"), "name": name, "resource_type": "model",
                   "package_name": "shop", "original_file_path": format!("models/{name}.sql"),
                   "depends_on": {"nodes": depends_on}, "config": {}})
        };
        let manifest = json!({"metadata": {"project_name": "shop"},
                              "nodes": {"model.shop.a": node("a", &[]), "model.shop.b": node("b", &["model.shop.a"])}});
        workspace.parsed(Some(config::ParseIndex::of(&manifest)), None);
        look("another project's document", &workspace, at(m, in_ref));
        look("a ref's argument", &workspace, at(root.join("models/b.sql"), in_ref));
        look("a file's start", &workspace, at(root.join("models/a.sql"), 0));
        look("an entry's name", &workspace, at(root.join("models/_a.yml"), in_name));
        look("no such id", &workspace, id("model.shop.x"));
        let (a_node, b_node) = (workspace.node(&id("model.shop.a")), workspace.node(&id("model.shop.b")));
        fs::write(root.join("dbt_project.yml"), "name: [").unwrap();
        workspace.epoch += 1;
        look("an entry's name, unreadable", &workspace, at(root.join("models/_a.yml"), in_name));
        let a = Answer::Found(json!("model.shop.a"));
        let expected = [
            ("an id before an index", Answer::Unanswered(vec![workspace.unindexed()])),
            ("another project's document before an index", Answer::Unanswered(vec![elsewhere.clone()])),
            ("another project's document", Answer::Unanswered(vec![elsewhere.clone()])),
            ("a ref's argument", a.clone()),
            ("a file's start", a.clone()),
            ("an entry's name", a),
            ("no such id", Answer::Empty),
            ("an entry's name, unreadable", Answer::Unanswered(vec![Reason::Unreadable(root.clone())])),
        ];
        assert_eq!(seen, expected.map(|(step, answer)| (step.to_owned(), answer)));
        // a's children and summary; b, which none depends on, has `children` all the same.
        let field = |answer: &Answer, key: &str| match answer {
            Answer::Found(node) => node[key].clone(),
            _ => Value::Null,
        };
        let described = (field(&a_node, "children"), field(&a_node, "summary").is_string(), field(&b_node, "children"));
        assert_eq!(described, (json!(["model.shop.b"]), true, json!([])), "{a_node:?} {b_node:?}");
        // What a front end with fields gives of each.
        let (n, failure) = (json!({"unique_id": "model.shop.a"}), "dbt parse failed: x");
        let replies = [
            reply(Answer::Found(n.clone()), Some(failure), "model.shop.a"),
            reply(Answer::Empty, None, "model.shop.x"),
            reply(Answer::Empty, Some(failure), "model.shop.x"),
            reply(Answer::Unanswered(vec![Reason::Unindexed("it is running".into())]), Some(failure), "a"),
            reply(Answer::Unanswered(vec![elsewhere.clone()]), Some(failure), "a"),
        ];
        let said = [
            (n, Some(failure.to_owned())),
            (Value::Null, Some("model.shop.x names no node".to_owned())),
            (Value::Null, Some(failure.to_owned())),
            (Value::Null, Some(failure.to_owned())),
            (Value::Null, Some(elsewhere.to_string())),
        ];
        assert_eq!(replies, said);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn why_a_place_names_a_node_it_cannot_give() {
        let root = project::scratch_project("workspace-why", &[("dbt_project.yml", "name: shop\n")]);
        let mut workspace = Workspace { root: Some(root.clone()), ..Workspace::default() };
        let node = json!({"unique_id": "model.shop.a"});
        let unindexed = Answer::Unanswered(vec![workspace.unindexed()]);
        let before = [workspace.why(Some(None)), workspace.why(Some(Some(&node))), workspace.why(None)];
        assert_eq!(before, [unindexed.clone(), unindexed, Answer::Empty], "before any index");
        workspace.parsed(Some(config::ParseIndex::of(&json!({"metadata": {"project_name": "shop"}}))), None);
        assert_eq!(
            [workspace.why(Some(None)), workspace.why(None)],
            [Answer::Empty, Answer::Empty],
            "a project that reads"
        );
        fs::remove_file(root.join("dbt_project.yml")).unwrap();
        workspace.epoch += 1;
        let unreadable = Answer::Unanswered(vec![Reason::Unreadable(root.clone())]);
        assert_eq!(
            [workspace.why(Some(None)), workspace.why(None)],
            [unreadable, Answer::Empty],
            "dbt_project.yml gone"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_workspace_reads_its_project_and_parses_when_stale_with_no_front_end() {
        let root =
            project::scratch_project("workspace", &[("dbt_project.yml", "name: shop\n"), ("models/a.sql", "select 1")]);
        // Not the user's cache.
        let cache_dir = project::scratch_project("workspace-cache", &[]);
        let mut workspace = Workspace { root: Some(root.clone()), cache_dir: Some(cache_dir), ..Workspace::default() };
        let project = workspace.project().unwrap();
        let graph = workspace.graph(&project);
        let held = project.files.contains_key(&root.join("models/a.sql")) && graph.items.contains_key("model.shop.a");
        assert!(held, "the project's file and the graph's item");
        let (sender, outcomes) = mpsc::channel();
        let report = || -> config::ReportFn {
            let sender = sender.clone();
            Box::new(move |outcome| drop(sender.send(outcome)))
        };
        // Each step, what it gave, then whether a parse runs, whether the index is fresh, and the failure kept.
        let mut seen = Vec::new();
        let mut look = |step: &str, given: Option<String>, workspace: &Workspace| {
            seen.push((
                step.to_owned(),
                given,
                workspace.running.is_some(),
                workspace.fresh,
                workspace.failure.clone(),
            ));
        };
        // Before any parse, nothing to compare the disk with.
        let mut before = Workspace { root: Some(root.clone()), fresh: true, ..Workspace::default() };
        look("before any parse", before.parse(true, report()), &before);
        // A server command with no `lsp`: no parse, and why, once.
        workspace.dbt = config::Dbt::new(vec!["dbt".into()], None);
        look("no lsp", workspace.parse(false, report()), &workspace);
        look("no lsp again", workspace.parse(false, report()), &workspace);
        // With one, one parse at a time.
        workspace.dbt = config::Dbt::new(vec!["true".into(), "lsp".into()], None);
        look("started", workspace.parse(false, report()), &workspace);
        look("nothing written", workspace.parse(true, report()), &workspace);
        let outcome = outcomes.recv_timeout(Duration::from_secs(10)).expect("the parse's outcome");
        look("parsed", workspace.parsed(outcome.index, outcome.failure), &workspace);
        look("fresh", workspace.parse(true, report()), &workspace);
        let parsed = workspace.index.is_none() && outcomes.try_recv().is_err();
        // A file written on disk, with no change told: unseen until a look, then stale, and parsed again, one
        // parse at a time.
        fs::write(root.join("models/b.sql"), "select 2").unwrap();
        look("b written, no look", workspace.parse(false, report()), &workspace);
        look("b written", workspace.parse(true, report()), &workspace);
        fs::write(root.join("models/c.sql"), "select 3").unwrap();
        look("c written", workspace.parse(true, report()), &workspace);
        let no_lsp = Some("the server command has no lsp, so dbt-lsp-plus runs no dbt parse".to_owned());
        let expected = [
            ("before any parse", None, false, true, None),
            ("no lsp", no_lsp.clone(), false, false, no_lsp.clone()),
            ("no lsp again", None, false, false, no_lsp.clone()),
            ("started", None, true, true, no_lsp.clone()),
            ("nothing written", None, true, true, no_lsp.clone()),
            ("parsed", None, false, true, None),
            ("fresh", None, false, true, None),
            ("b written, no look", None, false, true, None),
            ("b written", None, true, true, None),
            ("c written", None, true, false, None),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(step, given, running, fresh, failure)| (step.to_owned(), given, running, fresh, failure))
            .collect();
        assert_eq!(seen, expected);
        assert!(parsed, "an index with no manifest, or a second outcome");
        workspace.stop();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_fill_and_the_stamps_follow_the_server_s_install_path() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/a.sql", "select 1"),
            ("vendor/extra/dbt_project.yml", "name: extra\n"),
            ("vendor/extra/models/x.sql", "select 2"),
        ];
        let root = project::scratch_project("workspace-install-path", &files);
        let cache_dir = project::scratch_project("workspace-install-path-cache", &[]);
        let server = ["true", "lsp", "--packages-install-path", "vendor"].map(str::to_owned).to_vec();
        let dbt = config::Dbt::new(server, None);
        let mut workspace =
            Workspace { root: Some(root.clone()), dbt, cache_dir: Some(cache_dir), ..Workspace::default() };
        workspace.warm();
        let filled = workspace.filling.take().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(filled.iter().any(|(path, ..)| *path == root.join("vendor/extra/models/x.sql")), "the fill");
        let (sender, outcomes) = mpsc::channel();
        let report = || -> config::ReportFn {
            let sender = sender.clone();
            Box::new(move |outcome| drop(sender.send(outcome)))
        };
        assert_eq!(workspace.parse(false, report()), None);
        let outcome = outcomes.recv_timeout(Duration::from_secs(10)).expect("the parse's outcome");
        workspace.parsed(outcome.index, outcome.failure);
        // The stamp that the parse took and the look's, both under the install path, are one.
        workspace.parse(true, report());
        assert!(workspace.fresh && workspace.running.is_none(), "nothing written");
        fs::write(root.join("vendor/extra/models/y.sql"), "select 3").unwrap();
        workspace.parse(true, report());
        assert!(workspace.running.is_some(), "a package's model written: stale, parsed again");
        workspace.stop();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn each_reason_says_what_it_is() {
        let cases = [
            (
                Reason::Unreadable(PathBuf::from("/work/shop")),
                "no dbt project at /work/shop: its dbt_project.yml is missing, is not YAML, or names no project",
            ),
            (Reason::Unindexed("it is running".into()), "dbt parse has not finished: it is running"),
            (Reason::Refused("another CTE is named final".into()), "another CTE is named final"),
            (
                Reason::Elsewhere {
                    document: PathBuf::from("/r/b/models/m.sql"),
                    project: PathBuf::from("/r/b"),
                    served: PathBuf::from("/r/a"),
                },
                "/r/b/models/m.sql lies in the dbt project at /r/b, and this session serves only the one at /r/a",
            ),
        ];
        for (reason, said) in cases {
            assert_eq!(reason.to_string(), said, "{reason:?}");
        }
    }
}
