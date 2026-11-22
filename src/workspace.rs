// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The core as the front ends see it, the model's Workspace: the session's
//! project (one, plan 049), its dbt, the scan kept between requests and its
//! first fill, the last parse's index and the parse under way, the call
//! hierarchy's graph, and when the index is stale.  It speaks no protocol:
//! no message, no position, no URI.

use crate::answer::Reason;
use crate::graph::{Graph, ServerNode};
use crate::project::{self, Project};
use crate::{cache, config, packages, run};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

/// The project's files that a thread reads before the first request needs
/// them, as `cache::fill` gives them.
type Filled = Vec<(PathBuf, cache::Key, Arc<project::Scanned>)>;

/// The generations of the project, the server's nodes and the index a
/// graph was made of, and whether it took the index's edges, alone.
type MadeOf = (u64, u64, u64, bool);

/// The core's state of the session's project.  A front end owes it: `epoch`
/// bumped at each of its events, `fresh` set false at a change told,
/// `notice` called before the parse a request asks for, each outcome a parse
/// reports handed to `parsed`, and `stop` called last, once the runs that
/// write in its cache have stopped.  Only the Workspace writes `index`,
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
    /// Whether a parse runs cut off the network and from writing the project.
    pub cut: bool,
    /// The front end's count of its events: the scan looks at the files once an epoch.
    pub epoch: u64,
    /// The last parse's index: `parsed` alone writes it, as it moves the
    /// graph's generation with it.
    pub index: Option<config::ParseIndex>,
    /// Whether no change was told, nor found on disk by `notice`, since the last parse started.
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
    /// The graph, with the generations it was made of.
    graph: RefCell<Option<(MadeOf, Rc<Graph>)>>,
    /// How many times the index changed: the graph alone holds while this stays.
    generation: u64,
    cache_dir: Option<PathBuf>,
    /// The stamp of the files on disk (`cache::disk`) when the last parse started; none before any.
    stamp: Option<u64>,
}

impl Workspace {
    /// The project as its files and the open texts OPEN say, if there is
    /// one: the cache's, looked at once an epoch.
    pub fn project(&self, open: &cache::Open) -> Option<Rc<Project>> {
        let mut cache = self.cache.borrow_mut();
        let filling = self.filling.take();
        match filling.as_ref().map(Receiver::try_recv) {
            Some(Ok(filled)) => cache.adopt(filled),
            Some(Err(TryRecvError::Empty)) => drop(self.filling.replace(filling)),
            Some(Err(TryRecvError::Disconnected)) | None => {}
        }
        cache.project(self.root.as_deref()?, self.epoch, open)
    }

    /// The first scan of the project, read on a thread meanwhile, so that
    /// the first request needs none.
    pub fn warm(&self) {
        if let Some(root) = self.root.clone() {
            let (filled, filling) = mpsc::channel();
            thread::spawn(move || drop(filled.send(cache::fill(&root))));
            self.filling.replace(Some(filling));
        }
    }

    /// The graph of PROJECT with SERVER's nodes once listed, SERVER the
    /// generation of dbt lsp's nodes and its listing's nodes; with none,
    /// alone, the last parse's: made again only when the project, the nodes
    /// or the index changed, as the call hierarchy asks for it at each step.
    /// Its calls are looked up with the last parse's index.
    pub fn graph(&self, project: &Project, server: Option<(u64, Option<&[ServerNode]>)>) -> Rc<Graph> {
        let nodes_generation = server.map_or(0, |(generation, _)| generation);
        let made_of = (self.cache.borrow().generation, nodes_generation, self.generation, server.is_none());
        if let Some((_, graph)) = self.graph.borrow().as_ref().filter(|(was, _)| *was == made_of) {
            return graph.clone();
        }
        let scanned = Graph::scanned(project, self.index.as_ref());
        let parsed = self.index.as_ref().filter(|_| server.is_none()).map(config::ParseIndex::listed);
        let listed = server.and_then(|(_, nodes)| nodes);
        let graph = Rc::new(match parsed.as_deref().or(listed) {
            Some(nodes) => scanned.with_server(nodes, project),
            None => scanned,
        });
        *self.graph.borrow_mut() = Some((made_of, graph.clone()));
        graph
    }

    /// Why no index answers yet, as nodeInfo and a hover say it.
    pub fn unindexed(&self) -> Reason {
        let why = match (self.running.is_some(), &self.failure) {
            (true, _) => "it is running",
            (false, Some(failure)) => failure,
            (false, None) => "waiting for dbt lsp to compile the project",
        };
        Reason::Unindexed(why.to_owned())
    }

    /// Starts a parse when the index is stale and dbt can parse: no parse
    /// under way, packages that need no network, guarded as alone when
    /// GUARD, a server command with `lsp`; REPORT gets its outcome.  With no
    /// REPORT nothing starts.  Why none could start, when it is not the
    /// last parse's failure already, for the front end to tell once.
    pub fn parse(&mut self, guard: bool, report: Option<config::ReportFn>) -> Option<String> {
        let root = self.root.clone().filter(|_| !self.fresh && self.running.is_none())?;
        if let Some(why) = packages::refused(&root, self.dbt.command(), self.cut, guard) {
            return self.failed(Some(why));
        }
        let cache = self.cache_dir(&root);
        let parse = config::PARSE.map(str::to_owned);
        let Some(run) = self.dbt.run(&parse, &root, cache, config::PARSE_LOG, self.cut) else {
            return self.failed(Some("the server command has no lsp, so dbt-lsp-plus runs no dbt parse".into()));
        };
        let report = report?;
        (self.fresh, self.stamp) = (true, Some(cache::disk(&root)));
        self.running = Some(config::start(run, report));
        None
    }

    /// A look at the files on disk on behalf of a request: one that may hold
    /// a resource and that changed since the last parse started makes the
    /// index stale, as a change told does.  Nothing to compare before a parse.
    /// Whether it found such a change.
    pub fn notice(&mut self) -> bool {
        let (Some(root), Some(stamp), true) = (&self.root, self.stamp, self.fresh) else { return false };
        self.fresh = cache::disk(root) == stamp;
        !self.fresh
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
            let cache = config::cache_dir(&config::cache_base().unwrap_or_else(std::env::temp_dir), root, server);
            config::remove_stale(&cache);
            cache
        });
        cache.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn a_workspace_reads_its_project_and_parses_when_stale_with_no_front_end() {
        let root =
            project::scratch_project("workspace", &[("dbt_project.yml", "name: shop\n"), ("models/a.sql", "select 1")]);
        // Not the user's cache.
        let cache_dir = project::scratch_project("workspace-cache", &[]);
        let mut workspace = Workspace { root: Some(root.clone()), cache_dir: Some(cache_dir), ..Workspace::default() };
        let (texts, edits) = (HashMap::new(), HashMap::new());
        let project = workspace.project(&cache::Open { texts: &texts, edits: &edits }).unwrap();
        let graph = workspace.graph(&project, None);
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
        before.notice();
        look("before any parse", None, &before);
        // A server command with no `lsp`: no parse, and why, once.
        workspace.dbt = config::Dbt::new(vec!["dbt".into()], None);
        look("no lsp", workspace.parse(true, Some(report())), &workspace);
        look("no lsp again", workspace.parse(true, Some(report())), &workspace);
        // With one, nothing starts without a report, and one parse at a time with one.
        workspace.dbt = config::Dbt::new(vec!["true".into(), "lsp".into()], None);
        look("no report", workspace.parse(true, None), &workspace);
        look("started", workspace.parse(true, Some(report())), &workspace);
        workspace.notice();
        look("nothing written", None, &workspace);
        look("under way", workspace.parse(true, Some(report())), &workspace);
        let outcome = outcomes.recv_timeout(Duration::from_secs(10)).expect("the parse's outcome");
        look("parsed", workspace.parsed(outcome.index, outcome.failure), &workspace);
        look("fresh", workspace.parse(true, Some(report())), &workspace);
        let parsed = workspace.index.is_none() && outcomes.try_recv().is_err();
        // A file written on disk, with no change told: stale, and parsed again, one parse at a time.
        fs::write(root.join("models/b.sql"), "select 2").unwrap();
        workspace.notice();
        look("b written", None, &workspace);
        look("stale", workspace.parse(true, Some(report())), &workspace);
        fs::write(root.join("models/c.sql"), "select 3").unwrap();
        workspace.notice();
        look("c written", None, &workspace);
        look("stale under way", workspace.parse(true, Some(report())), &workspace);
        let no_lsp = Some("the server command has no lsp, so dbt-lsp-plus runs no dbt parse".to_owned());
        let expected = [
            ("before any parse", None, false, true, None),
            ("no lsp", no_lsp.clone(), false, false, no_lsp.clone()),
            ("no lsp again", None, false, false, no_lsp.clone()),
            ("no report", None, false, false, no_lsp.clone()),
            ("started", None, true, true, no_lsp.clone()),
            ("nothing written", None, true, true, no_lsp.clone()),
            ("under way", None, true, true, no_lsp.clone()),
            ("parsed", None, false, true, None),
            ("fresh", None, false, true, None),
            ("b written", None, false, false, None),
            ("stale", None, true, true, None),
            ("c written", None, true, false, None),
            ("stale under way", None, true, false, None),
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
}
