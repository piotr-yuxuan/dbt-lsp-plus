// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The language server: the session's event loop, which answers the
//! client's requests from the Workspace and tells it what dbt's runs left.

use crate::answer::{Answer, Reason};
use crate::capability;
use crate::commands;
use crate::locate;
use crate::project::{Project, Scanned};
use crate::rpc;
use crate::text::{self, Encoding, LineStarts};
use crate::workspace::{Which, Workspace};
use crate::{
    cache, config, document, features, navigation, packages, parse_log, properties, reference, run, runs, sources,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufReader, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime};
use url::Url;

/// The prefix of the ids of the requests that the server sends to the
/// client.
pub const OWN_ID: &str = "dbt-lsp-plus/";
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
/// The unit of every position the client and the server exchange: LSP's
/// default, which the server announces by announcing none.
const ENCODING: Encoding = Encoding::Utf16;

/// How the server answers a request of the client's.
enum Route {
    Here(&'static str),
    /// With this answer: null before a project, or the method's nothing.
    Answer(Answer),
    /// As this command for clients, by dbt's CLI.
    Command(&'static str),
    /// No method or command of the server's.
    Unknown,
    /// A command with no name.
    Invalid,
}

/// The client's notifications after which the graph may have changed.
const CHANGES: [&str; 5] = [
    "textDocument/didSave",
    "workspace/didChangeWatchedFiles",
    "workspace/didCreateFiles",
    "workspace/didRenameFiles",
    "workspace/didDeleteFiles",
];
/// The directories under the project where dbt writes its output and its
/// logs, which hold no resource.
const OUTPUT: [&str; 2] = ["target", "logs"];

enum Event {
    /// A message of the client's, null for a body that is not JSON.
    Client(Value),
    ClientClosed,
    Parsed(config::Outcome),
    /// The failures of one kind, `runs::LAST_RUN` or `runs::STALE_SOURCE`,
    /// when their file was written; none for results of a command that
    /// builds nothing.
    Runs(&'static str, Option<Vec<runs::Failure>>, SystemTime),
    /// The answer of a run of dbt's CLI to the client's request of that id.
    Ran(Value, Value),
}

/// What a watched file and the manifest give: its failures, or none to keep
/// those known.
type ReadFailures = fn(&Value, &Value) -> Option<Vec<runs::Failure>>;

/// How often the files of the last run are looked at.
const RUNS_EVERY: Duration = Duration::from_secs(1);

/// What the server knows of each file's diagnostics, by its path as the
/// root maps it.
#[derive(Default)]
struct Diagnostics {
    /// The failures of the last run.
    failures: HashMap<PathBuf, Vec<runs::Failure>>,
    /// The errors and warnings of the last parse that ran to its end.
    parsed: HashMap<PathBuf, Vec<parse_log::Problem>>,
    /// The messages of the problems in no file that were logged.
    logged: HashSet<String>,
    /// When the client last changed or saved each document.
    edited: HashMap<PathBuf, SystemTime>,
    /// The URI under which the server last published.
    published: HashMap<PathBuf, String>,
}

struct Proxy<W: Write> {
    client: W,
    /// Where the session's threads report: the client's reader, the parse,
    /// the runs and their watchers; none in the unit tests, which run
    /// nothing.
    events: Option<Sender<Event>>,
    /// The line that says which project and which dbt the server found,
    /// told to the client once it can be.
    line: Option<String>,
    /// The root with its links resolved, onto which the client's paths map.
    canonical_root: Option<PathBuf>,
    hierarchical: bool,
    /// Whether the client sent `initialize`, before which LSP lets the server send nothing.
    initialized: bool,
    documents: HashMap<PathBuf, String>,
    /// The URI under which the client opened each of them, which an edit
    /// of it must name.
    uris: HashMap<PathBuf, String>,
    /// Whether the client opened a document, the first of which prompts a parse.
    opened: bool,
    next_id: u64,
    shutdown: bool,
    /// Whether the client reads markdown in a hover.
    markdown: bool,
    /// Whether the client may be asked to refresh its inlay hints.
    hint_refresh: bool,
    /// Whether the client lets a server register a watcher of files.
    watches: bool,
    diagnostics: Diagnostics,
    /// The core's state of the session's project, which the server hands
    /// the open texts: its root, its dbt, its scan, its parse and index,
    /// its graph.
    workspace: Workspace,
    /// The count of each open document's edits, under which the cache keeps
    /// it, and the count so far.
    edits: HashMap<PathBuf, u64>,
    edited: u64,
    /// The runs of dbt's CLI that clients asked for, one at a time: those
    /// waiting, with the client's id and their run, and the one under way.
    runs: VecDeque<(Value, commands::Job, run::Run)>,
    run: Option<run::Running>,
}

/// PATH with each `..` dropping the component before it, as written: no
/// link is followed.
pub fn lexical(path: &Path) -> PathBuf {
    path.components().fold(PathBuf::new(), |mut path, component| {
        match component {
            Component::ParentDir => {
                path.pop();
            }
            other => path.push(other),
        }
        path
    })
}

fn uri_path(uri: Option<&str>) -> Option<PathBuf> {
    Url::parse(uri?).ok()?.to_file_path().ok()
}

/// Whether the client's notification METHOD, of PARAMS, may change the
/// graph: one of `CHANGES`, but a watched-files event whose every change is
/// a `file:` URI that MAP places in one of the directories OUTPUT gives,
/// dbt's output, which each of the user's runs rewrites.  An event the
/// server cannot read is a change.
fn changes_graph(
    method: &str,
    params: &Value,
    output: impl Fn() -> Vec<PathBuf>,
    map: impl Fn(Option<&str>) -> Option<PathBuf>,
) -> bool {
    if !CHANGES.contains(&method) {
        return false;
    }
    let changes = params["changes"].as_array().map_or(&[][..], Vec::as_slice);
    if method != "workspace/didChangeWatchedFiles" || changes.is_empty() {
        return true;
    }
    let output = output();
    let in_output = |change: &Value| {
        let uri = change["uri"].as_str().filter(|uri| Url::parse(uri).is_ok_and(|url| url.scheme() == "file"));
        map(uri).is_some_and(|path| output.iter().any(|dir| path.starts_with(dir)))
    };
    output.is_empty() || !changes.iter().all(in_output)
}

/// The `--project-dir` of COMMAND, as `--project-dir X` or
/// `--project-dir=X`, made absolute as spelt, so that the paths the server
/// returns read as the user wrote them: each `..` drops the component before
/// it.  Resolved only when that names another directory, through a link
/// before a `..`.
pub fn project_dir(command: &[String]) -> Option<PathBuf> {
    let value = config::flag(command, "--project-dir")?;
    let spelt = lexical(&std::path::absolute(value).ok()?);
    match fs::canonicalize(value) {
        Ok(resolved) if fs::canonicalize(&spelt).ok().as_ref() != Some(&resolved) => Some(resolved),
        _ => Some(spelt),
    }
}

impl<W: Write> Proxy<W> {
    fn new(client: W, root: Option<PathBuf>) -> Self {
        let mut proxy = Proxy {
            client,
            events: None,
            line: None,
            canonical_root: None,
            hierarchical: false,
            initialized: false,
            documents: HashMap::new(),
            uris: HashMap::new(),
            opened: false,
            next_id: 0,
            shutdown: false,
            markdown: false,
            hint_refresh: false,
            watches: false,
            diagnostics: Diagnostics::default(),
            workspace: Workspace::default(),
            edits: HashMap::new(),
            edited: 0,
            runs: VecDeque::new(),
            run: None,
        };
        if let Some(root) = root {
            proxy.set_root(root);
        }
        proxy
    }

    fn set_root(&mut self, root: PathBuf) {
        self.canonical_root = fs::canonicalize(&root).ok();
        self.workspace.root = Some(root);
    }

    fn send_client(&mut self, message: &Value) {
        // A client that stopped reading also closes its output: the loop ends there.
        let _ = rpc::write_value(&mut self.client, message);
    }

    fn reply_error(&mut self, id: &Value, code: i64, message: &str) {
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    /// Tells the client, once, which project and which dbt the server found,
    /// as one log line.
    fn say_found(&mut self) {
        if let Some(text) = self.line.take() {
            let params = json!({"type": 3, "message": text});
            self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": params}));
        }
    }

    /// Takes ROOT as the project, found from the client's folder or from a
    /// document: what the user wrote from the server's directory keeps naming
    /// the same once dbt runs in the project.
    fn take_project(&mut self, root: PathBuf) {
        let git = locate::Git { program: "git".into(), env: std::env::vars_os().collect() };
        // What the user wrote from the server's directory still names the same from the project.
        let server = std::env::current_dir().map(|cwd| config::anchored(self.workspace.dbt.command(), &cwd));
        let found = locate::dbt(&root, &server.unwrap_or_else(|_| self.workspace.dbt.command().to_vec()), &git);
        let skipped: String =
            found.skipped.iter().map(|(path, why)| format!("; skipped {} ({why})", path.display())).collect();
        let (root_shown, named, source) = (root.display(), &found.named, found.source);
        let line = format!("dbt-lsp-plus: project {root_shown}, dbt {named} ({source}){skipped}.");
        eprintln!("{line}");
        self.line = Some(line);
        self.workspace.dbt = config::Dbt::new(found.command, Some(root.clone()));
        self.set_root(root);
    }

    /// Answers the client's `initialize` of ID, learning from its PARAMS what
    /// the session keeps; without a project the server stays quiet until a
    /// document names one.
    fn on_initialize(&mut self, id: &Value, params: &Value) {
        self.hierarchical = params
            .pointer("/capabilities/textDocument/documentSymbol/hierarchicalDocumentSymbolSupport")
            == Some(&json!(true));
        let formats = params.pointer("/capabilities/textDocument/hover/contentFormat").and_then(Value::as_array);
        // The client lists its formats in its order of preference.
        let first =
            formats.and_then(|formats| formats.iter().find(|format| *format == "markdown" || *format == "plaintext"));
        self.markdown = first == Some(&json!("markdown"));
        self.hint_refresh = params.pointer("/capabilities/workspace/inlayHint/refreshSupport") == Some(&json!(true));
        let watched = "/capabilities/workspace/didChangeWatchedFiles/dynamicRegistration";
        self.watches = params.pointer(watched) == Some(&json!(true));
        let folder = params.pointer("/workspaceFolders/0/uri").or_else(|| params.get("rootUri"));
        let folder = uri_path(folder.and_then(Value::as_str)).filter(|folder| folder.join("dbt_project.yml").is_file());
        match (folder, &self.workspace.root) {
            (Some(folder), None) => self.take_project(folder),
            (Some(folder), Some(_)) if fs::canonicalize(&folder).ok() == self.canonical_root => self.set_root(folder),
            _ => {}
        }
        self.workspace.warm();
        self.initialized = true;
        self.watch_runs();
        let result = json!({"capabilities": capability::announced(),
                            "serverInfo": {"name": "dbt-lsp-plus", "version": env!("CARGO_PKG_VERSION")}});
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        self.say_found();
    }

    /// The path of a `file:` URI, mapped onto the root when it reaches the
    /// same file through another spelling of its directory: a link, a
    /// canonical path.  A path under the root as spelt stays as it is, links
    /// within the project included, since the scan walks them so.
    fn local_path(&self, uri: Option<&str>) -> Option<PathBuf> {
        let path = uri_path(uri)?;
        let mapped = || {
            let root = self.workspace.root.as_ref()?;
            if path.starts_with(root) {
                return None;
            }
            // The directory alone, so that a file not yet saved maps too.
            let canonical = fs::canonicalize(path.parent()?).ok()?.join(path.file_name()?);
            let rest = canonical.strip_prefix(self.canonical_root.as_ref()?).ok()?;
            Some(root.join(rest))
        };
        Some(mapped().unwrap_or(path))
    }

    /// Keeps the text of the open documents, for the outline of unsaved
    /// edits, and the URI each was opened under.
    fn track(&mut self, method: &str, params: &Value) {
        let uri = params.pointer("/textDocument/uri").and_then(Value::as_str);
        let Some(path) = self.local_path(uri) else { return };
        // A file changed since the last run: its failures may no longer hold.
        // Where the parse has problems, their ranges are of the text before
        // the edit, which the client has moved: its next publication drops
        // the failures instead.  An empty set has no range to move.
        if matches!(method, "textDocument/didChange" | "textDocument/didSave") {
            self.diagnostics.edited.insert(path.clone(), SystemTime::now());
            let ranged = self.diagnostics.parsed.get(&path).is_some_and(|problems| !problems.is_empty());
            if self.diagnostics.failures.remove(&path).is_some() && !ranged {
                self.publish(&path);
            }
        }
        match method {
            "textDocument/didOpen" => {
                let text = params.pointer("/textDocument/text").and_then(Value::as_str).unwrap_or_default();
                self.documents.insert(path.clone(), text.to_owned());
                self.edited += 1;
                self.edits.insert(path.clone(), self.edited);
                self.uris.insert(path, uri.unwrap_or_default().to_owned());
            }
            "textDocument/didChange" => {
                self.edited += 1;
                self.edits.insert(path.clone(), self.edited);
                let changes = params.get("contentChanges").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
                if let Some(text) = self.documents.get_mut(&path) {
                    changes.iter().for_each(|change| text::apply_change(text, change, ENCODING));
                }
            }
            "textDocument/didClose" => {
                // An unsaved text gives way to the file's.
                self.documents.remove(&path);
                self.edits.remove(&path);
                self.uris.remove(&path);
            }
            _ => {}
        }
    }

    /// The project as its files and the open documents say, if there is
    /// one: the Workspace's, looked at once an epoch.
    fn project(&self) -> Option<Rc<Project>> {
        self.workspace.project(&self.open_documents())
    }

    fn open_documents(&self) -> cache::Open<'_> {
        cache::Open { texts: &self.documents, edits: &self.edits }
    }

    /// The outline of a document, from its open text or its file; null for a
    /// document that is neither.
    fn outline(&self, params: &Value) -> Option<Value> {
        let uri = params.pointer("/textDocument/uri")?.as_str()?;
        let text = self.text_of(uri).map(|(_, text)| text);
        Some(text.map_or(Value::Null, |text| features::document_symbols(&text, ENCODING, self.hierarchical, uri)))
    }

    /// The path of the document at URI and its text, open or on disk.
    fn text_of(&self, uri: &str) -> Option<(PathBuf, String)> {
        let path = self.local_path(Some(uri))?;
        let text = self.documents.get(&path).cloned().or_else(|| text::read(&path))?;
        Some((path, text))
    }

    /// The answer to the call hierarchy's METHOD, on the graph of the files
    /// with the last parse's nodes.
    fn hierarchy(&self, method: &str, params: &Value) -> Option<Answer> {
        let Some(project) = self.project() else { return Some(Answer::Unanswered(vec![self.unreadable()])) };
        let graph = self.workspace.graph(&project);
        if method != "textDocument/prepareCallHierarchy" {
            let id = params.pointer("/item/data/uniqueId")?.as_str()?;
            let incoming = method == "callHierarchy/incomingCalls";
            return Some(Answer::Found(features::calls(&project, &graph, id, incoming, ENCODING)));
        }
        // A document that is no file, such as an unsaved buffer, holds no resource.
        let Some(path) = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str)) else {
            return Some(Answer::Empty);
        };
        let text = project.text(&path).unwrap_or_default();
        let offset = LineStarts::new(text).offset_of(params.get("position")?, ENCODING)?;
        let prepared = features::prepare_call_hierarchy(&project, &graph, &path, offset, ENCODING);
        Some(prepared.map_or(Answer::Empty, Answer::Found))
    }

    /// The document of a request, its text, open or on disk, and the offset
    /// of the request's position in it.
    fn document_at(&self, params: &Value) -> Option<(PathBuf, String, usize)> {
        let (path, text) = self.text_of(params.pointer("/textDocument/uri")?.as_str()?)?;
        let offset = LineStarts::new(&text).offset_of(params.get("position")?, ENCODING)?;
        Some((path, text, offset))
    }

    /// The document of a request, its scan and the offset of the request's
    /// position in it: PROJECT's scan of the file where PROJECT holds it
    /// with the same text, else a scan of its text, open or on disk.
    fn scan_at(&self, params: &Value, project: Option<&Project>) -> Option<(PathBuf, Arc<Scanned>, usize)> {
        let (path, text, offset) = self.document_at(params)?;
        let held = project.and_then(|project| project.files.get(&path)).filter(|held| held.text == text);
        let scanned = held.cloned().unwrap_or_else(|| Arc::new(Scanned::of(&path, text)));
        Some((path, scanned, offset))
    }

    /// The source named at the position of a request on a document, with
    /// the project scanned.  Only the scan holds every occurrence: on a
    /// document it does not hold, such as a file not saved yet or one
    /// outside the resource paths, there is none.
    fn source_at(&self, params: &Value) -> Option<(Rc<Project>, reference::Name)> {
        let (path, scanned, offset) = self.scan_at(params, None)?;
        let name = sources::name_at(&scanned, offset)?;
        let project = self.project().filter(|project| project.files.contains_key(&path))?;
        Some((project, name))
    }

    /// How the server answers the client's request METHOD, by the row of
    /// `CAPABILITIES` it names: a name with no row gets -32601 (`shutdown`
    /// aside, null), `workspace/executeCommand` with no string `command`
    /// -32602; a row's request before a project gets null, a valid result
    /// of every method, and after, its handler's answer.
    fn route(&self, method: &str, params: &Value) -> Route {
        let executes = method == "workspace/executeCommand";
        let name = if executes { params["command"].as_str() } else { Some(method) };
        let Some(name) = name else { return Route::Invalid };
        let row = capability::of(name).filter(|row| row.is_command() == executes);
        match (row.map(|row| row.name), self.workspace.root.is_some()) {
            (None, _) if method == "shutdown" => Route::Answer(Answer::Empty),
            (None, _) => Route::Unknown,
            // A valid result of every method the server announces, and of each an agent was seen to send.
            (Some(_), false) => Route::Answer(Answer::Found(Value::Null)),
            (Some(command @ (commands::COMPILE | commands::NODES)), true) => Route::Command(command),
            (Some(name), true) => Route::Here(name),
        }
    }

    /// The definition, or the REFERENCES, of the target at the request's
    /// position, by dbt 2.0.9's lookup, found, its nothing included; empty
    /// where the document or the position does not read, or names no
    /// target.  The two share the reading of the position and of the
    /// project.
    fn on_navigation(&self, references: bool, params: &Value) -> Option<Answer> {
        let project = self.project();
        let Some((path, scanned, at)) = self.scan_at(params, project.as_deref()) else { return Some(Answer::Empty) };
        let Some(name) = navigation::reference_at(&path, &scanned, at) else { return Some(Answer::Empty) };
        let Some(project) = project else { return Some(Answer::Unanswered(vec![self.unreadable()])) };
        let lookup = || navigation::Lookup::new(&project, self.workspace.index.as_ref());
        if !references {
            let locations = self.locations(&project, &navigation::resolve(&lookup(), &path, &name));
            return Some(Answer::Found(match &locations[..] {
                [] => Value::Null,
                [location] => location.clone(),
                _ => Value::Array(locations),
            }));
        }
        let declaration = params.pointer("/context/includeDeclaration").and_then(Value::as_bool) == Some(true);
        if name.source_table().is_some() {
            let uri_of = |path: &Path| self.uri_of(path);
            return Some(Answer::Found(sources::references(&project, &name, declaration, ENCODING, &uri_of)));
        }
        let found = navigation::references(&lookup(), &path, &name, declaration);
        Some(Answer::Found(Value::Array(self.locations(&project, &found))))
    }

    /// Why the server cannot answer from the project: the root's
    /// `dbt_project.yml` does not read, which the scan needs.
    fn unreadable(&self) -> Reason {
        Reason::Unreadable(self.workspace.root.clone().unwrap_or_default())
    }

    /// The URI of the file at PATH, as the client spelt it if it opened it.
    fn uri_of(&self, path: &Path) -> String {
        self.uris.get(path).cloned().unwrap_or_else(|| features::uri(path))
    }

    /// The locations of FOUND in PROJECT, as the client spelt their URIs.
    fn locations(&self, project: &Project, found: &[navigation::Target]) -> Vec<Value> {
        let location = |found: &navigation::Target| {
            // Up to the range's end only: a macro's references may span thousands of files.
            let text = project.text(&found.path).unwrap_or_default();
            let starts = LineStarts::new(text.get(..found.range.end).unwrap_or(text));
            let range = starts.range(found.range.start, found.range.end, ENCODING);
            json!({"uri": self.uri_of(&found.path), "range": range})
        };
        found.iter().map(location).collect()
    }

    /// The rename of the source at the request's position, in a file the scan
    /// holds: a rename that left the asking file as it is would be wrong.
    fn on_source(&self, params: &Value) -> Option<Answer> {
        let Some((project, name)) = self.source_at(params) else { return Some(Answer::Empty) };
        let uri_of = |path: &Path| self.uri_of(path);
        let new = params.get("newName")?.as_str()?;
        Some(Answer::of_rename(sources::rename(&project, &name, new, ENCODING, &uri_of)))
    }

    /// The rename of the source, else of the CTE, at the request's position;
    /// empty where the document or the position does not read.
    fn on_rename(&self, params: &Value) -> Option<Answer> {
        if self.source_at(params).is_some() {
            return self.on_source(params);
        }
        let Some((_, text, offset)) = self.document_at(params) else { return Some(Answer::Empty) };
        let (uri, new) = (params.pointer("/textDocument/uri")?.as_str()?, params.get("newName")?.as_str()?);
        Some(document::rename_cte(&text, uri, offset, new, ENCODING).map_or(Answer::Empty, Answer::of_rename))
    }

    /// The folding ranges, highlights or selection ranges of a document;
    /// null for a document neither open nor on disk.
    fn on_document(&self, method: &str, params: &Value) -> Option<Value> {
        let uri = params.pointer("/textDocument/uri")?.as_str()?;
        let sql = uri.ends_with(".sql");
        if method == "textDocument/documentHighlight" {
            let Some((_, text, offset)) = self.document_at(params) else { return Some(Value::Null) };
            return Some(document::highlights(&text, sql, offset, ENCODING));
        }
        // A YAML file folds and selects best by its indentation, which the client knows.
        let Some((_, text)) = self.text_of(uri).filter(|_| sql || uri.ends_with(".md")) else {
            return Some(Value::Null);
        };
        if method == "textDocument/foldingRange" {
            return Some(document::folding_ranges(&text, sql));
        }
        let starts = LineStarts::new(&text);
        let positions = params.get("positions")?.as_array()?.iter().map(|p| starts.offset_of(p, ENCODING));
        Some(document::selection_ranges(&text, sql, &positions.collect::<Option<Vec<usize>>>()?, ENCODING))
    }

    /// The information of a node, named by its id or by a position, as
    /// `Workspace::node` gives it; why while no parse has finished, even
    /// for a position on no document; empty for a position on no document
    /// once one has; none for no argument.
    fn node_info(&self, params: &Value) -> Option<Answer> {
        let which = params.pointer("/arguments/0").map(|argument| match argument.as_str() {
            Some(id) => Some(Which::Id(id.to_owned())),
            None => {
                let position = json!({"textDocument": {"uri": argument["uri"]}, "position": argument["position"]});
                self.document_at(&position).map(Which::At)
            }
        });
        match (which, self.workspace.index.is_some()) {
            (Some(Some(which)), _) => Some(self.workspace.node(&self.open_documents(), &which)),
            (_, false) => Some(Answer::Unanswered(vec![self.workspace.unindexed()])),
            (Some(None), true) => Some(Answer::Empty),
            (None, true) => None,
        }
    }

    /// Lets the Workspace start a parse when the index is stale and dbt can
    /// parse, but after `shutdown`, when the session ends.  Its outcome comes
    /// back as an event; why none could start is logged.
    fn want_index(&mut self) {
        if self.shutdown {
            return;
        }
        let events = self.events.clone();
        let report = events
            .map(|events| -> config::ReportFn { Box::new(move |outcome| drop(events.send(Event::Parsed(outcome)))) });
        if let Some(why) = self.workspace.parse(report) {
            self.log_parse(&why);
        }
    }

    /// On behalf of a request: the Workspace looks at the files on disk
    /// first, then `want_index`.
    fn ask_index(&mut self) {
        self.workspace.notice();
        self.want_index();
    }

    /// A request of a command for clients, COMMAND with PARAMS: refused
    /// after `shutdown`, invalid unless its one argument is a string, a
    /// compile's URI or the selector of nodes; else run by dbt's CLI.
    fn on_command(&mut self, command: &'static str, id: &Value, params: &Value) {
        if self.shutdown {
            return self.reply_error(id, INVALID_REQUEST, "the client asked for shutdown");
        }
        let Some(argument) = params.pointer("/arguments/0").filter(|argument| argument.is_string()) else {
            return self.reply_error(id, INVALID_PARAMS, "invalid parameters");
        };
        self.run_command(command, id, argument);
    }

    /// Queues the run of COMMAND by dbt's CLI for the client's request ID
    /// with ARGUMENT; or answers why it cannot run: a file it cannot
    /// compile, packages that dbt would install, a compile where a package
    /// may have on-run hooks, a server command with no `lsp`.
    fn run_command(&mut self, command: &'static str, id: &Value, argument: &Value) {
        let root = self.workspace.root.clone().unwrap_or_default();
        let (job, hooks) = match command {
            commands::COMPILE => {
                let (cwd, path) = (self.dbt_dir().unwrap_or_default(), self.local_path(argument.as_str()));
                let job = commands::compile_job(path.as_deref(), &root, self.project().as_deref(), cwd);
                (job, commands::hooked(&root, self.workspace.dbt.command()))
            }
            _ => (Ok(commands::nodes_job(argument.as_str().unwrap_or_default())), None),
        };
        let refused = packages::refused(&root, self.workspace.dbt.command()).or(hooks);
        let dir = self.workspace.cache_dir(&root).join("run");
        let run = job.as_ref().ok().and_then(|job| self.workspace.dbt.run(&job.words, &root, dir, config::RUN_LOG));
        let failure = match (job, refused, run) {
            (Ok(job), None, Some(run)) => {
                self.runs.push_back((id.clone(), job, run));
                return self.next_run();
            }
            (Err(failure), ..) => failure,
            (_, Some(why), _) => commands::failure(command, &why),
            (_, _, None) => commands::failure(command, commands::NO_LSP_WORD),
        };
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": failure}));
    }

    /// Starts the next run waiting, unless one is under way.
    fn next_run(&mut self) {
        if self.run.is_some() {
            return;
        }
        let (Some(events), Some((id, job, run))) = (self.events.clone(), self.runs.pop_front()) else {
            return;
        };
        let report = Box::new(move |result| drop(events.send(Event::Ran(id, result))));
        self.run = Some(commands::start(job, run, report));
    }

    /// A run's RESULT, the answer to the client's request ID; then the next
    /// run.
    fn on_ran(&mut self, id: &Value, result: &Value) {
        if let Some(run) = self.run.take() {
            run.stop();
        }
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        self.next_run();
    }

    /// Logs WHY a parse failed or could not start, out of the user's way, as
    /// the Workspace gives it: once until the reason changes.
    fn log_parse(&mut self, why: &str) {
        // After `shutdown`, LSP lets the server send nothing but answers.
        if !self.shutdown {
            let params = json!({"type": 2, "message": format!("dbt-lsp-plus: {why}")});
            self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": params}));
        }
    }

    /// A parse's outcome, which the Workspace takes: a client asked to
    /// refresh its hints when there is an index, a failure logged.
    fn on_parsed(&mut self, outcome: config::Outcome) {
        let indexed = outcome.index.is_some();
        let failed = self.workspace.parsed(outcome.index, outcome.failure);
        if indexed {
            self.refresh_hints();
        }
        if let Some(why) = failed {
            self.log_parse(&why);
        }
        self.on_problems(outcome.problems);
        // A change during this parse wants the next one now: no request may
        // come to ask for it.
        self.want_index();
    }

    /// The errors and warnings of a parse that ran to its end, PROBLEMS, in
    /// place of the last parse's: each in its file, the root's if it is
    /// there, else the one in dbt's working directory, else the one
    /// package's that holds it; one in no such file is logged, once.  Each
    /// file with problems, before or now, is published again.
    fn on_problems(&mut self, problems: Option<Vec<parse_log::Problem>>) {
        let Some(problems) = problems else { return };
        // The packages, while the project's configuration still reads.
        let packages: Vec<PathBuf> =
            self.project().map_or_else(Vec::new, |project| project.packages.values().cloned().collect());
        // dbt names a YAML file's error from its working directory, and a
        // package's file from its package.
        let cwd = self.dbt_dir();
        let mut placed: HashMap<PathBuf, Vec<parse_log::Problem>> = HashMap::new();
        for problem in problems {
            let path = problem.spot.as_ref().and_then(|spot| {
                // dbt names a file from a sibling directory with `..`.
                let file = |dir: &Path| Some(lexical(&dir.join(&spot.path))).filter(|path| path.is_file());
                let in_cwd =
                    || cwd.as_deref().and_then(file).and_then(|path| self.local_path(Some(&features::uri(&path))));
                let mut held = packages.iter().filter_map(|dir| file(dir));
                let one_held = || held.next().filter(|_| held.next().is_none());
                self.workspace.root.as_deref().and_then(file).or_else(in_cwd).or_else(one_held)
            });
            match path {
                Some(path) => placed.entry(path).or_default().push(problem),
                None if !self.shutdown && self.diagnostics.logged.insert(problem.message.clone()) => {
                    let text = format!("dbt-lsp-plus: dbt parse: {}", problem.message);
                    let params = json!({"type": problem.severity, "message": text});
                    self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": params}));
                }
                None => {}
            }
        }
        let previous = std::mem::replace(&mut self.diagnostics.parsed, placed);
        // Unchanged too, as an edit may have dropped a file's failures since
        // and left their publication to this parse; but for a document with
        // edits unsaved, whose ranges the client has moved.
        let mut paths: Vec<PathBuf> = previous.keys().chain(self.diagnostics.parsed.keys()).cloned().collect();
        paths.sort();
        paths.dedup();
        paths.retain(|path| previous.get(path) != self.diagnostics.parsed.get(path) || !self.unsaved(path));
        paths.iter().for_each(|path| self.publish(path));
    }

    /// Whether the document at PATH is open with edits unsaved, its line
    /// ends aside: an editor such as Emacs sends a CRLF file's text with LF.
    fn unsaved(&self, path: &Path) -> bool {
        let lines = |text: &str| text.replace("\r\n", "\n");
        self.documents.get(path).is_some_and(|open| text::read(path).is_none_or(|file| lines(&file) != lines(open)))
    }

    /// The working directory of dbt's processes: the project's when the
    /// client's folder or a document named it, else the server's.
    fn dbt_dir(&self) -> Option<PathBuf> {
        self.workspace.dbt.dir().map(Path::to_owned).or_else(|| std::env::current_dir().ok())
    }

    /// Asks a client that can refresh its inlay hints to do so, since they
    /// show the relations of the index.
    fn refresh_hints(&mut self) {
        if !self.hint_refresh || self.shutdown {
            return;
        }
        self.ask_client("workspace/inlayHint/refresh", None);
    }

    /// Sends the client the server's request METHOD, with PARAMS if any,
    /// under an id of `OWN_ID`'s, whose answer `on_client` drops.
    fn ask_client(&mut self, method: &str, params: Option<Value>) {
        self.next_id += 1;
        let id = json!(format!("{OWN_ID}{}", self.next_id));
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        self.send_client(&request);
    }

    /// Stops a run under way, then the Workspace's parse, which removes this
    /// process's cache.
    fn stop_parse(&mut self) {
        if let Some(run) = self.run.take() {
            run.stop();
        }
        self.workspace.stop();
    }

    /// Answers a request with `handle`'s Answer, after asking for the
    /// index where that answer reads it, as its capability says, so that a
    /// reason is that of the parse it may start; inlay hints, which only make
    /// an index, ask after, when there is none.  A refusal fails it with its
    /// reason; a panic while answering fails that request alone, never the
    /// session.
    fn answer(&mut self, method: &str, id: &Value, params: &Value) {
        if capability::of(method).is_some_and(|capability| capability.wants_index) {
            self.ask_index();
        }
        let computed = panic::catch_unwind(AssertUnwindSafe(|| self.handle(method, params)));
        match computed {
            Ok(Some(answer)) => self.send_client(&answer.frame(id, method)),
            Ok(None) => self.reply_error(id, INVALID_PARAMS, "invalid parameters"),
            Err(_) => self.reply_error(id, INTERNAL_ERROR, "dbt-lsp-plus failed to answer this request"),
        }
        if method == "textDocument/inlayHint" && self.workspace.index.is_none() {
            self.ask_index();
        }
    }

    /// The Answer to METHOD at PARAMS, by the method's handler; none for
    /// invalid parameters.  It sends nothing.
    fn handle(&self, method: &str, params: &Value) -> Option<Answer> {
        match method {
            "textDocument/documentSymbol" => self.outline(params).map(Answer::Found),
            "workspace/symbol" => self.symbols(params),
            config::NODE_INFO => self.node_info(params),
            "textDocument/references" | "textDocument/definition" => {
                self.on_navigation(method == "textDocument/references", params)
            }
            "textDocument/rename" => self.on_rename(params),
            "textDocument/foldingRange" | "textDocument/documentHighlight" | "textDocument/selectionRange" => {
                self.on_document(method, params).map(Answer::Found)
            }
            "textDocument/hover" => Some(self.on_hover(params)),
            "textDocument/inlayHint" => self.on_hints(params),
            _ => self.hierarchy(method, params),
        }
    }

    /// The workspace's symbols that match the request's query; why none
    /// over a project that does not read.
    fn symbols(&self, params: &Value) -> Option<Answer> {
        let query = params.get("query")?.as_str()?;
        let Some(project) = self.project() else { return Some(Answer::Unanswered(vec![self.unreadable()])) };
        Some(Answer::Found(features::workspace_symbols(&project, query, ENCODING)))
    }

    /// A message of the client's: a request, a notification, or the answer
    /// to a request of the server's, which is dropped, as anything else.
    fn on_client(&mut self, message: &Value) {
        let params = message.get("params").unwrap_or(&Value::Null);
        match (message.get("method").and_then(Value::as_str), message.get("id")) {
            (Some(method), Some(id)) => self.on_request(method, id, params),
            (Some(method), None) => self.on_notification(method, params),
            _ => {}
        }
    }

    /// A notification of the client's, METHOD with PARAMS: kept; when the
    /// graph may have changed, a parse, which nothing else would start.  With
    /// a project, `initialized` registers the watcher of files.  Before a
    /// project, the first document opened in one names it.
    fn on_notification(&mut self, method: &str, params: &Value) {
        if method == "textDocument/didOpen" && self.workspace.root.is_none() && self.initialized && !self.shutdown {
            self.on_first_project(params);
        }
        self.track(method, params);
        if method == "initialized" && self.workspace.root.is_some() {
            self.register_watcher();
        }
        // The first document opened in the project.
        let first_open = method == "textDocument/didOpen"
            && self.workspace.root.is_some()
            && !std::mem::replace(&mut self.opened, true);
        // dbt's output, as spelt and as resolved: a link out of the project included.
        let output = || {
            let spelt = self.workspace.root.iter().flat_map(|root| OUTPUT.map(|dir| root.join(dir)));
            spelt.flat_map(|dir| [fs::canonicalize(&dir).ok(), Some(dir)]).flatten().collect()
        };
        if changes_graph(method, params, output, |uri| self.local_path(uri)) || first_open {
            self.workspace.fresh = false;
            self.want_index();
        }
    }

    /// The project of the document that PARAMS opens, when it lies in one,
    /// before the document is tracked: taken, with the first scan, the
    /// watcher of runs and the watcher of files that `initialize` and
    /// `initialized` start with a project; then the line that says so.
    fn on_first_project(&mut self, params: &Value) {
        let path = uri_path(params.pointer("/textDocument/uri").and_then(Value::as_str));
        let Some(root) = path.and_then(|path| locate::project_of(&path, self.workspace.dbt.command())) else { return };
        self.take_project(root);
        self.workspace.warm();
        self.watch_runs();
        self.register_watcher();
        self.say_found();
    }

    /// Registers a watcher of every file, for a client that lets it: an edit
    /// outside the editor changes the graph too.
    fn register_watcher(&mut self) {
        if self.watches {
            let watcher = json!({"id": "dbt-lsp-plus-file-watcher", "method": "workspace/didChangeWatchedFiles",
                                 "registerOptions": {"watchers": [{"globPattern": "**/*"}]}});
            self.ask_client("client/registerCapability", Some(json!({"registrations": [watcher]})));
        }
    }

    /// The hover at PARAMS: the summary and facts of the node named there,
    /// else a doc's or a generic test's block; else null, but for a node
    /// named before any index, why there is none, and for a node named by a
    /// `ref`, a `source` or an entry's `name:` over a project that does not
    /// read, once there is an index, its reason.
    fn on_hover(&self, params: &Value) -> Answer {
        let index = self.workspace.index.as_ref();
        // Line 1, column 1 of a file stands for the node it defines.
        let start = params["position"] == json!({"line": 0, "character": 0});
        let named = self.workspace.node_at(&self.open_documents(), self.document_at(params).as_ref(), start);
        let summary = index
            .zip(named.flatten())
            .map(|(index, node)| (format!("{}\n{}", index.summary(node), index.facts(node)), true));
        // A doc's or a generic test's block, from the project's scan, which the cache keeps between requests, as
        // for `node_at`'s entry and file's start.
        let block = || {
            let (path, scanned, offset) = self.scan_at(params, None)?;
            let name = navigation::reference_at(&path, &scanned, offset)
                .filter(|name| matches!(name.kind, reference::Kind::Doc | reference::Kind::Test))?;
            let project = self.project()?;
            let found = navigation::resolve(&navigation::Lookup::new(&project, index), &path, &name);
            navigation::hover(&project, &name, found.first()?, self.markdown).map(|text| (text, false))
        };
        let nothing = match (named, index) {
            (Some(_), None) => Answer::Unanswered(vec![self.workspace.unindexed()]),
            (Some(None), Some(_)) if self.project().is_none() => Answer::Unanswered(vec![self.unreadable()]),
            _ => Answer::Empty,
        };
        let found = summary.or_else(block);
        found.map_or(nothing, |(text, hard)| Answer::Found(features::hover(&text, hard, self.markdown)))
    }

    /// The relation hints of the request's range: `[]` with no index, no
    /// project or no document; none when its parameters do not read.
    fn on_hints(&self, params: &Value) -> Option<Answer> {
        let uri = params.pointer("/textDocument/uri")?.as_str()?;
        let (start, end) = (params.pointer("/range/start")?, params.pointer("/range/end")?);
        let (Some(index), Some(project), Some((path, text))) =
            (self.workspace.index.as_ref(), self.project(), self.text_of(uri))
        else {
            return Some(Answer::Empty);
        };
        let lines = LineStarts::new(&text);
        let range = lines.offset_of(start, ENCODING)?..lines.offset_of(end, ENCODING)?;
        let lookup = navigation::Lookup::new(&project, Some(index));
        let hints = features::inlay_hints(&text, sources::file_calls(&path, &text), &lookup, &path, range, ENCODING);
        Some(Answer::Found(Value::Array(hints)))
    }

    /// A request of the client: `initialize`, answered; `on_elsewhere`'s on
    /// a document of another project; else as `route` sends it, `shutdown`
    /// marked first.
    fn on_request(&mut self, method: &str, id: &Value, params: &Value) {
        if method == "initialize" {
            return self.on_initialize(id, params);
        }
        self.shutdown |= method == "shutdown";
        if let Some(why) = self.outside(method, params) {
            return self.on_elsewhere(method, id, why);
        }
        match self.route(method, params) {
            Route::Here(method) => self.answer(method, id, params),
            Route::Answer(answer) => self.send_client(&answer.frame(id, method)),
            Route::Command(command) => self.on_command(command, id, params),
            Route::Unknown => {
                let named =
                    params["command"].as_str().filter(|_| method == "workspace/executeCommand").unwrap_or(method);
                self.reply_error(id, METHOD_NOT_FOUND, &format!("dbt-lsp-plus does not answer {named}"));
            }
            Route::Invalid => self.reply_error(id, INVALID_PARAMS, "invalid parameters"),
        }
    }

    /// Why the request METHOD at PARAMS is not the session's project's: its
    /// document, named by `textDocument.uri`, lies in another project
    /// (`locate::elsewhere`), for a method that reads the project; none
    /// before a project, and for any other request.
    fn outside(&self, method: &str, params: &Value) -> Option<Reason> {
        let root = self.workspace.root.as_ref()?;
        if !capability::of(method).is_some_and(|capability| capability.reads_project) {
            return None;
        }
        let document = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str))?;
        let project = locate::elsewhere(&document, root, self.workspace.dbt.command())?;
        Some(Reason::Elsewhere { document, project, served: root.clone() })
    }

    /// Answers the request METHOD of ID on a document of another project
    /// with WHY, computing nothing on it; inlay hints, a decoration, with
    /// none.
    fn on_elsewhere(&mut self, method: &str, id: &Value, why: Reason) {
        let answer = if method == "textDocument/inlayHint" { Answer::Empty } else { Answer::Unanswered(vec![why]) };
        self.send_client(&answer.frame(id, method));
    }

    /// Watches the files of the user's last run, in the target path
    /// (`DBT_TARGET_PATH`, else `target` under the root), from a thread
    /// that reads them when their size or time changes and reports their
    /// failures to the loop.
    fn watch_runs(&mut self) {
        let (Some(root), Some(events)) = (self.workspace.root.clone(), self.events.clone()) else { return };
        let target = root.join(std::env::var_os("DBT_TARGET_PATH").unwrap_or_else(|| "target".into()));
        let stale = |found: &Value, index: &Value| Some(runs::stale(found, index));
        let watched: [(&str, &'static str, ReadFailures); 2] =
            [("run_results.json", runs::LAST_RUN, runs::failures), ("sources.json", runs::STALE_SOURCE, stale)];
        for (file, code, read_failures) in watched {
            let (results, manifest, events) = (target.join(file), target.join("manifest.json"), events.clone());
            thread::spawn(move || {
                let stamp = |path: &Path| fs::metadata(path).ok().map(|meta| (meta.modified().ok(), meta.len()));
                let read =
                    |path: &Path| fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                let mut seen = None;
                loop {
                    thread::sleep(RUNS_EVERY);
                    let now = (stamp(&results), stamp(&manifest));
                    if seen.as_ref() == Some(&now) {
                        continue;
                    }
                    let present = seen.replace(now).is_some_and(|(results, _)| results.is_some());
                    // The manifest, which may be large, only for results to place.
                    let failures = match read(&results) {
                        Some(found) => {
                            let Some(index) = read(&manifest) else { continue };
                            read_failures(&found, &index)
                        }
                        // Results gone, as after `dbt clean`: nothing to show.
                        None if now.0.is_none() && present => Some(Vec::new()),
                        // None ever, or a file read while dbt writes it, which
                        // changes again when dbt has written it.
                        None => continue,
                    };
                    let written = now.0.and_then(|(time, _)| time).unwrap_or(SystemTime::UNIX_EPOCH);
                    if events.send(Event::Runs(code, failures, written)).is_err() {
                        return;
                    }
                }
            });
        }
    }

    /// The failures of kind CODE, their file written at WRITTEN, in place of
    /// those of that kind: by the path of each package's file, those of a
    /// file changed since left out; each file whose failures change is
    /// published again.
    fn on_runs(&mut self, code: &str, found: Option<Vec<runs::Failure>>, written: SystemTime) {
        let (Some(found), Some(project)) = (found, self.project()) else { return };
        let edited = |path: &Path| {
            let saved = fs::metadata(path).and_then(|meta| meta.modified()).is_ok_and(|time| time > written);
            saved || self.diagnostics.edited.get(path).is_some_and(|time| *time > written)
        };
        // The failures of the other kind stay.
        let kept = self.diagnostics.failures.iter().map(|(path, failures)| {
            (path.clone(), failures.iter().filter(|failure| failure.code != code).cloned().collect::<Vec<_>>())
        });
        let mut failures: HashMap<PathBuf, Vec<runs::Failure>> = kept.filter(|(_, kept)| !kept.is_empty()).collect();
        for failure in found {
            let Some(path) = project.packages.get(&failure.package).map(|dir| dir.join(&failure.path)) else {
                continue;
            };
            if !edited(&path) {
                failures.entry(path).or_default().push(failure);
            }
        }
        let previous = std::mem::replace(&mut self.diagnostics.failures, failures);
        let mut changed: Vec<PathBuf> = previous.keys().chain(self.diagnostics.failures.keys()).cloned().collect();
        changed.sort();
        changed.dedup();
        changed.retain(|path| previous.get(path) != self.diagnostics.failures.get(path));
        // A document with edits unsaved keeps the parse's ranges the client
        // has moved: the next parse publishes it.
        let parsed = |path: &PathBuf| self.diagnostics.parsed.get(path).is_some_and(|problems| !problems.is_empty());
        changed.retain(|path| !parsed(path) || !self.unsaved(path));
        changed.iter().for_each(|path| self.publish(path));
    }

    /// Publishes PATH's diagnostics, the last run's failures and the last
    /// parse's problems, under the client's URI for it, else the file's, with
    /// no version; the URI last published under, if another, gets none.
    /// Nothing after `shutdown`.
    fn publish(&mut self, path: &Path) {
        if self.shutdown {
            return;
        }
        let uri = self.uris.get(path).cloned().unwrap_or_else(|| features::uri(path));
        let mut diagnostics = Vec::new();
        // The failures, placed in the text as it stands.
        let failures = self.diagnostics.failures.get(path).map_or(&[][..], Vec::as_slice);
        let text = self.documents.get(path).cloned().or_else(|| text::read(path)).unwrap_or_default();
        let lines = LineStarts::new(&text);
        let (entries, tests) = (properties::entries(&text), properties::tests(&text));
        diagnostics.extend(failures.iter().map(|failure| {
            let range = runs::locate(&text, &entries, &tests, &failure.locator);
            json!({"range": lines.range(range.start, range.end, ENCODING), "severity": failure.severity,
                   "source": "dbt-lsp-plus", "code": failure.code, "message": failure.message})
        }));
        // At dbt's line and column, from 1 and in code points; a file's start
        // when dbt gives none.
        let problems = self.diagnostics.parsed.get(path).map_or(&[][..], Vec::as_slice);
        diagnostics.extend(problems.iter().map(|problem| {
            let (line, column) = problem.spot.as_ref().and_then(|spot| spot.at).unwrap_or((1, 1));
            let at = lines.offset(line.saturating_sub(1), column.saturating_sub(1), Encoding::Utf32);
            json!({"range": lines.range(at, at, ENCODING), "severity": problem.severity,
                   "source": "dbt parse", "code": problem.code, "message": problem.message})
        }));
        let publish = |uri: &str, diagnostics: Vec<Value>| {
            json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                   "params": {"uri": uri, "diagnostics": diagnostics}})
        };
        if let Some(old) = self.diagnostics.published.insert(path.to_owned(), uri.clone()).filter(|old| *old != uri) {
            self.send_client(&publish(&old, Vec::new()));
        }
        self.send_client(&publish(&uri, diagnostics));
    }

    /// Handles the events of INBOX, one epoch each, until `exit`, or until the
    /// client closes its output.
    fn serve(&mut self, inbox: Receiver<Event>) {
        for event in inbox {
            // A new epoch: the cache looks at the project's files again.
            self.workspace.epoch += 1;
            match event {
                Event::Client(message) => {
                    self.on_client(&message);
                    if message["method"] == "exit" {
                        return;
                    }
                }
                Event::ClientClosed => return,
                Event::Parsed(outcome) => self.on_parsed(outcome),
                Event::Runs(code, found, written) => self.on_runs(code, found, written),
                Event::Ran(id, result) => self.on_ran(&id, &result),
            }
        }
    }
}

/// Serves on the standard input and output, COMMAND, the server command,
/// naming the user's dbt and its parse's flags.  Given its `--project-dir`,
/// that project is said at once on the standard error, and to the client
/// once it is initialized; else nothing runs before the client's folder or
/// a document names the project.
pub fn run(command: &[String]) -> ExitCode {
    let (events, inbox) = mpsc::channel();
    let reader = events.clone();
    thread::spawn(move || {
        let mut input = BufReader::new(io::stdin());
        while let Ok(Some(message)) = rpc::read_frame(&mut input) {
            if reader.send(Event::Client(message)).is_err() {
                return;
            }
        }
        let _ = reader.send(Event::ClientClosed);
    });
    let given = project_dir(command);
    let mut proxy = Proxy::new(io::BufWriter::new(io::stdout().lock()), given.clone());
    if let Some(root) = given {
        let line =
            format!("dbt-lsp-plus: project {}, dbt {}.", root.display(), locate::Located::written(command).named);
        eprintln!("{line}");
        proxy.line = Some(line);
    }
    proxy.events = Some(events);
    proxy.workspace.dbt = config::Dbt::new(command.to_vec(), None);
    proxy.serve(inbox);
    proxy.stop_parse();
    if proxy.shutdown { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project;

    #[test]
    fn the_agent_page_quotes_the_proxy_s_own_words() {
        let page = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/how-to/use-with-an-agent.md"));
        let page = page.unwrap().split_whitespace().collect::<Vec<_>>().join(" ");
        let sources = [
            ("proxy.rs", include_str!("proxy.rs")),
            ("workspace.rs", include_str!("workspace.rs")),
            ("answer.rs", include_str!("answer.rs")),
            ("cli.rs", include_str!("cli.rs")),
            ("commands.rs", include_str!("commands.rs")),
            ("packages.rs", include_str!("packages.rs")),
            ("locate.rs", include_str!("locate.rs")),
        ];
        let marker = "\n#[cfg(test)]\nmod tests {";
        // Whitespace collapsed as on the page, so that a site rustfmt rewraps is still found.
        let code = sources.map(|(name, source)| {
            source.split_once(marker).expect(name).0.split_whitespace().collect::<Vec<_>>().join(" ")
        });
        // What the page quotes, then the code at the site it cites, with enough around it to be that site alone.
        let quoted = [
            ("dbt parse has not finished: it is running", r#"(true, _) => "it is running""#),
            ("dbt parse has not finished:", r#"write!(f, "dbt parse has not finished: {why}")"#),
            (
                "dbt parse has not finished: it has not run, or left no manifest",
                r#"(false, None) => "it has not run, or left no manifest""#,
            ),
            ("dbt parse has not finished: dbt parse failed:", r#"format!("dbt parse failed: {failure}")"#),
            // The arm that puts a failure after the prefix, which the quotes of a failure rest on.
            ("dbt parse has not finished: dbt parse failed:", "(false, Some(failure)) => failure,"),
            (
                "no dbt_project.yml at or above <root>; give a path within the project",
                r#"Missing::Above => format!("no dbt_project.yml at or above {shown}; give a path within the project")"#,
            ),
            ("then `... run dbt deps`", r#""dbt parse would install it, rewriting the install path; run dbt deps""#),
            (
                "A session finds one project, once",
                r#"method == "textDocument/didOpen" && self.workspace.root.is_none()"#,
            ),
            // The reasons that the server gives where it cannot answer, one arm each of `Reason`'s words.
            (
                "`no dbt project at <root>: its dbt_project.yml is missing, is not YAML, or names no project`",
                r#""no dbt project at {}: its dbt_project.yml is missing, is not YAML, or names no project""#,
            ),
            (
                "lies in the dbt project at <other>, and this session serves only the one at <root>",
                r#""{} lies in the dbt project at {}, and this session serves only the one at {}""#,
            ),
        ];
        for (words, site) in quoted {
            assert!(page.contains(words), "the agent page says {words:?}");
            let sites: usize = code.iter().map(|source| source.matches(site).count()).sum();
            assert_eq!(sites, 1, "one site outside the tests: {site}");
        }
    }

    #[test]
    fn only_a_change_outside_dbt_s_output_may_change_the_graph() {
        let root = Path::new("/work/a b/shop");
        // As the server gives them: a link of logs out of the project, resolved, included.
        let output = vec![root.join("target"), root.join("logs"), PathBuf::from("/elsewhere/logs")];
        let file = |path: &str| format!("file:///work/a%20b/shop/{path}");
        let watched = |uris: &[String]| {
            let changes: Vec<Value> = uris.iter().map(|uri| json!({"uri": uri, "type": 2})).collect();
            json!({"changes": changes})
        };
        let (event, created) = ("workspace/didChangeWatchedFiles", "workspace/didCreateFiles");
        let (log, model) = (file("logs/dbt-lsp.log"), file("models/orders.sql"));
        let resolved = "file:///elsewhere/logs/dbt-lsp.log".to_owned();
        let cases = [
            ("target itself", event, watched(&[file("target")]), true, false),
            ("output only", event, watched(&[file("target/compiled/shop/x.sql"), log.clone()]), true, false),
            ("through a link out of the project", event, watched(std::slice::from_ref(&resolved)), true, false),
            ("output, then a model", event, watched(&[log.clone(), model.clone()]), true, true),
            ("a model, then output", event, watched(&[model.clone(), log.clone()]), true, true),
            ("another first directory", event, watched(&[file("target2/x")]), true, true),
            ("a target below the root's top", event, watched(&[file("models/target/x.sql")]), true, true),
            ("the root itself", event, watched(&[file("")]), true, true),
            (
                "outside the root, beside output",
                event,
                watched(&["file:///else/target/x".into(), log.clone()]),
                true,
                true,
            ),
            ("not a file", event, watched(&["untitled:Untitled-1".into(), log.clone()]), true, true),
            (
                "another scheme on a path in the output",
                event,
                watched(&["untitled:/work/a%20b/shop/target/x".into()]),
                true,
                true,
            ),
            ("no URI, beside output", event, json!({"changes": [{"type": 2}, {"uri": log, "type": 2}]}), true, true),
            ("no change", event, json!({"changes": []}), true, true),
            ("no changes", event, json!({}), true, true),
            ("no root", event, watched(std::slice::from_ref(&resolved)), false, true),
            ("a file the client creates", created, json!({"files": [{"uri": file("target/x")}]}), true, true),
            ("a save", "textDocument/didSave", json!({}), true, true),
            ("an edit", "textDocument/didChange", json!({}), true, false),
        ];
        for (name, method, params, rooted, expected) in cases {
            let given = || if rooted { output.clone() } else { Vec::new() };
            assert_eq!(changes_graph(method, &params, given, uri_path), expected, "{name}");
        }
    }

    #[test]
    fn a_hover_before_any_index_says_why_after_asking_for_one() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let orders = features::uri(&root.join("models/marts/orders.sql"));
        let inside_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}});
        // No index, none asked for yet: the hover asks for one, then says why there is none, as nodeInfo would
        // now; the parse cannot start here, the server command having no `lsp`, which only the parse asked for says.
        let mut proxy = Proxy::new(Vec::new(), Some(root));
        let asked = "dbt parse has not finished: the server command has no lsp, so dbt-lsp-plus runs no dbt parse";
        proxy.on_request("textDocument/hover", &json!(1), &inside_ref);
        let mut client = &proxy.client[..];
        let written = std::iter::from_fn(|| rpc::read_frame(&mut client).unwrap());
        let written: Vec<Value> = written.filter(|message| message["id"] == 1).collect();
        let error = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32803, "message": asked}});
        assert_eq!(written, [error]);
    }

    #[test]
    fn a_request_reads_the_held_scan_until_the_open_text_differs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let path = root.join("models/marts/orders.sql");
        let params = json!({"textDocument": {"uri": features::uri(&path)}, "position": {"line": 0, "character": 0}});
        let mut proxy = Proxy::new(Vec::new(), Some(root));
        let project = proxy.project().unwrap();
        let (_, scanned, _) = proxy.scan_at(&params, Some(&project)).unwrap();
        assert!(Arc::ptr_eq(&scanned, &project.files[&path]), "the project's scan of the file");
        let open = "select 1";
        proxy.track("textDocument/didOpen", &json!({"textDocument": {"uri": features::uri(&path), "text": open}}));
        let (_, scanned, _) = proxy.scan_at(&params, Some(&project)).unwrap();
        assert!(!Arc::ptr_eq(&scanned, &project.files[&path]), "a scan of its own");
        assert_eq!(scanned.text, open);
    }

    #[test]
    fn a_node_s_information_at_a_function_s_argument_in_sql_is_empty_and_at_its_start_the_file_s() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let path = root.join("models/marts/uses_f.sql");
        let text = "select {{ function('f') }}(1) as v";
        let mut proxy = Proxy::new(Vec::new(), Some(root.clone()));
        proxy.documents.insert(path.clone(), text.to_owned());
        assert_eq!(
            proxy.workspace.node_at(
                &proxy.open_documents(),
                Some(&(path.clone(), text.to_owned(), text.find("'f'").unwrap() + 1)),
                false
            ),
            None,
            "a function's argument names no node before any parse"
        );
        // No argument, or a file neither open nor on disk: why there is no index yet; once there is one, none
        // and empty.
        let missing = features::uri(&root.join("models/marts/missing.sql"));
        let at_missing = json!({"arguments": [{"uri": missing, "position": {"line": 0, "character": 0}}]});
        let unindexed = Some(Answer::Unanswered(vec![proxy.workspace.unindexed()]));
        assert_eq!((proxy.node_info(&json!({})), proxy.node_info(&at_missing)), (unindexed.clone(), unindexed));
        let node = json!({"unique_id": "model.shop.uses_f", "name": "uses_f", "resource_type": "model",
                          "package_name": "shop", "original_file_path": "models/marts/uses_f.sql", "config": {}});
        let manifest = json!({"metadata": {"project_name": "shop"}, "nodes": {"model.shop.uses_f": node}});
        proxy.workspace.index = Some(config::ParseIndex::of(&manifest));
        let at = |character: usize| json!({"arguments": [{"uri": features::uri(&path), "position": {"line": 0, "character": character}}]});
        // A call whose argument holds the position names its node, or none, as a function's does.
        assert_eq!(proxy.node_info(&at(text.find("'f'").unwrap() + 1)), Some(Answer::Empty));
        let Some(Answer::Found(start)) = proxy.node_info(&at(0)) else { panic!("no node at the file's start") };
        assert_eq!(start["unique_id"], "model.shop.uses_f");
        assert_eq!(proxy.node_info(&json!({})), None);
        assert_eq!(proxy.node_info(&at_missing), Some(Answer::Empty));
    }

    #[test]
    fn hover_node_information_and_hints_describe_the_node_a_ref_s_definition_leads_to() {
        const USES: &str = "select * from {{ ref('dim', v=1) }} join {{ ref('dim') }} join {{ ref('stale') }} \
                            join {{ source('raw', 'payments') }}";
        const DIM: &str =
            "version: 2\nmodels:\n  - name: dim\n    latest_version: 2\n    versions:\n      - v: 1\n      - v: 2\n";
        const SOURCES: &str = "version: 2\nsources:\n  - name: raw\n    tables:\n      - name: payments\n";
        let scratch = |name: &str, config: &str| {
            let files = [
                ("dbt_project.yml", config),
                ("models/dim_v1.sql", "select 1"),
                ("models/dim_v2.sql", "select 2"),
                ("models/stale.sql", "select 3"),
                ("models/_dim.yml", DIM),
                ("models/_sources.yml", SOURCES),
                ("models/uses.sql", USES),
                ("dbt_packages/pkg/dbt_project.yml", "name: pkg\n"),
                ("dbt_packages/pkg/models/_sources.yml", SOURCES),
            ];
            project::scratch_project(name, &files)
        };
        let version = |number: u64| {
            json!({"unique_id": format!("model.shop.dim.v{number}"), "name": "dim", "resource_type": "model",
                   "package_name": "shop", "original_file_path": format!("models/dim_v{number}.sql"),
                   "version": number, "latest_version": 2, "relation_name": format!("v{number}"), "config": {}})
        };
        let versions = json!({"model.shop.dim.v1": version(1), "model.shop.dim.v2": version(2)});
        // Disabled by a parse before the last, which lacks it, while its file remains.
        let stale = json!({"unique_id": "model.shop.stale", "name": "stale", "resource_type": "model",
                           "package_name": "shop", "original_file_path": "models/stale.sql",
                           "config": {"materialized": "ephemeral"}});
        // The root disables its `raw.payments`, which `pkg` declares too, enabled.
        let payments = |package: &str| {
            json!({"unique_id": format!("source.{package}.raw.payments"), "name": "payments", "source_name": "raw",
                   "resource_type": "source", "package_name": package, "original_file_path": "models/_sources.yml",
                   "relation_name": format!("{package}_payments"), "config": {}})
        };
        let sources = json!({"source.pkg.raw.payments": payments("pkg")});
        let proxy_on = |root: &Path| {
            let mut proxy = Proxy::new(Vec::new(), Some(root.to_owned()));
            let shop = json!({"project_name": "shop"});
            let disabled = json!({"model.shop.stale": [stale], "source.shop.raw.payments": [payments("shop")]});
            let mut index = config::ParseIndex::of(
                &json!({"metadata": shop, "nodes": versions, "sources": sources, "disabled": disabled}),
            );
            let disabled = json!({"source.shop.raw.payments": [payments("shop")]});
            let last = json!({"metadata": shop, "nodes": versions, "sources": sources, "disabled": disabled});
            index.overlay(config::ParseIndex::of(&last));
            proxy.workspace.index = Some(index);
            proxy
        };
        // A hover and nodeInfo inside NEEDLE, a call's argument, in `uses.sql`.
        let at = |root: &Path, needle: &str| {
            let uri = features::uri(&root.join("models/uses.sql"));
            let position = json!({"line": 0, "character": USES.find(needle).unwrap() + 1});
            (
                json!({"textDocument": {"uri": uri}, "position": position}),
                json!({"arguments": [{"uri": uri, "position": position}]}),
            )
        };
        // Hints over the whole of `uses.sql`.
        let hints = |root: &Path| {
            let (uri, end) =
                (features::uri(&root.join("models/uses.sql")), json!({"line": 0, "character": USES.len()}));
            json!({"textDocument": {"uri": uri}, "range": {"start": {"line": 0, "character": 0}, "end": end}})
        };
        let root = scratch("proxy-node-lookup", "name: shop\n");
        let proxy = proxy_on(&root);
        // NodeInfo and a hover inside each call's argument: the version named; the stale node, whose file remains,
        // as the index holds it; the root's disabled source, as definition leads.
        let calls = ["'dim', v=1", "'stale'", "'payments'"];
        let nodes = calls.map(|needle| {
            let Some(Answer::Found(node)) = proxy.node_info(&at(&root, needle).1) else {
                panic!("no node at {needle}")
            };
            (node["unique_id"].as_str().unwrap().to_owned(), node["disabled"] == true)
        });
        let expected = [("model.shop.dim.v1", false), ("model.shop.stale", true), ("source.shop.raw.payments", true)];
        assert_eq!(nodes, expected.map(|(id, disabled)| (id.to_owned(), disabled)));
        let hovered = calls.map(|needle| match proxy.on_hover(&at(&root, needle).0) {
            Answer::Found(hover) => hover["contents"]["value"].as_str().unwrap_or_default().to_owned(),
            _ => String::new(),
        });
        let described = hovered.iter().zip(expected).all(|(text, (id, _))| text.contains(&format!("`{id}`")));
        assert!(described, "{hovered:?}");
        let Some(Answer::Found(found)) = proxy.on_hints(&hints(&root)) else { panic!("no hints in uses.sql") };
        let labels: Vec<&Value> = found.as_array().unwrap().iter().map(|hint| &hint["label"]).collect();
        assert_eq!(labels, [&json!("v2")], "none after `v=1)`, none for a disabled node, the stale one or the source");
        // The call hierarchy looks its calls up with the index: the package's source, not the root's disabled one.
        let item = json!({"item": {"data": {"uniqueId": "model.shop.uses"}}});
        let Some(Answer::Found(calls)) = proxy.hierarchy("callHierarchy/outgoingCalls", &item) else {
            panic!("no outgoing calls of uses")
        };
        let called: Vec<&Value> =
            calls.as_array().unwrap().iter().map(|call| &call["to"]["data"]["uniqueId"]).collect();
        let expected = ["model.shop.dim.v1", "model.shop.dim.v2", "model.shop.stale", "source.pkg.raw.payments"];
        assert_eq!(called, expected.map(|id| json!(id)).iter().collect::<Vec<_>>());
        // Over a project that does not read, its reason, as its definition gives; no hint; without an index, why none.
        let broken = scratch("proxy-node-lookup-unreadable", "name: [");
        let (dim, info) = at(&broken, "'dim', v=1");
        let mut proxy = proxy_on(&broken);
        assert_eq!(proxy.node_info(&info), Some(Answer::Unanswered(vec![Reason::Unreadable(broken.clone())])));
        assert_eq!(proxy.on_hover(&dim), Answer::Unanswered(vec![Reason::Unreadable(broken.clone())]), "a hover");
        assert_eq!(proxy.on_hints(&hints(&broken)), Some(Answer::Empty), "the hints");
        // An entry's `name:` reads as a `ref` there: `dim` of `name: dim`.
        let entry = json!({"line": 2, "character": 10});
        let dim_yml = features::uri(&broken.join("models/_dim.yml"));
        let (on_entry, entry_info) = (
            json!({"textDocument": {"uri": dim_yml}, "position": entry}),
            json!({"arguments": [{"uri": dim_yml, "position": entry}]}),
        );
        let unreadable = Answer::Unanswered(vec![Reason::Unreadable(broken.clone())]);
        let entry_read = (proxy.node_info(&entry_info), proxy.on_hover(&on_entry));
        proxy.workspace.index = None;
        let unindexed = Answer::Unanswered(vec![proxy.workspace.unindexed()]);
        assert_eq!(proxy.on_hover(&dim), unindexed, "a hover before any index");
        let entry_before = proxy.on_hover(&on_entry);
        assert_eq!(
            (entry_read, entry_before),
            ((Some(unreadable.clone()), unreadable), unindexed),
            "an entry's, with an index then before any"
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(broken).unwrap();
    }

    #[test]
    fn a_position_that_names_nothing_is_empty_and_a_target_found_nowhere_is_found() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let proxy_on = |text: &str, path: &Path| {
            let mut proxy = Proxy::new(Vec::new(), Some(root.clone()));
            proxy.documents.insert(path.to_owned(), text.to_owned());
            proxy
        };
        let at = |path: &Path, character: usize| json!({"textDocument": {"uri": features::uri(path)}, "position": {"line": 0, "character": character}});
        // An unsaved document's ref to a model that no file holds, and a keyword before it.
        let scratch = root.join("models/marts/scratch.sql");
        let text = "select * from {{ ref('nowhere') }}";
        let proxy = proxy_on(text, &scratch);
        let (keyword, nowhere) = (at(&scratch, 1), at(&scratch, text.find("nowhere").unwrap() + 2));
        // A definition, then the references.
        assert_eq!(
            [false, true].map(|references| proxy.on_navigation(references, &keyword)),
            [Some(Answer::Empty), Some(Answer::Empty)]
        );
        let found = [Some(Answer::Found(Value::Null)), Some(Answer::Found(json!([])))];
        assert_eq!([false, true].map(|references| proxy.on_navigation(references, &nowhere)), found);
        let outside = at(&root.join("packages.yml"), 0);
        assert_eq!(proxy.hierarchy("textDocument/prepareCallHierarchy", &outside), Some(Answer::Empty));
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn reads_the_project_dir_in_both_forms() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(project_dir(&args(&["dbt", "lsp", "--project-dir", "."])), Some(here.clone()));
        assert_eq!(project_dir(&args(&["dbt", "lsp", "--project-dir=."])), Some(here.clone()));
        assert_eq!(project_dir(&args(&["dbt", "lsp", "--project-dir=src/.."])), Some(here));
        assert_eq!(project_dir(&args(&["dbt", "lsp", "--project-directory", "x"])), None);
        assert_eq!(project_dir(&args(&["dbt", "lsp"])), None);
    }

    #[test]
    fn keeps_the_project_dir_as_spelt_unless_a_link_before_dot_dot_moves_it() {
        let dir = std::env::temp_dir().join(format!("dbt-lsp-plus-project-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        std::os::unix::fs::symlink(dir.join("a"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("a/b"), dir.join("deep")).unwrap();
        let dir_of = |spelling: &str| project_dir(&args(&["dbt", "lsp", "--project-dir", spelling]));
        let spelt = dir.join("link");
        assert_eq!(dir_of(spelt.to_str().unwrap()), Some(spelt.clone()));
        assert_eq!(dir_of(dir.join("link/b/..").to_str().unwrap()), Some(spelt));
        // deep/.. is a/b/.. to the system, a; read lexically, it would be DIR.
        assert_eq!(dir_of(dir.join("deep/..").to_str().unwrap()), Some(fs::canonicalize(dir.join("a")).unwrap()));
        fs::remove_dir_all(&dir).unwrap();
    }
}
