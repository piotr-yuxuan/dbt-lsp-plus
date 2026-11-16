// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy: forwards every message between the editor and `dbt lsp`
//! byte for byte, and answers itself the requests of the features that the
//! server does not announce.

use crate::graph::{self, Graph, ServerNode};
use crate::project::Project;
use crate::rpc::{self, Frame};
use crate::text::{self, Encoding, LineIndex};
use crate::{config, document, features, navigation, project, properties, runs, scan, sources};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use url::Url;

/// The prefix of the ids of the requests that the proxy itself sends to the
/// server; the client's ids pass unchanged.
pub const OWN_ID: &str = "dbt-lsp-plus/";
/// How long the server has to exit after `exit` before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);
const REQUEST_FAILED: i64 = -32803;
const INVALID_REQUEST: i64 = -32600;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Each capability the proxy adds when the server lacks it, with the
/// methods it then answers.
const ADDED: [(&str, &[&str]); 6] = [
    ("documentSymbolProvider", &["textDocument/documentSymbol"]),
    ("workspaceSymbolProvider", &["workspace/symbol"]),
    (
        "callHierarchyProvider",
        &["textDocument/prepareCallHierarchy", "callHierarchy/incomingCalls", "callHierarchy/outgoingCalls"],
    ),
    ("foldingRangeProvider", &["textDocument/foldingRange"]),
    ("documentHighlightProvider", &["textDocument/documentHighlight"]),
    ("selectionRangeProvider", &["textDocument/selectionRange"]),
];

/// The requests that the proxy answers itself on some targets, since dbt
/// lsp 2.0.9 answers them with nothing there: references and renames of a
/// source, a rename of a CTE, a definition that `navigation` finds;
/// elsewhere they go to the server.
const TARGETED: [&str; 3] = ["textDocument/references", "textDocument/rename", "textDocument/definition"];

/// The selectors of the graph, one request each: `dbt.listNodes` reads only
/// its first argument, `*` leaves sources out, and a second request in
/// flight cancels the first.
const SELECTORS: [&str; 2] = ["*", "source:*"];
/// How often a listing cancelled by another `dbt.listNodes` is tried again.
const MAX_ATTEMPTS: u32 = 3;
/// A listing unanswered for this long is abandoned.
const LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// At most this many restarts of a server that died, within the window.
const RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(600);
/// How long a restarted server has to answer its `initialize`.
const REPLAY_TIMEOUT: Duration = Duration::from_secs(10);
/// The client's notifications after which the graph may have changed.  The
/// server's `dbt/lspCompileComplete` is no such signal: a listing makes the
/// server compile, so listing on it would never end.
const CHANGES: [&str; 5] = [
    "textDocument/didSave",
    "workspace/didChangeWatchedFiles",
    "workspace/didCreateFiles",
    "workspace/didRenameFiles",
    "workspace/didDeleteFiles",
];
/// The directories under its project where dbt lsp 2.0.9 writes at each
/// compilation, whatever its `--target-path` and `--log-path` say.
const OUTPUT: [&str; 2] = ["target", "logs"];

enum Event {
    Client(Frame),
    ClientClosed,
    /// A frame of the server process of that generation.
    Server(u64, Frame),
    /// The end of the output of the server process of that generation.
    ServerClosed(u64),
    Parsed(config::Outcome),
    /// The failures of one kind, `runs::LAST_RUN` or `runs::STALE_SOURCE`,
    /// when their file was written; none for results of a command that
    /// builds nothing.
    Runs(&'static str, Option<Vec<runs::Failure>>, SystemTime),
}

/// What a watched file and the manifest give: its failures, or none to keep
/// those known.
type ReadFailures = fn(&Value, &Value) -> Option<Vec<runs::Failure>>;

/// How often the files of the last run are looked at.
const RUNS_EVERY: Duration = Duration::from_secs(1);

/// What the proxy knows to start a server that died again.
#[derive(Default)]
struct Restart {
    /// When each restart of the window began.
    past: Vec<Instant>,
    /// When the next restart starts.
    due: Option<Instant>,
    /// A server started again, until it answers its `initialize`: its id,
    /// and the deadline of that answer.
    starting: Option<(Server, String, Instant)>,
    /// The client's `initialize` parameters, which a restart sends again.
    initialize: Option<Value>,
    /// The language and last version of each open document.
    languages: HashMap<PathBuf, Value>,
    versions: HashMap<PathBuf, Value>,
    /// The ids of the registrations the client holds.
    registered: HashSet<String>,
    /// The progress tokens the server began and has not ended.
    progress: HashMap<String, Value>,
    /// The generation of the last server started.
    spawned: u64,
}

/// The wait before restarting a server that died at NOW, after the restarts
/// of PAST: the n-th restart of a window of `RESTART_WINDOW` waits n
/// seconds; none past `RESTARTS` restarts.
fn restart_delay(past: &[Instant], now: Instant) -> Option<Duration> {
    let recent = past.iter().filter(|at| now.saturating_duration_since(**at) < RESTART_WINDOW).count();
    (recent < RESTARTS).then(|| Duration::from_secs(recent as u64 + 1))
}

/// What the proxy knows of each file's diagnostics, by its path as the root
/// maps it.
#[derive(Default)]
struct Diagnostics {
    /// The server's last diagnostics, with the URI it named the file by.
    server: HashMap<PathBuf, (String, Vec<Value>)>,
    /// The failures of the last run.
    failures: HashMap<PathBuf, Vec<runs::Failure>>,
    /// When the client last changed or saved each document.
    edited: HashMap<PathBuf, SystemTime>,
    /// The URI under which the proxy last published.
    published: HashMap<PathBuf, String>,
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    /// Which process of the session it is, from 0: its events carry it, so
    /// that those of a process the proxy let go of are dropped.
    generation: u64,
}

/// What the proxy remembers of a request of the client's that it forwarded.
#[derive(Clone, PartialEq)]
enum Pending {
    Request,
    /// A `dbt.listNodes`: the proxy lists only when none is pending, since a
    /// second listing cancels the first.
    Listing,
    /// Answered with null if the server dies first, as LSP asks of `shutdown`.
    Shutdown,
    /// A hover whose answer gets this text first: a node's summary, whose
    /// line breaks are hard, or a docs block's text or a test's signature,
    /// markdown as written.
    Hover(String, bool),
    /// An inlay hint request, whose answer gets these hints after its own.
    Hints(Vec<Value>),
}

impl Pending {
    /// The server's answer, MESSAGE, as this kind of request makes it, for a
    /// client that reads markdown when MARKDOWN; none to pass it unchanged.
    fn merged(&self, message: &Value, markdown: bool) -> Option<Value> {
        match self {
            Pending::Hover(text, hard) => features::merge_hover(message, text, *hard, markdown),
            Pending::Hints(hints) => features::merge_hints(message, hints),
            _ => None,
        }
    }
}

/// What the proxy knows of the user's `dbt parse`.
#[derive(Default)]
struct Parsing {
    /// The server command, from which the parse's is made.
    server: Vec<String>,
    /// Where a parse reports; none in the unit tests, which parse nothing.
    events: Option<Sender<Event>>,
    running: Option<config::Running>,
    index: Option<config::Index>,
    /// Whether no change came since the last parse started.
    fresh: bool,
    /// Whether the server compiled since the last change: dbt has then
    /// resolved the packages itself.
    compiled: bool,
    /// Whether a request wanted an index before the server had compiled:
    /// the server's compilation then starts the parse.
    wanted: bool,
    /// Why the last parse failed, or none could start, as logged.
    failure: Option<String>,
    /// Whether a parse runs cut off the network and from writing the project.
    cut: bool,
    cache: Option<PathBuf>,
}

struct Proxy<W: Write> {
    client: W,
    server: Option<Server>,
    /// Whether the user's dbt has no `lsp`, as dbt-oss: no server runs, and
    /// the proxy answers alone, from its scan and the user's `dbt parse`.
    standalone: bool,
    /// Why there is no server, told to the client once it is initialized.
    absent: Option<String>,
    /// The project directory: the server command's `--project-dir`, spelt
    /// as the client spells it when the client names the same directory.
    root: Option<PathBuf>,
    /// The root with its links resolved, onto which the client's paths map.
    canonical_root: Option<PathBuf>,
    encoding: Encoding,
    hierarchical: bool,
    /// The methods that the proxy answers, fixed by the server's capabilities.
    owned: HashSet<&'static str>,
    /// Whether the client sent `initialize`, before which LSP lets the proxy send nothing.
    initialized: bool,
    /// The id of the client's `initialize` while the server has not answered it.
    initialize: Option<Value>,
    /// The client's requests that the server has yet to answer.
    forwarded: HashMap<String, (Value, Pending)>,
    documents: HashMap<PathBuf, String>,
    /// The URI under which the client opened each of them, which an edit
    /// of it must name.
    uris: HashMap<PathBuf, String>,
    /// Whether the client opened a document, the first of which prompts a listing.
    opened: bool,
    next_id: u64,
    /// The proxy's `dbt.listNodes` in flight: its id, its selector's slot, when it was sent.
    listing: Option<(String, usize, Instant)>,
    /// The nodes of the first selector, while the second is listed.
    listed: Vec<ServerNode>,
    attempts: u32,
    /// The graph may have changed while it could not be listed: list it later.
    relist: bool,
    nodes: Option<Vec<ServerNode>>,
    shutdown: bool,
    /// Whether the client reads markdown in a hover.
    markdown: bool,
    /// Whether the client may be asked to refresh its inlay hints.
    hint_refresh: bool,
    /// The proxy's own requests to the client, whose answers stay here.
    asked: HashSet<String>,
    diagnostics: Diagnostics,
    restart: Restart,
    parsing: Parsing,
}

/// The path of a `file:` URI.
fn uri_path(uri: Option<&str>) -> Option<PathBuf> {
    Url::parse(uri?).ok()?.to_file_path().ok()
}

/// Whether the client's notification METHOD, of PARAMS, may change the
/// graph: one of `CHANGES`, but a watched-files event whose every change is
/// a `file:` URI that MAP places in one of the directories OUTPUT gives,
/// dbt lsp's own output, where it writes at each compilation: listing on it
/// would never end, for a client that honours dbt lsp's watcher of `**/*`.
/// An event the proxy cannot read is a change.
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

fn key(id: &Value) -> String {
    id.to_string()
}

/// The `--project-dir` of COMMAND, as `--project-dir X` or
/// `--project-dir=X`, made absolute as spelt, so that the paths the proxy
/// returns read as the user wrote them: each `..` drops the component before
/// it.  Resolved only when that names another directory, through a link
/// before a `..`.
fn project_dir(command: &[String]) -> Option<PathBuf> {
    let value = config::flag(command, "--project-dir")?;
    let spelt = std::path::absolute(value).ok()?.components().fold(PathBuf::new(), |mut path, component| {
        match component {
            Component::ParentDir => {
                path.pop();
            }
            other => path.push(other),
        }
        path
    });
    match fs::canonicalize(value) {
        Ok(resolved) if fs::canonicalize(&spelt).ok().as_ref() != Some(&resolved) => Some(resolved),
        _ => Some(spelt),
    }
}

impl<W: Write> Proxy<W> {
    fn new(client: W, server: Option<Server>, absent: Option<String>, root: Option<PathBuf>) -> Self {
        let mut proxy = Proxy {
            client,
            server,
            standalone: false,
            absent,
            root: None,
            canonical_root: None,
            encoding: Encoding::Utf16,
            hierarchical: false,
            owned: HashSet::new(),
            initialized: false,
            initialize: None,
            forwarded: HashMap::new(),
            documents: HashMap::new(),
            uris: HashMap::new(),
            opened: false,
            next_id: 0,
            listing: None,
            listed: Vec::new(),
            attempts: 0,
            relist: false,
            nodes: None,
            shutdown: false,
            markdown: false,
            hint_refresh: false,
            asked: HashSet::new(),
            diagnostics: Diagnostics::default(),
            restart: Restart::default(),
            parsing: Parsing::default(),
        };
        if let Some(root) = root {
            proxy.set_root(root);
        }
        proxy
    }

    fn set_root(&mut self, root: PathBuf) {
        self.canonical_root = fs::canonicalize(&root).ok();
        self.root = Some(root);
    }

    fn send_client(&mut self, message: &Value) {
        // A client that stopped reading also closes its output: the loop ends there.
        let _ = rpc::write_value(&mut self.client, message);
    }

    fn send_client_body(&mut self, body: &[u8]) {
        let _ = rpc::write_body(&mut self.client, body);
    }

    fn send_server(&mut self, body: &[u8]) {
        if let Some(server) = &mut self.server {
            // A failed write means the server is gone; its reader reports it.
            let _ = rpc::write_body(&mut server.stdin, body);
        }
    }

    fn reply_error(&mut self, id: &Value, code: i64, message: &str) {
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    /// Adds to CAPABILITIES each capability of ADDED that it lacks, and owns its methods.
    fn add_capabilities(&mut self, capabilities: &mut serde_json::Map<String, Value>) {
        for (capability, methods) in ADDED {
            if capabilities.get(capability).is_none_or(|value| value.is_null() || *value == json!(false)) {
                capabilities.insert(capability.to_owned(), json!(true));
                self.owned.extend(methods);
            }
        }
        // Which requests the proxy answers itself, project-wide, on which kinds
        // of target, beside the server's own experimental capabilities: a
        // client such as dbt-mode steps aside for exactly those.
        let experimental = capabilities.entry("experimental").or_insert(Value::Null);
        if experimental.is_null() {
            *experimental = json!({});
        }
        if let Some(experimental) = experimental.as_object_mut() {
            let answered = json!({"version": 1, "references": ["source"], "rename": ["source", "cte"],
                                  "definition": ["source", "ref", "doc", "entry", "test"]});
            experimental.insert("dbtLspPlus".into(), answered);
        }
        // The proxy's own command, beside the server's.
        let provider = capabilities.entry("executeCommandProvider").or_insert_with(|| json!({}));
        if let Some(commands) = provider.as_object_mut().map(|provider| provider.entry("commands").or_insert(json!([])))
        {
            commands.as_array_mut().into_iter().for_each(|commands| commands.push(json!(config::NODE_INFO)));
        }
    }

    /// Answers `initialize` without a server: the proxy's own features only.
    fn initialize_alone(&mut self, id: &Value) {
        let mut capabilities = serde_json::Map::new();
        capabilities.insert("textDocumentSync".into(), json!({"openClose": true, "change": 2}));
        // Alone, the proxy still answers references and renames of sources,
        // and definitions on the targets of `navigation`.
        capabilities.insert("referencesProvider".into(), json!(true));
        capabilities.insert("renameProvider".into(), json!(true));
        capabilities.insert("definitionProvider".into(), json!(true));
        // Alone for good, it wants saves, which start a parse, and answers
        // hovers and hints from the parse alone.
        if self.standalone {
            capabilities.insert("textDocumentSync".into(), json!({"openClose": true, "change": 2, "save": {}}));
            capabilities.insert("hoverProvider".into(), json!(true));
            capabilities.insert("inlayHintProvider".into(), json!(true));
            self.owned.extend(["textDocument/hover", "textDocument/inlayHint"]);
        }
        self.add_capabilities(&mut capabilities);
        self.watch_runs();
        let info = json!({"name": "dbt-lsp-plus", "version": env!("CARGO_PKG_VERSION")});
        self.send_client(
            &json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": capabilities, "serverInfo": info}}),
        );
        // A dbt without `lsp` is no failure: said once, out of the user's way.
        if self.standalone {
            let dbt = self.parsing.server.iter().take_while(|word| *word != "lsp").cloned().collect::<Vec<_>>();
            let text = format!("dbt-lsp-plus: {} has no lsp: dbt-lsp-plus answers alone.", dbt.join(" "));
            let params = json!({"type": 3, "message": text});
            return self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": params}));
        }
        let why = self.absent.take().unwrap_or_else(|| "dbt lsp is not running".into());
        let text = format!("{why}: dbt-lsp-plus serves only its own features: symbols, lineage, folding, renames.");
        self.send_client(
            &json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": text}}),
        );
    }

    /// Learns from the client's `initialize`: the outline's shape, and the
    /// root when the server command named none, or the client's spelling of
    /// the one it named.
    fn on_initialize(&mut self, params: &Value) {
        self.restart.initialize = Some(params.clone());
        self.hierarchical = params
            .pointer("/capabilities/textDocument/documentSymbol/hierarchicalDocumentSymbolSupport")
            == Some(&json!(true));
        let formats = params.pointer("/capabilities/textDocument/hover/contentFormat").and_then(Value::as_array);
        // The client lists its formats in its order of preference.
        let first =
            formats.and_then(|formats| formats.iter().find(|format| *format == "markdown" || *format == "plaintext"));
        self.markdown = first.is_some_and(|format| format == "markdown");
        self.hint_refresh = params.pointer("/capabilities/workspace/inlayHint/refreshSupport") == Some(&json!(true));
        let folder = params.pointer("/workspaceFolders/0/uri").or_else(|| params.get("rootUri"));
        let folder = uri_path(folder.and_then(Value::as_str)).filter(|folder| folder.join("dbt_project.yml").is_file());
        let same = |folder: &Path| fs::canonicalize(folder).ok() == self.canonical_root;
        match (folder, &self.root) {
            (Some(folder), None) => self.set_root(folder),
            (Some(folder), Some(_)) if same(&folder) => self.set_root(folder),
            (None, None) => {
                if let Ok(dir) = std::env::current_dir() {
                    self.set_root(dir);
                }
            }
            _ => {}
        }
    }

    /// The path of a `file:` URI, mapped onto the root when it reaches the
    /// same file through another spelling of its directory: a link, a
    /// canonical path.  A path under the root as spelt stays as it is, links
    /// within the project included, since the scan walks them so.
    fn local_path(&self, uri: Option<&str>) -> Option<PathBuf> {
        let path = uri_path(uri)?;
        let mapped = || {
            let root = self.root.as_ref()?;
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
        // Where the server has diagnostics, their ranges are of the text
        // before the edit, which the client has moved: its next publication
        // drops the failures instead.  An empty set has no range to move.
        if matches!(method, "textDocument/didChange" | "textDocument/didSave") {
            self.diagnostics.edited.insert(path.clone(), SystemTime::now());
            // A server gone publishes no more: its old set is no reason to wait.
            let server = self.diagnostics.server.get(&path).filter(|_| self.server.is_some());
            let ranged = server.is_some_and(|(_, diagnostics)| !diagnostics.is_empty());
            if self.diagnostics.failures.remove(&path).is_some() && !ranged {
                self.publish(&path);
            }
        }
        match method {
            "textDocument/didOpen" => {
                let text = params.pointer("/textDocument/text").and_then(Value::as_str).unwrap_or_default();
                self.documents.insert(path.clone(), text.to_owned());
                self.restart.languages.insert(path.clone(), params["textDocument"]["languageId"].clone());
                self.restart.versions.insert(path.clone(), params["textDocument"]["version"].clone());
                self.uris.insert(path, uri.unwrap_or_default().to_owned());
            }
            "textDocument/didChange" => {
                self.restart.versions.insert(path.clone(), params["textDocument"]["version"].clone());
                let changes = params.get("contentChanges").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
                if let Some(text) = self.documents.get_mut(&path) {
                    changes.iter().for_each(|change| text::apply_change(text, change, self.encoding));
                }
            }
            "textDocument/didClose" => {
                self.restart.languages.remove(&path);
                self.restart.versions.remove(&path);
                self.documents.remove(&path);
                self.uris.remove(&path);
            }
            _ => {}
        }
    }

    /// The project as its files and the open documents say, if there is one.
    fn project(&self) -> Option<Project> {
        project::scan(self.root.as_deref()?, &self.documents)
    }

    /// The outline of a document, from its open text or its file; null for a
    /// document that is neither.
    fn outline(&self, params: &Value) -> Option<Value> {
        let uri = params.pointer("/textDocument/uri")?.as_str()?;
        let text = self.text_of(uri).map(|(_, text)| text);
        Some(text.map_or(Value::Null, |text| features::document_symbols(&text, self.encoding, self.hierarchical, uri)))
    }

    /// The path of the document at URI and its text, open or on disk.
    fn text_of(&self, uri: &str) -> Option<(PathBuf, String)> {
        let path = self.local_path(Some(uri))?;
        let text = self.documents.get(&path).cloned().or_else(|| text::read(&path))?;
        Some((path, text))
    }

    /// The answer to the call hierarchy's METHOD, on the graph of the files
    /// with the server's nodes once listed, or, alone, the last parse's.
    fn hierarchy(&self, method: &str, params: &Value) -> Option<Value> {
        let Some(project) = self.project() else { return Some(Value::Null) };
        let scanned = Graph::scanned(&project);
        let parsed = self.parsing.index.as_ref().filter(|_| self.standalone).map(config::Index::listed);
        let graph = match parsed.as_ref().or(self.nodes.as_ref()) {
            Some(nodes) => scanned.with_server(nodes, &project),
            None => scanned,
        };
        if method != "textDocument/prepareCallHierarchy" {
            let id = params.pointer("/item/data/uniqueId")?.as_str()?;
            let incoming = method == "callHierarchy/incomingCalls";
            return Some(features::calls(&project, &graph, id, incoming, self.encoding));
        }
        // A document that is no file, such as an unsaved buffer, holds no resource.
        let Some(path) = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str)) else {
            return Some(Value::Null);
        };
        let text = project.texts.get(&path).map_or("", String::as_str);
        let offset = LineIndex::new(text).offset_of(params.get("position")?, self.encoding)?;
        Some(features::prepare_call_hierarchy(&project, &graph, &path, offset, self.encoding))
    }

    /// The document of a request, its text, open or on disk, and the offset
    /// of the request's position in it.
    fn document_at(&self, params: &Value) -> Option<(PathBuf, String, usize)> {
        let (path, text) = self.text_of(params.pointer("/textDocument/uri")?.as_str()?)?;
        let offset = LineIndex::new(&text).offset_of(params.get("position")?, self.encoding)?;
        Some((path, text, offset))
    }

    /// The source named at the position of a request on a document, with
    /// the project scanned.  Only the scan holds every occurrence: on a
    /// document it does not hold, such as a file not saved yet or one
    /// outside the resource paths, there is none, and the request goes to
    /// the server.
    fn source_at(&self, params: &Value) -> Option<(Project, sources::Target)> {
        let (path, text, offset) = self.document_at(params)?;
        let target = sources::target_at(&path, &text, offset)?;
        let project = self.project().filter(|project| project.texts.contains_key(&path))?;
        Some((project, target))
    }

    /// The method under which the proxy answers a request itself: an owned
    /// method, its own command, or a targeted request on one of its targets.
    fn local_method(&self, method: &str, params: &Value) -> Option<&'static str> {
        if let Some(owned) = self.owned.get(method) {
            return Some(owned);
        }
        if method == "workspace/executeCommand" && params["command"] == config::NODE_INFO {
            return Some(config::NODE_INFO);
        }
        self.targeted(method, params)
    }

    /// A request of `TARGETED` that the proxy answers itself, by what its
    /// position holds: references and rename on a source, a rename on a CTE
    /// of a SQL file, or a definition on a target of `navigation`.
    fn targeted(&self, method: &str, params: &Value) -> Option<&'static str> {
        let method = TARGETED.into_iter().find(|targeted| *targeted == method)?;
        let (path, text, offset) = self.document_at(params)?;
        let sql = path.extension().is_some_and(|extension| extension == "sql");
        let answered = match method {
            "textDocument/definition" => navigation::target_at(&path, &text, offset).is_some(),
            // A CTE lives in its file; a source only the scan sees whole.
            "textDocument/rename" if sql && document::cte_at(&text, offset).is_some() => true,
            _ => self.source_at(params).is_some(),
        };
        answered.then_some(method)
    }

    /// The definition of the target at the request's position, by dbt
    /// 2.0.9's lookup: its location, several, or null.
    fn on_definition(&self, params: &Value) -> Option<Result<Value, String>> {
        let (path, text, offset) = self.document_at(params)?;
        let target = navigation::target_at(&path, &text, offset);
        let (Some(target), Some(project)) = (target, self.project()) else { return Some(Ok(Value::Null)) };
        let uri_of = |path: &Path| self.uris.get(path).cloned().unwrap_or_else(|| features::uri(path));
        let locations: Vec<Value> = navigation::resolve(&project, &path, &target)
            .iter()
            .map(|found| {
                let index = LineIndex::new(project.texts.get(&found.path).map_or("", String::as_str));
                let range = index.range(found.range.start, found.range.end, self.encoding);
                json!({"uri": uri_of(&found.path), "range": range})
            })
            .collect();
        Some(Ok(match &locations[..] {
            [] => Value::Null,
            [location] => location.clone(),
            _ => Value::Array(locations),
        }))
    }

    /// The references or the rename of the source at the request's position.
    fn on_source(&self, method: &str, params: &Value) -> Option<Result<Value, String>> {
        let Some((project, target)) = self.source_at(params) else { return Some(Ok(Value::Null)) };
        let uri_of = |path: &Path| self.uris.get(path).cloned().unwrap_or_else(|| features::uri(path));
        if method == "textDocument/references" {
            let declarations = params.pointer("/context/includeDeclaration").and_then(Value::as_bool) == Some(true);
            return Some(Ok(sources::references(&project, &target, declarations, self.encoding, &uri_of)));
        }
        let new = params.get("newName")?.as_str()?;
        Some(sources::rename(&project, &target, new, self.encoding, &uri_of))
    }

    /// The rename of the source, else of the CTE, at the request's position.
    fn on_rename(&self, params: &Value) -> Option<Result<Value, String>> {
        if self.source_at(params).is_some() {
            return self.on_source("textDocument/rename", params);
        }
        let (_, text, offset) = self.document_at(params)?;
        let (uri, new) = (params.pointer("/textDocument/uri")?.as_str()?, params.get("newName")?.as_str()?);
        Some(document::rename_cte(&text, uri, offset, new, self.encoding).unwrap_or(Ok(Value::Null)))
    }

    /// The folding ranges, highlights or selection ranges of a document;
    /// null for a document neither open nor on disk.
    fn on_document(&self, method: &str, params: &Value) -> Option<Value> {
        let uri = params.pointer("/textDocument/uri")?.as_str()?;
        let sql = uri.ends_with(".sql");
        if method == "textDocument/documentHighlight" {
            let Some((_, text, offset)) = self.document_at(params) else { return Some(Value::Null) };
            return Some(document::highlights(&text, sql, offset, self.encoding));
        }
        // A YAML file folds and selects best by its indentation, which the client knows.
        let Some((_, text)) = self.text_of(uri).filter(|_| sql || uri.ends_with(".md")) else {
            return Some(Value::Null);
        };
        if method == "textDocument/foldingRange" {
            return Some(document::folding_ranges(&text, sql));
        }
        let index = LineIndex::new(&text);
        let positions = params.get("positions")?.as_array()?.iter().map(|p| index.offset_of(p, self.encoding));
        Some(document::selection_ranges(&text, sql, &positions.collect::<Option<Vec<usize>>>()?, self.encoding))
    }

    /// The node of the `ref` or `source` at the position of PARAMS, or of the
    /// entry whose `name:` is there, else, when FILE, the one node its
    /// document defines.
    fn node_at<'a>(&self, index: &'a config::Index, params: &Value, file: bool) -> Option<&'a Value> {
        let (path, text, offset) = self.document_at(params)?;
        let inside =
            |call: &scan::Call| call.arguments.iter().any(|range| range.start <= offset && offset <= range.end);
        // YAML and a Python model call outside Jinja, as plan 002 reads them.
        let plain =
            path.extension().is_some_and(|extension| ["yml", "yaml", "py"].contains(&&*extension.to_string_lossy()));
        let calls = if plain { scan::yaml_calls(&text) } else { scan::calls(&text) };
        if let Some(call) = calls.into_iter().find(inside) {
            return index.named(&call.target);
        }
        // An entry's `name:` stands for its node, looked up as dbt 2.0.9 does.
        let entry = navigation::target_at(&path, &text, offset).filter(|target| {
            matches!(target, navigation::Target::Named { kind: "model" | "seed" | "snapshot" | "analysis", .. })
                || matches!(target, navigation::Target::Source(sources::Target::Table { .. }))
        });
        if let Some(target) = entry {
            let project = self.project()?;
            return navigation::resolve(&project, &path, &target).first().and_then(|found| index.node(&found.id));
        }
        let relative = path.strip_prefix(self.root.as_ref()?).ok().filter(|_| file)?;
        index.of_file(&relative.to_string_lossy())
    }

    /// The information of a node, named by its id or by a position: the
    /// manifest's node with its relation, null for no node; a failure while
    /// no parse has finished.
    fn node_info(&self, params: &Value) -> Option<Result<Value, String>> {
        let Some(index) = &self.parsing.index else {
            let why = match (self.parsing.running.is_some(), &self.parsing.failure) {
                (true, _) => "it is running",
                (false, Some(failure)) => failure,
                (false, None) => "waiting for dbt lsp to compile the project",
            };
            return Some(Err(format!("dbt parse has not finished: {why}")));
        };
        let argument = params.pointer("/arguments/0")?;
        let node = match argument.as_str() {
            Some(id) => index.node(id),
            None => {
                let position = json!({"textDocument": {"uri": argument["uri"]}, "position": argument["position"]});
                self.node_at(index, &position, true)
            }
        };
        Some(Ok(node.cloned().unwrap_or(Value::Null)))
    }

    /// Starts a parse when the index is stale and dbt can parse: alone, or
    /// with a server that compiled since the last change; packages that need
    /// no network, no parse under way, no shutdown, a server command with
    /// `lsp`.
    fn want_index(&mut self) {
        // A listing makes dbt lsp compile, as the first document opened does.
        if !self.parsing.compiled && self.listing.is_none() && !self.shutdown {
            self.list_nodes();
        }
        self.parsing.wanted |= !self.parsing.compiled;
        let parsing = &self.parsing;
        // Alone, no server resolves the packages first: their guard is enough.
        let can = self.standalone || (parsing.compiled && self.server.is_some());
        let ready = !parsing.fresh && can && parsing.running.is_none();
        let Some(root) = self.root.clone().filter(|_| ready && !self.shutdown) else { return };
        if let Some(why) = config::packages_refused(&root, &self.parsing.server) {
            return self.log_parse(&why);
        }
        // An empty or relative base would put the cache in the working directory.
        let absolute = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute());
        let base = absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")));
        let server = &self.parsing.server;
        let cache = self.parsing.cache.get_or_insert_with(|| {
            let cache = config::cache_dir(&base.unwrap_or_else(std::env::temp_dir), &root, server);
            config::remove_stale(&cache);
            cache
        });
        let Some(command) = config::parse_command(server, &root, cache, self.parsing.cut) else {
            return self.log_parse("the server command has no lsp, so dbt-lsp-plus runs no dbt parse");
        };
        let (Some(events), cache) = (self.parsing.events.clone(), cache.clone()) else { return };
        self.parsing.fresh = true;
        let report = Box::new(move |outcome| drop(events.send(Event::Parsed(outcome))));
        self.parsing.running = Some(config::start(command, cache, config::PARSE_DEADLINE, report));
    }

    /// Logs WHY a parse failed or could not start, out of the user's way,
    /// once until the reason changes.
    fn log_parse(&mut self, why: &str) {
        // After `shutdown`, LSP lets the server send nothing but answers.
        if self.parsing.failure.as_deref() != Some(why) && !self.shutdown {
            let params = json!({"type": 2, "message": format!("dbt-lsp-plus: {why}")});
            self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": params}));
        }
        self.parsing.failure = Some(why.to_owned());
    }

    /// A parse's outcome: its index replaces the last, or, from a parse that
    /// failed part of the way, lays its entries over the last.
    fn on_parsed(&mut self, outcome: config::Outcome) {
        if let Some(running) = self.parsing.running.take() {
            running.stop();
        }
        if outcome.index.is_some() {
            self.refresh_hints();
        }
        match (outcome.index, self.parsing.index.as_mut()) {
            (Some(index), Some(last)) if outcome.failure.is_some() => last.overlay(index),
            (Some(index), _) => self.parsing.index = Some(index),
            (None, _) => {}
        }
        match outcome.failure {
            Some(failure) => self.log_parse(&format!("dbt parse failed: {failure}")),
            None => self.parsing.failure = None,
        }
        // Alone, a change during this parse wants the next one now: no
        // request may come to ask for it.
        if self.standalone {
            self.want_index();
        }
    }

    /// Asks a client that can refresh its inlay hints to do so, since they
    /// show the relations of the index, under an id of the proxy's own,
    /// whose answer stays here.
    fn refresh_hints(&mut self) {
        if !self.hint_refresh || self.shutdown {
            return;
        }
        self.next_id += 1;
        let id = json!(format!("{OWN_ID}{}", self.next_id));
        self.asked.insert(key(&id));
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "method": "workspace/inlayHint/refresh"}));
    }

    /// Stops a parse under way and removes this process's cache.
    fn stop_parse(&mut self) {
        if let Some(running) = self.parsing.running.take() {
            running.stop();
        }
        if let Some(cache) = self.parsing.cache.take() {
            let _ = fs::remove_dir_all(cache);
        }
    }

    /// Answers a request that the proxy answers itself, which it remembers as
    /// PENDING.  A refusal fails it with its reason; a panic while answering
    /// fails that request alone, never the session.
    fn answer(&mut self, method: &str, id: &Value, params: &Value, pending: &Pending) {
        // nodeInfo needs the index; alone, so does the call hierarchy's graph.
        let hierarchy = method.ends_with("CallHierarchy") || method.starts_with("callHierarchy/");
        if method == config::NODE_INFO || (self.standalone && hierarchy) {
            self.want_index();
        }
        let computed = panic::catch_unwind(AssertUnwindSafe(|| match method {
            "textDocument/documentSymbol" => self.outline(params).map(Ok),
            "workspace/symbol" => params.get("query").and_then(Value::as_str).map(|query| {
                Ok(self
                    .project()
                    .map_or(json!([]), |project| features::workspace_symbols(&project, query, self.encoding)))
            }),
            config::NODE_INFO => self.node_info(params),
            "textDocument/references" => self.on_source(method, params),
            "textDocument/definition" => self.on_definition(params),
            "textDocument/rename" => self.on_rename(params),
            "textDocument/foldingRange" | "textDocument/documentHighlight" | "textDocument/selectionRange" => {
                self.on_document(method, params).map(Ok)
            }
            // As dbt lsp 2.0.9's answer with nothing of its own gets them,
            // null for a hover, no hint: the proxy's part, else that.
            "textDocument/hover" | "textDocument/inlayHint" => {
                let nothing = json!({"textDocument/hover": null, "textDocument/inlayHint": []});
                let merged = pending.merged(&json!({"result": nothing[method]}), self.markdown);
                Some(Ok(merged.map_or_else(|| nothing[method].clone(), |answer| answer["result"].clone())))
            }
            _ => self.hierarchy(method, params).map(Ok),
        }));
        match computed {
            Ok(Some(Ok(result))) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Ok(Some(Err(reason))) => self.reply_error(id, REQUEST_FAILED, &reason),
            Ok(None) => self.reply_error(id, INVALID_PARAMS, "invalid parameters"),
            Err(_) => self.reply_error(id, INTERNAL_ERROR, "dbt-lsp-plus failed to answer this request"),
        }
    }

    fn client_listing(&self) -> bool {
        self.forwarded.values().any(|(_, pending)| *pending == Pending::Listing)
    }

    /// Asks the server for the graph, unless a listing is under way, the
    /// proxy's own or the client's, or there is no server.
    fn list_nodes(&mut self) {
        if self.listing.is_some() || self.client_listing() {
            self.relist = true;
        } else if self.server.is_some() {
            self.send_list(0);
        }
    }

    /// Sends the listing of selector SLOT, under an id that no pending
    /// request of the client's carries; nothing once the client asked for
    /// shutdown, after which LSP allows only `exit`.
    fn send_list(&mut self, slot: usize) {
        if self.shutdown {
            return;
        }
        let id = loop {
            self.next_id += 1;
            let id = format!("{OWN_ID}{}", self.next_id);
            if !self.forwarded.contains_key(&key(&json!(id))) {
                break id;
            }
        };
        let params = json!({"command": "dbt.listNodes", "arguments": [SELECTORS[slot]]});
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "workspace/executeCommand", "params": params});
        self.listing = Some((id, slot, Instant::now()));
        self.send_server(&serde_json::to_vec(&request).unwrap_or_default());
    }

    /// Lists the graph if a change came while it could not be.
    fn resume(&mut self) {
        if std::mem::take(&mut self.relist) {
            self.list_nodes();
        }
    }

    /// Abandons a listing unanswered for LIST_TIMEOUT; the graph stays.
    fn expire_listing(&mut self) {
        if self.listing.as_ref().is_some_and(|(_, _, sent)| sent.elapsed() >= LIST_TIMEOUT) {
            self.listing = None;
            self.listed.clear();
            self.attempts = 0;
            self.resume();
        }
    }

    /// Takes the server's answer to the proxy's listing in flight; the answer
    /// to an abandoned one is dropped.  A failed listing keeps the graph.
    fn on_own_response(&mut self, id: &str, message: &Value) {
        if self.restart.starting.as_ref().is_some_and(|(_, own, _)| own == id) {
            return self.on_restarted(message);
        }
        let Some((_, slot, _)) = self.listing.take_if(|(own, ..)| own == id) else { return };
        let result = message.get("result").unwrap_or(&Value::Null);
        if result.get("error") == Some(&json!("Operation cancelled")) && self.attempts < MAX_ATTEMPTS {
            self.attempts += 1;
            self.listed.clear();
            self.relist = true;
            return self.resume();
        }
        // `*` lists nothing only when the project failed to compile: the scan found resources.
        let listed = graph::parse_list_nodes(result).into_nodes().filter(|nodes| slot == 1 || !nodes.is_empty());
        match (slot, listed) {
            (0, Some(nodes)) if !self.client_listing() => {
                self.listed = nodes;
                return self.send_list(1);
            }
            // The client's own listing would cancel the second selector: start again after it.
            (0, Some(_)) => self.relist = true,
            (_, Some(sources)) => {
                let mut nodes = std::mem::take(&mut self.listed);
                nodes.extend(sources);
                self.nodes = Some(nodes);
            }
            (_, None) => self.listed.clear(),
        }
        self.attempts = 0;
        self.resume();
    }

    fn on_client(&mut self, frame: Frame) {
        let Some(message) = &frame.value else { return self.send_server(&frame.body) };
        let params = message.get("params").unwrap_or(&Value::Null);
        match (message.get("method").and_then(Value::as_str), message.get("id")) {
            (Some(method), Some(id)) => self.on_request(method, id, params, &frame.body),
            (Some(method), None) => self.on_notification(method, params, &frame.body),
            // The client's answer to the proxy's own request was never the server's.
            (None, Some(id)) if self.asked.remove(&key(id)) => {}
            _ => self.send_server(&frame.body),
        }
    }

    /// A notification of the client's, METHOD with PARAMS, BODY as it came:
    /// kept and forwarded; when the graph may have changed, a listing, and,
    /// alone, a parse, which nothing else would start.
    fn on_notification(&mut self, method: &str, params: &Value, body: &[u8]) {
        self.track(method, params);
        // The server gets the change before the listing it prompts.
        self.send_server(body);
        let first_open = method == "textDocument/didOpen" && !std::mem::replace(&mut self.opened, true);
        // dbt lsp's output, as spelt and as resolved: a link out of the project included.
        let output = || {
            let spelt = self.root.iter().flat_map(|root| OUTPUT.map(|dir| root.join(dir)));
            spelt.flat_map(|dir| [fs::canonicalize(&dir).ok(), Some(dir)]).flatten().collect()
        };
        if changes_graph(method, params, output, |uri| self.local_path(uri)) || first_open {
            (self.parsing.fresh, self.parsing.compiled) = (false, false);
            self.list_nodes();
            if self.standalone {
                self.want_index();
            }
        }
    }

    /// What the proxy keeps of the client's request METHOD, and the lifecycle
    /// it starts: the client's `initialize` and `shutdown`, a listing that the
    /// proxy must not cancel, the summary that a hover's answer gets.
    fn remember(&mut self, method: &str, id: &Value, params: &Value) -> Pending {
        match (method, params["command"].as_str()) {
            ("initialize", _) => {
                self.on_initialize(params);
                self.initialized = true;
                self.initialize = Some(id.clone());
                Pending::Request
            }
            ("shutdown", _) => {
                self.shutdown = true;
                Pending::Shutdown
            }
            (_, Some("dbt.listNodes")) => Pending::Listing,
            ("textDocument/hover", _) => {
                let index = self.parsing.index.as_ref();
                let summary =
                    index.and_then(|index| self.node_at(index, params, false).map(|node| (index.summary(node), true)));
                // A doc's or a generic test's block, the project scanned only then.
                let block = || {
                    let (path, text, offset) = self.document_at(params)?;
                    let target = navigation::target_at(&path, &text, offset)
                        .filter(|target| matches!(target, navigation::Target::Named { kind: "doc" | "test", .. }))?;
                    let project = self.project()?;
                    let found = navigation::resolve(&project, &path, &target);
                    navigation::hover(&project, &target, found.first()?, self.markdown).map(|text| (text, false))
                };
                summary.or_else(block).map_or(Pending::Request, |(text, hard)| Pending::Hover(text, hard))
            }
            ("textDocument/inlayHint", _) => {
                // A panic leaves the server's answer as it is.
                let hints = panic::catch_unwind(AssertUnwindSafe(|| {
                    let index = self.parsing.index.as_ref()?;
                    let (path, text) = self.text_of(params.pointer("/textDocument/uri")?.as_str()?)?;
                    let lines = LineIndex::new(&text);
                    let start = lines.offset_of(params.pointer("/range/start")?, self.encoding)?;
                    let end = lines.offset_of(params.pointer("/range/end")?, self.encoding)?;
                    let calls = sources::file_calls(&path, &text);
                    Some(features::inlay_hints(&text, calls, index, start..end, self.encoding))
                }));
                let hints = hints.ok().flatten().filter(|hints| !hints.is_empty());
                hints.map_or(Pending::Request, Pending::Hints)
            }
            _ => Pending::Request,
        }
    }

    /// A request of the client: answered here when the proxy owns its method,
    /// else forwarded; without a server, the lifecycle requests are answered
    /// as LSP asks, a definition with no target gets null, and any other
    /// fails.
    fn on_request(&mut self, method: &str, id: &Value, params: &Value, body: &[u8]) {
        // The server would get two requests under one id.
        if self.listing.as_ref().is_some_and(|(own, ..)| id.as_str() == Some(own)) {
            return self.reply_error(id, INVALID_REQUEST, "dbt-lsp-plus has a request in flight under this id");
        }
        let pending = self.remember(method, id, params);
        match (self.local_method(method, params), &self.server, method) {
            (Some(owned), ..) => self.answer(owned, id, params, &pending),
            (None, Some(_), _) => {
                self.forwarded.insert(key(id), (id.clone(), pending));
                self.send_server(body);
            }
            (None, None, "initialize") => {
                self.initialize = None;
                self.initialize_alone(id);
            }
            // Alone, a definition with no target is null, not a failure.
            (None, None, "shutdown" | "textDocument/definition") => {
                self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": null}))
            }
            (None, None, _) => self.reply_error(id, REQUEST_FAILED, "dbt lsp is not running"),
        }
        // Once the request is forwarded, so that a listing takes no id of the
        // client's: a hover keeps the index fresh, an inlay hint only makes one.
        if method == "textDocument/hover" || (method == "textDocument/inlayHint" && self.parsing.index.is_none()) {
            self.want_index();
        }
    }

    /// A message of the server.  A response is the proxy's own only when its
    /// id has the proxy's prefix and answers no request of the client's.
    fn on_server(&mut self, frame: Frame) {
        let Some(message) = &frame.value else { return self.send_client_body(&frame.body) };
        match (message.get("method").and_then(Value::as_str), message.get("id")) {
            (None, Some(id)) => self.on_server_response(id, message, &frame.body),
            // The server has resolved the packages: dbt may parse, for a
            // request that wanted an index before.
            (Some("dbt/lspCompileComplete"), _) => {
                self.parsing.compiled = true;
                if std::mem::take(&mut self.parsing.wanted) {
                    self.want_index();
                }
                self.send_client_body(&frame.body);
            }
            (Some("textDocument/publishDiagnostics"), None) => self.on_server_diagnostics(message, &frame.body),
            (Some("client/registerCapability"), Some(id)) => self.on_registration(id, message, &frame.body),
            (Some("$/progress"), None) => self.on_progress(message, &frame.body),
            _ => self.send_client_body(&frame.body),
        }
    }

    /// The server's `$/progress`, BODY as it came, forwarded: the tokens it
    /// begins and has not ended are kept, since a server that dies ends none.
    fn on_progress(&mut self, message: &Value, body: &[u8]) {
        let (token, kind) = (&message["params"]["token"], message.pointer("/params/value/kind"));
        match kind.and_then(Value::as_str) {
            Some("begin") => drop(self.restart.progress.insert(key(token), token.clone())),
            Some("end") => drop(self.restart.progress.remove(&key(token))),
            _ => {}
        }
        self.send_client_body(body);
    }

    /// The server's `client/registerCapability`, BODY as it came: forwarded,
    /// unless the client already holds every registration it names, as after
    /// a restart, when the proxy answers it.
    fn on_registration(&mut self, id: &Value, message: &Value, body: &[u8]) {
        let registrations = message.pointer("/params/registrations").and_then(Value::as_array);
        let ids: Vec<String> =
            registrations.into_iter().flatten().filter_map(|r| r["id"].as_str()).map(str::to_owned).collect();
        if !ids.is_empty() && ids.iter().all(|id| self.restart.registered.contains(id)) {
            let answer = json!({"jsonrpc": "2.0", "id": id, "result": null}).to_string();
            return self.send_server(answer.as_bytes());
        }
        self.restart.registered.extend(ids);
        self.send_client_body(body);
    }

    /// A response of the server, BODY as it came: to a request of the
    /// client's, forwarded as its pending kind makes it, or to one of the
    /// proxy's own, kept.
    fn on_server_response(&mut self, id: &Value, message: &Value, body: &[u8]) {
        let pending = self.forwarded.remove(&key(id));
        if let (None, Some(own)) = (&pending, id.as_str())
            && own.starts_with(OWN_ID)
        {
            return self.on_own_response(own, message);
        }
        if let Some((_, Pending::Listing)) = pending {
            self.resume();
        }
        if let Some(merged) = pending.as_ref().and_then(|(_, kind)| kind.merged(message, self.markdown)) {
            return self.send_client(&merged);
        }
        if self.initialize.as_ref() == Some(id) {
            self.initialize = None;
            let mut message = message.clone();
            let encoding = message.pointer("/result/capabilities/positionEncoding").and_then(Value::as_str);
            self.encoding = Encoding::parse(encoding);
            if let Some(capabilities) = message.pointer_mut("/result/capabilities").and_then(Value::as_object_mut) {
                self.add_capabilities(capabilities);
            }
            self.send_client(&message);
            return self.watch_runs();
        }
        self.send_client_body(body);
    }

    /// The server's diagnostics of a file, BODY as it came: forwarded as
    /// they are, unless the last run failed there too.
    fn on_server_diagnostics(&mut self, message: &Value, body: &[u8]) {
        let uri = message.pointer("/params/uri").and_then(Value::as_str);
        let Some(path) = self.local_path(uri) else { return self.send_client_body(body) };
        let diagnostics = message.pointer("/params/diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
        self.diagnostics.server.insert(path.clone(), (uri.unwrap_or_default().to_owned(), diagnostics));
        if !self.diagnostics.failures.contains_key(&path) {
            self.diagnostics.published.insert(path, uri.unwrap_or_default().to_owned());
            return self.send_client_body(body);
        }
        self.publish(&path);
    }

    /// Watches the files of the user's last run, in the target path
    /// (`DBT_TARGET_PATH`, else `target` under the root), from a thread
    /// that reads them when their size or time changes and reports their
    /// failures to the loop.
    fn watch_runs(&mut self) {
        let (Some(root), Some(events)) = (self.root.clone(), self.parsing.events.clone()) else { return };
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
        changed.iter().for_each(|path| self.publish(path));
    }

    /// Publishes PATH's diagnostics, the server's last and the proxy's own,
    /// under the server's URI for it, else the client's, else the file's,
    /// with no version; the URI last published under, if another, gets
    /// none.  Nothing after `shutdown`.
    fn publish(&mut self, path: &Path) {
        if self.shutdown {
            return;
        }
        let server = self.diagnostics.server.get(path);
        let uri = server.map(|(uri, _)| uri.clone());
        let uri = uri.or_else(|| self.uris.get(path).cloned()).unwrap_or_else(|| features::uri(path));
        let mut diagnostics = server.map(|(_, diagnostics)| diagnostics.clone()).unwrap_or_default();
        // The failures, placed in the text as it stands.
        let failures = self.diagnostics.failures.get(path).map_or(&[][..], Vec::as_slice);
        let text = self.documents.get(path).cloned().or_else(|| text::read(path)).unwrap_or_default();
        let lines = LineIndex::new(&text);
        let (entries, tests) = (properties::entries(&text), properties::tests(&text));
        diagnostics.extend(failures.iter().map(|failure| {
            let range = runs::locate(&text, &entries, &tests, &failure.place);
            json!({"range": lines.range(range.start, range.end, self.encoding), "severity": failure.severity,
                   "source": "dbt-lsp-plus", "code": failure.code, "message": failure.message})
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

    /// The server is gone: answer what it left unanswered, once each, and
    /// carry on alone, on the scanned graph.  Before the client's
    /// `initialize`, only remember why, for its answer.
    fn on_server_closed(&mut self) {
        let starting = || self.restart.starting.take().map(|(server, ..)| server);
        let Some(mut server) = self.server.take().or_else(starting) else { return };
        let status = server.child.try_wait().ok().flatten();
        if status.is_none() {
            let _ = server.child.kill();
        }
        let status = status.or_else(|| server.child.wait().ok());
        let why = format!("dbt lsp exited ({})", status.map_or("unknown status".into(), |s| s.to_string()));
        if let Some(id) = &self.initialize {
            self.forwarded.remove(&key(id));
        }
        for (_, (id, pending)) in std::mem::take(&mut self.forwarded) {
            match pending {
                Pending::Shutdown => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": null})),
                _ => self.reply_error(&id, REQUEST_FAILED, &why),
            }
        }
        self.listing = None;
        self.attempts = 0;
        self.listed.clear();
        self.relist = false;
        self.nodes = None;
        self.absent = Some(why.clone());
        if let Some(id) = self.initialize.take() {
            return self.initialize_alone(&id);
        }
        self.lost_server(&why);
    }

    /// The server lost for WHY during a session: the progress it began is
    /// ended, and a restart follows after its delay, else the proxy is alone
    /// from now on; the client is told which.
    fn lost_server(&mut self, why: &str) {
        if !self.initialized || self.shutdown {
            return;
        }
        for (_, token) in std::mem::take(&mut self.restart.progress) {
            self.send_client(&json!({"jsonrpc": "2.0", "method": "$/progress",
                                     "params": {"token": token, "value": {"kind": "end"}}}));
        }
        let now = Instant::now();
        self.restart.past.retain(|at| now.saturating_duration_since(*at) < RESTART_WINDOW);
        let delay = restart_delay(&self.restart.past, now);
        self.restart.due = delay.map(|delay| now + delay);
        let text = match delay {
            Some(delay) => format!("{why}: restarting it in {} s.", delay.as_secs()),
            None => format!("{why}: dbt-lsp-plus keeps its own features: symbols, lineage, folding, renames."),
        };
        self.send_client(
            &json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 1, "message": text}}),
        );
    }

    /// Starts the server again with the command of the session, and sends it
    /// the client's `initialize` under an id of the proxy's own; a failure to
    /// start it is a server lost again.
    fn start_restart(&mut self) {
        let (Some(params), Some(events)) = (self.restart.initialize.clone(), self.parsing.events.clone()) else {
            return;
        };
        if self.shutdown {
            return;
        }
        self.restart.past.push(Instant::now());
        self.restart.spawned += 1;
        match spawn_server(&self.parsing.server, &events, self.restart.spawned) {
            Ok(mut server) => {
                self.next_id += 1;
                let id = format!("{OWN_ID}{}", self.next_id);
                let initialize = json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params});
                let _ = rpc::write_value(&mut server.stdin, &initialize);
                self.restart.starting = Some((server, id, Instant::now() + REPLAY_TIMEOUT));
            }
            Err(why) => self.lost_server(&why),
        }
    }

    /// The restarted server's answer to its `initialize`, MESSAGE: it becomes
    /// the server, initialized, given the client's open documents as they
    /// stand, and listed; refused, or after `shutdown`, it is stopped.
    fn on_restarted(&mut self, message: &Value) {
        let Some((mut server, ..)) = self.restart.starting.take() else { return };
        if message.get("error").is_some() || self.shutdown {
            let _ = server.child.kill();
            let _ = server.child.wait();
            return self.lost_server("dbt lsp refused its initialize");
        }
        self.server = Some(server);
        self.absent = None;
        self.send_server(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}).to_string().as_bytes());
        let mut documents: Vec<(&PathBuf, &String)> = self.documents.iter().collect();
        documents.sort();
        let opened: Vec<Value> = documents
            .into_iter()
            .map(|(path, text)| {
                let document = json!({"uri": self.uris.get(path), "languageId": self.restart.languages.get(path),
                                      "version": self.restart.versions.get(path), "text": text});
                json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": document}})
            })
            .collect();
        opened.iter().for_each(|open| self.send_server(open.to_string().as_bytes()));
        self.list_nodes();
    }

    /// What is due when the loop wakes with no event: the listing's deadline,
    /// a restart after its delay, a restarted server that did not answer.
    fn on_time(&mut self) {
        self.expire_listing();
        let now = Instant::now();
        if self.restart.due.take_if(|due| *due <= now).is_some() {
            self.start_restart();
        }
        if let Some((mut server, ..)) = self.restart.starting.take_if(|(.., deadline)| *deadline <= now) {
            let _ = server.child.kill();
            let _ = server.child.wait();
            self.lost_server("dbt lsp did not answer its initialize");
        }
    }

    /// Handles the events of INBOX until `exit`, or until the client closes
    /// its output; a listing past its deadline is abandoned on time.
    fn serve(&mut self, inbox: Receiver<Event>) {
        loop {
            let listing = self.listing.as_ref().map(|(_, _, sent)| *sent + LIST_TIMEOUT);
            let replay = self.restart.starting.as_ref().map(|(.., deadline)| *deadline);
            let next = [listing, self.restart.due, replay].into_iter().flatten().min();
            let wait = next.map_or(Duration::MAX, |at| at.saturating_duration_since(Instant::now()));
            let event = match inbox.recv_timeout(wait) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    self.on_time();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if self.handle(event) {
                return;
            }
        }
    }

    /// Handles EVENT; true when the session ends, at `exit` or when the client
    /// closes its output.
    fn handle(&mut self, event: Event) -> bool {
        // The last frames and the end of a process the proxy let go of.
        if let Event::Server(generation, _) | Event::ServerClosed(generation) = &event {
            let starting = self.restart.starting.as_ref().map(|(server, ..)| server);
            if !self.server.iter().chain(starting).any(|server| server.generation == *generation) {
                return false;
            }
        }
        match event {
            Event::Client(frame) => {
                let exit = frame.value.as_ref().and_then(|m| m.get("method")) == Some(&json!("exit"));
                self.on_client(frame);
                return exit;
            }
            Event::ClientClosed => return true,
            Event::Server(_, frame) => self.on_server(frame),
            Event::ServerClosed(_) => self.on_server_closed(),
            Event::Parsed(outcome) => self.on_parsed(outcome),
            Event::Runs(code, found, written) => self.on_runs(code, found, written),
        }
        false
    }

    /// Waits for the server to exit after `exit`, within EXIT_GRACE, then kills it.
    fn stop_server(&mut self) {
        if let Some((mut starting, ..)) = self.restart.starting.take() {
            let _ = starting.child.kill();
            let _ = starting.child.wait();
        }
        let Some(mut server) = self.server.take() else { return };
        drop(server.stdin);
        let deadline = Instant::now() + EXIT_GRACE;
        while Instant::now() < deadline && matches!(server.child.try_wait(), Ok(None)) {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = server.child.kill();
        let _ = server.child.wait();
    }
}

fn spawn_reader(
    input: impl io::Read + Send + 'static,
    events: Sender<Event>,
    wrap: impl Fn(Frame) -> Event + Send + 'static,
    closed: Event,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(input);
        while let Ok(Some(frame)) = rpc::read_frame(&mut reader) {
            if events.send(wrap(frame)).is_err() {
                return;
            }
        }
        let _ = events.send(closed);
    });
}

/// The server COMMAND started as process GENERATION, its output read into
/// EVENTS; else why not.
fn spawn_server(command: &[String], events: &Sender<Event>, generation: u64) -> Result<Server, String> {
    let spawned = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = spawned.map_err(|error| {
        eprintln!("dbt-lsp-plus: cannot start {}: {error}", command.join(" "));
        format!("dbt lsp could not start ({error})")
    })?;
    let stdout = child.stdout.take().expect("piped stdout");
    spawn_reader(
        stdout,
        events.clone(),
        move |frame| Event::Server(generation, frame),
        Event::ServerClosed(generation),
    );
    let stdin = child.stdin.take().expect("piped stdin");
    Ok(Server { child, stdin, generation })
}

/// Whether the dbt of COMMAND, a server command, has no `lsp`, as dbt-oss
/// 2.0.11: the words before its `lsp` word, then `lsp --help`, exit 2 with
/// "unrecognized subcommand 'lsp'" on the standard error.  A launcher's own
/// failure (`uvx dbt` offline exits 2 too), a probe past its deadline, any
/// other answer, or a command with no word before `lsp` is not.
fn lacks_lsp(command: &[String]) -> bool {
    let Some(at) = command.iter().position(|word| word == "lsp").filter(|at| *at > 0) else { return false };
    let mut probe = Command::new(&command[0]);
    probe.args(&command[1..at]).args(["lsp", "--help"]).env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    let Ok(mut child) = probe.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn() else {
        return false;
    };
    let stderr = child.stderr.take().expect("piped stderr");
    let (sender, read) = mpsc::channel();
    // Read apart, so that neither a full pipe nor a launcher's child holding
    // it open holds the proxy.
    thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.take(1 << 16).read_to_end(&mut text);
        let _ = sender.send(text);
    });
    let status = config::finish(child, config::PROBE_DEADLINE, &AtomicBool::new(false));
    let stderr = read.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    status.and_then(|status| status.code()) == Some(2)
        && String::from_utf8_lossy(&stderr).contains("unrecognized subcommand 'lsp'")
}

/// Runs the proxy on the standard input and output, in front of COMMAND,
/// alone when its dbt has no `lsp`.
pub fn run(command: &[String]) -> ExitCode {
    let (events, inbox) = mpsc::channel();
    let standalone = lacks_lsp(command);
    let (server, absent) = match (!standalone).then(|| spawn_server(command, &events, 0)) {
        Some(Ok(server)) => (Some(server), None),
        Some(Err(why)) => (None, Some(why)),
        None => (None, None),
    };
    let cut = config::can_cut();
    if !cut {
        eprintln!(
            "dbt-lsp-plus: unshare -rnm cannot cut dbt parse off the network and the project here: \
             dbt's flags and the guard on packages alone keep it offline and the project as it is"
        );
    }
    let parsing = Parsing { server: command.to_vec(), events: Some(events.clone()), cut, ..Parsing::default() };
    spawn_reader(io::stdin(), events, Event::Client, Event::ClientClosed);
    let mut proxy = Proxy::new(io::BufWriter::new(io::stdout().lock()), server, absent, project_dir(command));
    proxy.standalone = standalone;
    proxy.parsing = parsing;
    proxy.serve(inbox);
    proxy.stop_parse();
    proxy.stop_server();
    if proxy.shutdown { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_change_outside_dbt_s_output_may_change_the_graph() {
        let root = Path::new("/work/a b/shop");
        // As the proxy gives them: a link of logs out of the project, resolved, included.
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
    fn a_restart_waits_one_more_second_each_time_and_three_a_window() {
        let now = Instant::now() + Duration::from_secs(3600);
        let seconds = |past: &[Instant]| restart_delay(past, now).map(|delay| delay.as_secs());
        assert_eq!(seconds(&[]), Some(1));
        assert_eq!(seconds(&[now - Duration::from_secs(5)]), Some(2));
        assert_eq!(seconds(&[now - Duration::from_secs(9), now - Duration::from_secs(5)]), Some(3));
        let three = [now - Duration::from_secs(30), now - Duration::from_secs(20), now - Duration::from_secs(10)];
        assert_eq!(seconds(&three), None);
        // A restart that left the window counts no more.
        let one_old = [now - Duration::from_secs(601), now - Duration::from_secs(20), now - Duration::from_secs(10)];
        assert_eq!(seconds(&one_old), Some(3));
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn only_a_refusal_of_lsp_by_dbt_itself_means_no_lsp() {
        // `sh -c SCRIPT lsp --help` runs SCRIPT, as a dbt answering its probe.
        let dbt = |script: &str| lacks_lsp(&args(&["sh", "-c", script, "lsp", "--project-dir", "/p"]));
        let refused = "echo \"error: unrecognized subcommand 'lsp'\" >&2; exit 2";
        assert!(dbt(refused), "dbt-oss 2.0.11");
        assert!(!dbt("echo 'Start the dbt language server'; exit 0"), "2.0.9 prints its help");
        assert!(!dbt("echo 'error: Failed to fetch the index' >&2; exit 2"), "a launcher offline");
        assert!(!dbt("echo \"error: unrecognized subcommand 'lsp'\" >&2; exit 1"), "another status");
        // The probe takes only the words before `lsp`, and adds `--help`.
        assert!(dbt(
            "[ \"$0 $1\" = 'lsp --help' ] && [ $# -eq 1 ] && { echo \"unrecognized subcommand 'lsp'\" >&2; exit 2; }"
        ));
        // dbt-oss 2.0.11 colours the word under CLICOLOR_FORCE, unless NO_COLOR is set.
        let coloured =
            "[ -n \"$NO_COLOR\" ] || { printf \"unrecognized subcommand '\\033[33mlsp\\033[0m'\" >&2; exit 2; }";
        assert!(dbt(&format!("{coloured}; echo \"error: unrecognized subcommand 'lsp'\" >&2; exit 2")));
        assert!(!lacks_lsp(&args(&["/nonexistent/dbt", "lsp"])), "a dbt that cannot start");
        assert!(!lacks_lsp(&args(&["sh", "-c", "exit 2"])), "a command with no lsp word");
        assert!(!lacks_lsp(&args(&["lsp", "--project-dir", "/p"])), "no word before lsp");
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

    #[test]
    fn a_listing_past_its_deadline_is_abandoned_and_the_graph_kept() {
        let mut proxy = Proxy::new(Vec::new(), None, None, None);
        proxy.nodes = Some(Vec::new());
        proxy.listing = Some(("dbt-lsp-plus/1".into(), 1, Instant::now() - LIST_TIMEOUT));
        proxy.listed = vec![ServerNode {
            id: "model.shop.a".into(),
            name: "a".into(),
            resource_type: "model".into(),
            package: "shop".into(),
            original_file_path: "models/a.sql".into(),
            depends_on: Vec::new(),
        }];
        proxy.expire_listing();
        assert!(proxy.listing.is_none() && proxy.listed.is_empty());
        assert_eq!(proxy.nodes, Some(Vec::new()));
        // A listing within its deadline stays.
        proxy.listing = Some(("dbt-lsp-plus/2".into(), 0, Instant::now()));
        proxy.expire_listing();
        assert!(proxy.listing.is_some());
    }
}
