// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy: forwards every message between the editor and `dbt lsp`
//! byte for byte, and answers itself the requests of the features that the
//! server does not announce.

use crate::commands::{self, Via};
use crate::graph::{self, Graph, ServerNode};
use crate::locate::{self, Probe};
use crate::modes::{self, Mode};
use crate::project::{self, Project};
use crate::rpc::{self, Frame};
use crate::text::{self, Encoding, LineIndex};
use crate::{
    cache, config, document, features, navigation, packages, parse_log, properties, runs, scan, shadow, sources,
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufReader, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
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

/// The capabilities of the requests that the proxy answers in `plus`
/// whatever the server announces, and whether it then answers them as a
/// whole, as a hover, rather than by target.
const PROVIDERS: [(&str, &str, bool); 5] = [
    ("hoverProvider", "textDocument/hover", true),
    ("inlayHintProvider", "textDocument/inlayHint", true),
    ("definitionProvider", "textDocument/definition", false),
    ("referencesProvider", "textDocument/references", false),
    ("renameProvider", "textDocument/rename", false),
];

/// How the proxy answers a request of the client's.
enum Route {
    /// Itself, as this method.
    Here(&'static str),
    /// Itself, with this result: nothing, null or `[]` for a list, or the
    /// shadow's counts.
    Answer(Value),
    /// By the server.
    Forward,
    /// As this command of the proxy's for clients.
    Command(&'static str),
}

/// The requests that the proxy answers itself on some targets, since dbt
/// lsp 2.0.9 answers them with nothing there: references and renames of a
/// source, a rename of a CTE, a definition that `navigation` finds;
/// elsewhere they go to the server.
const TARGETED: [&str; 3] = ["textDocument/references", "textDocument/rename", "textDocument/definition"];

/// What the proxy answers itself, as `experimental.dbtLspPlus` announces
/// it, version 2: per request, each kind of target with the places where a
/// request's position may be, `sql`, `yaml` or `python` by the file's
/// extension.  A kind listed at a place is the proxy's at every position of
/// that kind there, never forwarded, with dbt lsp or without.
const ANSWERED: [(&str, &str, &[&str]); 13] = [
    ("definition", "source", &["sql", "yaml", "python"]),
    ("definition", "ref", &["sql", "yaml", "python"]),
    ("definition", "macro", &["sql", "yaml"]),
    ("definition", "doc", &["yaml"]),
    ("definition", "entry", &["yaml"]),
    ("definition", "test", &["yaml"]),
    ("definition", "function", &["sql", "yaml"]),
    ("references", "source", &["sql", "yaml", "python"]),
    ("references", "ref", &["sql", "yaml", "python"]),
    ("references", "macro", &["sql", "yaml"]),
    ("references", "function", &["sql", "yaml"]),
    ("rename", "source", &["sql", "yaml", "python"]),
    ("rename", "cte", &["sql"]),
];

/// The requests answered by the proxy that ask for the parse's index, in
/// both modes: nodeInfo, and a definition or references for versions and
/// disabled nodes.  A stale index is parsed again, as for a hover.
const WANT_INDEX: [&str; 3] = [config::NODE_INFO, "textDocument/definition", "textDocument/references"];

/// The selectors of the graph, one request each: `dbt.listNodes` reads only
/// its first argument, `*` leaves sources out, and a second request in
/// flight cancels the first.
const SELECTORS: [&str; 2] = ["*", "source:*"];
/// How often a listing cancelled by another `dbt.listNodes` is tried again.
const MAX_ATTEMPTS: u32 = 3;
/// A listing unanswered for this long is abandoned.
const LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// A request to which the proxy adds a part is answered with it alone if the
/// server has not answered within this long: three times what dbt lsp 2.0.9
/// took on the fixture during a compilation.
const MERGE_DEADLINE: Duration = Duration::from_millis(300);
/// A merged request to which the proxy has nothing to add is answered with
/// that nothing if the server has not answered within this long: above the
/// slowest answer of dbt lsp 2.0.9 seen, 1.9 s for the first hover on 2,000
/// models during its compilation.
const NOTHING_DEADLINE: Duration = Duration::from_secs(2);
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
    /// The answer of a run of dbt's CLI to the client's request of that id.
    Ran(Value, Value),
}

/// The project's files that a thread reads before the first request needs
/// them, as `cache::fill` gives them.
type Filled = Vec<(PathBuf, cache::Key, Arc<project::Scanned>)>;

/// What a watched file and the manifest give: its failures, or none to keep
/// those known.
type ReadFailures = fn(&Value, &Value) -> Option<Vec<runs::Failure>>;

/// How often the files of the last run are looked at.
const RUNS_EVERY: Duration = Duration::from_secs(1);

/// What the proxy says on its standard error where nothing can cut its
/// runs of dbt off.
const NO_CUT: &str = "dbt-lsp-plus: unshare -rnm cannot cut dbt parse off the network and the project here: \
                      dbt's flags and the guard on packages alone keep it offline and the project as it is";

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
    /// The errors and warnings of the last parse that ran to its end, alone.
    parsed: HashMap<PathBuf, Vec<parse_log::Problem>>,
    /// The messages of the problems in no file that were logged.
    logged: HashSet<String>,
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
    /// A merged request to which the proxy adds nothing: this nothing, null
    /// or `[]`, is its answer when the server's is an error, late, or none.
    Nothing(Value),
    /// Answered with the proxy's part alone once the server was late: its
    /// answer, when it comes, is dropped.
    Answered,
    /// A definition or references that the shadow follows: its method and
    /// parameters, and the count of the client's changes when it came.
    Shadow {
        method: String,
        params: Value,
        changes: u64,
    },
}

impl Pending {
    /// The server's answer, MESSAGE, as this kind of request makes it, for a
    /// client that reads markdown when MARKDOWN; none to pass it unchanged.
    /// An error to a request with a part of the proxy's gives that part.
    fn merged(&self, message: &Value, markdown: bool) -> Option<Value> {
        if message.get("error").is_some() {
            return self.alone(markdown).map(|part| json!({"jsonrpc": "2.0", "id": message["id"], "result": part}));
        }
        match self {
            Pending::Hover(text, hard) => features::merge_hover(message, text, *hard, markdown),
            Pending::Hints(hints) => features::merge_hints(message, hints),
            _ => None,
        }
    }

    /// The proxy's part alone, as the answer when the server gives none: a
    /// hover's text, the hints, a merged request's nothing; none for a
    /// request that is not merged.
    fn alone(&self, markdown: bool) -> Option<Value> {
        let nothing = match self {
            Pending::Hover(..) => json!({"result": null}),
            Pending::Hints(_) => json!({"result": []}),
            Pending::Nothing(nothing) => return Some(nothing.clone()),
            _ => return None,
        };
        self.merged(&nothing, markdown).map(|merged| merged["result"].clone())
    }

    /// How long the server has to answer this kind of request before the
    /// client gets the proxy's answer alone: a part is worth showing at
    /// once, a nothing only once the server is clearly late.
    fn deadline(&self) -> Option<Duration> {
        match self {
            Pending::Hover(..) | Pending::Hints(_) => Some(MERGE_DEADLINE),
            Pending::Nothing(_) => Some(NOTHING_DEADLINE),
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
    /// Where dbt's processes run: the project, when the client's folder or
    /// a document named it; none, the proxy's working directory, when the
    /// server command did.
    dir: Option<PathBuf>,
    cache: Option<PathBuf>,
    /// How many times the index changed: the graph alone holds while this stays.
    generation: u64,
}

/// The generations of the project, the server's nodes and the index a
/// graph was made of.
type MadeOf = (u64, u64, u64);

struct Proxy<W: Write> {
    client: W,
    server: Option<Server>,
    /// Whether the user's dbt has no `lsp`, as dbt-oss: no server runs, and
    /// the proxy answers alone, from its scan and the user's `dbt parse`.
    standalone: bool,
    /// Why there is no server, told to the client once it is initialized.
    absent: Option<String>,
    /// The line that says which project and which dbt the proxy found,
    /// told to the client once it can be.
    line: Option<String>,
    /// The project directory: the server command's `--project-dir`, spelt
    /// as the client spells it when the client names the same directory;
    /// else the client's folder, when it holds a project; else the project
    /// of the first document opened in one; none before.
    root: Option<PathBuf>,
    /// The root with its links resolved, onto which the client's paths map.
    canonical_root: Option<PathBuf>,
    encoding: Encoding,
    hierarchical: bool,
    /// The methods that the proxy answers, fixed by the modes and the server's capabilities.
    owned: HashSet<&'static str>,
    /// The user's settings, from the command line, then `initializationOptions`.
    settings: modes::Settings,
    /// The mode of each method, fixed when `initialize` is answered.
    table: modes::Table,
    /// Whether the backend is `never`, which the proxy says once alone.
    never: bool,
    /// The user's settings that `initializationOptions` held and the proxy
    /// refused, said once `initialize` is answered.
    refused: Vec<String>,
    /// Whether the proxy said that it reads no `dbtLspPlus` setting of
    /// `workspace/didChangeConfiguration`.
    ignored: bool,
    /// Whether the client sent `initialize`, before which LSP lets the proxy send nothing.
    initialized: bool,
    /// The id of the client's `initialize` while the server has not answered it.
    initialize: Option<Value>,
    /// The client's requests that the server has yet to answer.
    forwarded: HashMap<String, (Value, Pending)>,
    /// When each merged request is answered with the proxy's part or
    /// nothing alone, and its method.
    deadlines: HashMap<String, (Instant, String)>,
    /// The methods whose late answer the proxy logged.
    late: HashSet<String>,
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
    /// How many times the server's nodes changed: the graph holds while this stays.
    nodes_generation: u64,
    shutdown: bool,
    /// Whether the client reads markdown in a hover.
    markdown: bool,
    /// Whether the client may be asked to refresh its inlay hints.
    hint_refresh: bool,
    /// Whether the client lets a server register a watcher of files.
    watches: bool,
    /// The proxy's own requests to the client, whose answers stay here.
    asked: HashSet<String>,
    diagnostics: Diagnostics,
    restart: Restart,
    parsing: Parsing,
    /// The scan of the project, kept between requests, and refreshed once
    /// an epoch: one a request or event.
    cache: RefCell<cache::Cache>,
    /// The first fill of the cache, under way on a thread, taken once it is
    /// done: nothing waits for it, and a request that comes before reads
    /// the files itself.
    filling: RefCell<Option<Receiver<Filled>>>,
    epoch: u64,
    /// The count of each open document's edits, under which the cache keeps
    /// it, and the count so far.
    edits: HashMap<PathBuf, u64>,
    edited: u64,
    /// The count of the client's changes and closes of a document, after
    /// which the shadow compares no answer to an earlier request.
    changes: u64,
    /// The graph, with the generations it was made of.
    graph: RefCell<Option<(MadeOf, Rc<Graph>)>>,
    /// The shadow, on from the server's answer to `initialize` when the user
    /// set it.
    shadow: Option<shadow::Session>,
    /// The commands that the server listed in its answer to `initialize`.
    server_commands: HashSet<String>,
    /// The runs of dbt's CLI that clients asked for, one at a time: those
    /// waiting, with the client's id, their command line and their
    /// directory, and the one under way.
    runs: VecDeque<(Value, commands::Job, Vec<String>, PathBuf)>,
    run: Option<config::Running>,
}

/// PATH with each `..` dropping the component before it, as written: no
/// link is followed.
fn lexical(path: &Path) -> PathBuf {
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

/// The place of the file at PATH in `experimental.dbtLspPlus`, by its
/// extension: `sql`, `yaml` or `python`.
fn place(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("sql") => Some("sql"),
        Some("yml" | "yaml") => Some("yaml"),
        Some("py") => Some("python"),
        _ => None,
    }
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
    let spelt = lexical(&std::path::absolute(value).ok()?);
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
            line: None,
            root: None,
            canonical_root: None,
            encoding: Encoding::Utf16,
            hierarchical: false,
            owned: HashSet::new(),
            settings: modes::Settings::default(),
            table: modes::Table::default(),
            never: false,
            refused: Vec::new(),
            ignored: false,
            initialized: false,
            initialize: None,
            forwarded: HashMap::new(),
            deadlines: HashMap::new(),
            late: HashSet::new(),
            documents: HashMap::new(),
            uris: HashMap::new(),
            opened: false,
            next_id: 0,
            listing: None,
            listed: Vec::new(),
            attempts: 0,
            relist: false,
            nodes: None,
            nodes_generation: 0,
            shutdown: false,
            markdown: false,
            hint_refresh: false,
            watches: false,
            asked: HashSet::new(),
            diagnostics: Diagnostics::default(),
            restart: Restart::default(),
            parsing: Parsing::default(),
            cache: RefCell::default(),
            filling: RefCell::default(),
            epoch: 0,
            edits: HashMap::new(),
            edited: 0,
            changes: 0,
            graph: RefCell::default(),
            shadow: None,
            server_commands: HashSet::new(),
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
        let lacks = |capabilities: &serde_json::Map<String, Value>, capability: &str| {
            capabilities.get(capability).is_none_or(|value| value.is_null() || *value == json!(false))
        };
        let announced = |method: &str| {
            ADDED.iter().any(|(capability, methods)| methods.contains(&method) && !lacks(capabilities, capability))
        };
        self.table = modes::Table::new(&self.settings, &announced, self.standalone);
        // A method the proxy answers is announced; one left to the server, as the server announced it,
        // and with no backend, not at all.
        for (capability, methods) in ADDED.iter().filter(|(_, methods)| self.table.mode(methods[0]) != Mode::Dbt) {
            if lacks(capabilities, capability) {
                capabilities.insert((*capability).to_owned(), json!(true));
            }
            self.owned.extend(*methods);
        }
        let unanswered =
            PROVIDERS.iter().filter(|(_, method, _)| self.standalone && self.table.mode(method) == Mode::Dbt);
        for (capability, ..) in unanswered {
            capabilities.remove(*capability);
        }
        for (capability, method, owned) in
            PROVIDERS.iter().filter(|(_, method, _)| self.table.mode(method) == Mode::Plus)
        {
            if lacks(capabilities, capability) {
                capabilities.insert((*capability).to_owned(), json!(true));
            }
            self.owned.extend(owned.then_some(*method));
        }
        // Which requests the proxy answers itself, project-wide, on which kinds
        // of target, beside the server's own experimental capabilities: a
        // client such as dbt-mode steps aside for exactly those.
        let experimental = capabilities.entry("experimental").or_insert(Value::Null);
        if experimental.is_null() {
            *experimental = json!({});
        }
        if let Some(experimental) = experimental.as_object_mut() {
            experimental.insert("dbtLspPlus".into(), self.table.flag(&ANSWERED));
        }
        // The proxy's own commands, beside the server's, which it may forward its commands for clients to.
        let provider = capabilities.entry("executeCommandProvider").or_insert_with(|| json!({}));
        let listed = provider.get("commands").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
        self.server_commands = listed.map(str::to_owned).collect();
        let clients = commands::announced(&|dbt| self.server_commands.contains(dbt), self.runs_cli());
        let own = [Some(config::NODE_INFO), self.shadow.as_ref().map(|_| shadow::SUMMARY)];
        if let Some(commands) = provider.as_object_mut().map(|provider| provider.entry("commands").or_insert(json!([])))
        {
            let own: Vec<Value> = own.into_iter().flatten().chain(clients).map(|command| json!(command)).collect();
            commands.as_array_mut().into_iter().for_each(|commands| commands.extend(own.iter().cloned()));
        }
    }

    /// Says once, after `initialize` is answered, what of the user's settings
    /// cannot take effect: each refusal, the methods set to `dbt` that no
    /// backend answers, and a shadow with no dbt lsp to compare with; and a
    /// mode named by its former name.
    fn tell_settings(&mut self) {
        let mut said = std::mem::take(&mut self.refused);
        if !said.is_empty() {
            said.push(format!("the methods that take a mode are {}", modes::METHODS.join(", ")));
        }
        said.extend(self.settings.ours.then(|| "the mode ours is now named plus, and read so".to_owned()));
        let unanswered = self.table.unanswered();
        if !unanswered.is_empty() {
            said.push(format!("no dbt lsp answers {}, set to dbt: they get nothing", unanswered.join(", ")));
        }
        let off = self.settings.shadow && self.shadow.is_none();
        said.extend(off.then(|| "the shadow compares with dbt lsp, which does not run: it is off".to_owned()));
        if !said.is_empty() {
            let text = format!("dbt-lsp-plus: {}.", said.join("; "));
            self.send_client(&json!({"jsonrpc": "2.0", "method": "window/showMessage",
                                     "params": {"type": 2, "message": text}}));
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
        self.tell_settings();
        // Without a project yet, nothing is said: there may never be one.
        if self.root.is_none() {
            return;
        }
        // A dbt without `lsp` is no failure: said once, out of the user's way,
        // by the line of the dbt found, when one was looked for.
        if self.standalone {
            let dbt = self.parsing.server.iter().take_while(|word| *word != "lsp").cloned().collect::<Vec<_>>();
            let text = match self.never {
                true => "dbt-lsp-plus: the backend is never: dbt-lsp-plus answers alone.".to_owned(),
                false => format!("dbt-lsp-plus: {} has no lsp: dbt-lsp-plus answers alone.", dbt.join(" ")),
            };
            self.line.get_or_insert(text);
            return self.say_found();
        }
        let why = self.absent.take().unwrap_or_else(|| "dbt lsp is not running".into());
        let text = format!("{why}: dbt-lsp-plus serves only its own features: symbols, lineage, folding, renames.");
        self.send_client(
            &json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": text}}),
        );
        self.say_found();
    }

    /// Tells the client, once, which project and which dbt the proxy found,
    /// as one log line.
    fn say_found(&mut self) {
        if let Some(text) = self.line.take() {
            let params = json!({"type": 3, "message": text});
            self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": params}));
        }
    }

    /// Takes ROOT as the project, found from the client's folder, or from a
    /// document when DOCUMENT, on which the proxy stays alone: dbt's
    /// processes run there, the server command's relative paths are taken
    /// from the proxy's directory, its `dbt` becomes the project's, the cut
    /// is probed, the server started unless alone, and
    /// the line that says so is written on the standard error and kept for
    /// the client.
    fn take_project(&mut self, root: PathBuf, document: bool) {
        let git = locate::Git { program: "git".into(), env: std::env::vars_os().collect() };
        // What the user wrote from the proxy's directory still names the same from the project.
        let server = std::env::current_dir().map(|cwd| config::anchored(&self.parsing.server, &cwd));
        let found = locate::dbt(&root, &server.unwrap_or_else(|_| self.parsing.server.clone()), &git);
        let no_lsp =
            || found.lsp.map_or_else(|| locate::probe(&found.command, Some(&root)) == Probe::NoLsp, |lsp| !lsp);
        // Neither with `never` nor on a document is the dbt probed for a server.
        let alone = match (self.never, document) {
            (true, _) => Some("the backend is never"),
            (_, true) => Some("dbt lsp is not started for a project found from a file"),
            _ => no_lsp().then_some("it has no lsp"),
        };
        self.parsing.cut = config::can_cut();
        if !self.parsing.cut {
            eprintln!("{NO_CUT}");
        }
        let skipped: String =
            found.skipped.iter().map(|(path, why)| format!("; skipped {} ({why})", path.display())).collect();
        let tail = alone.map_or_else(|| ".".to_owned(), |why| format!("; {why}: dbt-lsp-plus answers alone."));
        let (root_shown, named, source) = (root.display(), &found.named, found.source);
        let line = format!("dbt-lsp-plus: project {root_shown}, dbt {named} ({source}){skipped}{tail}");
        eprintln!("{line}");
        self.line = Some(line);
        (self.parsing.server, self.parsing.dir, self.standalone) = (found.command, Some(root.clone()), alone.is_some());
        if let (None, Some(events)) = (alone, self.parsing.events.clone()) {
            match spawn_server(&self.parsing.server, &events, 0, Some(&root)) {
                Ok(server) => self.server = Some(server),
                Err(why) => self.absent = Some(why),
            }
        }
        self.set_root(root);
    }

    /// The first scan of the project, read on a thread meanwhile, so that
    /// the first request needs none.
    fn scan_first(&mut self) {
        if let Some(root) = self.root.clone() {
            let (filled, filling) = mpsc::channel();
            thread::spawn(move || drop(filled.send(cache::fill(&root))));
            self.filling.replace(Some(filling));
        }
    }

    /// Learns from the client's `initialize`: the outline's shape, and the
    /// root when the server command named none, or the client's spelling of
    /// the one it named.  A project in the client's folder starts what the
    /// server command's would have at the proxy's start; without one, the
    /// proxy is alone for good, and quiet until a document names a project.
    fn on_initialize(&mut self, params: &Value) {
        let options = params.get("initializationOptions").unwrap_or(&Value::Null);
        (self.settings, self.refused) = self.settings.over(options);
        // As the server got it before modes, at a restart too.
        self.restart.initialize = Some(modes::without_section(params));
        self.hierarchical = params
            .pointer("/capabilities/textDocument/documentSymbol/hierarchicalDocumentSymbolSupport")
            == Some(&json!(true));
        let formats = params.pointer("/capabilities/textDocument/hover/contentFormat").and_then(Value::as_array);
        // The client lists its formats in its order of preference.
        let first =
            formats.and_then(|formats| formats.iter().find(|format| *format == "markdown" || *format == "plaintext"));
        self.markdown = first.is_some_and(|format| format == "markdown");
        self.hint_refresh = params.pointer("/capabilities/workspace/inlayHint/refreshSupport") == Some(&json!(true));
        let watched = "/capabilities/workspace/didChangeWatchedFiles/dynamicRegistration";
        self.watches = params.pointer(watched) == Some(&json!(true));
        let folder = params.pointer("/workspaceFolders/0/uri").or_else(|| params.get("rootUri"));
        let folder = uri_path(folder.and_then(Value::as_str)).filter(|folder| folder.join("dbt_project.yml").is_file());
        let same = |folder: &Path| fs::canonicalize(folder).ok() == self.canonical_root;
        match (folder, &self.root) {
            (Some(folder), None) => self.take_project(folder, false),
            (Some(folder), Some(_)) if same(&folder) => self.set_root(folder),
            (None, None) => self.standalone = true,
            _ => {}
        }
        self.scan_first();
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
            // So are the parse's problems, alone.
            let parsed = self.diagnostics.parsed.get(&path).is_some_and(|problems| !problems.is_empty());
            let ranged = server.is_some_and(|(_, diagnostics)| !diagnostics.is_empty()) || parsed;
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
                self.restart.languages.insert(path.clone(), params["textDocument"]["languageId"].clone());
                self.restart.versions.insert(path.clone(), params["textDocument"]["version"].clone());
                self.uris.insert(path, uri.unwrap_or_default().to_owned());
            }
            "textDocument/didChange" => {
                self.restart.versions.insert(path.clone(), params["textDocument"]["version"].clone());
                self.changes += 1;
                self.edited += 1;
                self.edits.insert(path.clone(), self.edited);
                let changes = params.get("contentChanges").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
                if let Some(text) = self.documents.get_mut(&path) {
                    changes.iter().for_each(|change| text::apply_change(text, change, self.encoding));
                }
            }
            "textDocument/didClose" => {
                // An unsaved text gives way to the file's.
                self.changes += 1;
                self.restart.languages.remove(&path);
                self.restart.versions.remove(&path);
                self.documents.remove(&path);
                self.edits.remove(&path);
                self.uris.remove(&path);
            }
            _ => {}
        }
    }

    /// The project as its files and the open documents say, if there is
    /// one: the cache's, looked at once an epoch.
    fn project(&self) -> Option<Rc<Project>> {
        let open = cache::Open { texts: &self.documents, edits: &self.edits };
        let mut cache = self.cache.borrow_mut();
        let filling = self.filling.take();
        match filling.as_ref().map(Receiver::try_recv) {
            Some(Ok(filled)) => cache.adopt(filled),
            Some(Err(TryRecvError::Empty)) => drop(self.filling.replace(filling)),
            Some(Err(TryRecvError::Disconnected)) | None => {}
        }
        cache.project(self.root.as_deref()?, self.epoch, &open)
    }

    /// The graph of PROJECT with the server's nodes once listed, or, alone,
    /// the last parse's: made again only when the project, the nodes or the
    /// index changed, as the call hierarchy asks for it at each step.
    fn graph(&self, project: &Project) -> Rc<Graph> {
        let made_of = (self.cache.borrow().generation, self.nodes_generation, self.parsing.generation);
        if let Some((_, graph)) = self.graph.borrow().as_ref().filter(|(was, _)| *was == made_of) {
            return graph.clone();
        }
        let scanned = Graph::scanned(project);
        let parsed = self.parsing.index.as_ref().filter(|_| self.standalone).map(config::Index::listed);
        let graph = Rc::new(match parsed.as_ref().or(self.nodes.as_ref()) {
            Some(nodes) => scanned.with_server(nodes, project),
            None => scanned,
        });
        *self.graph.borrow_mut() = Some((made_of, graph.clone()));
        graph
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
        let graph = self.graph(&project);
        if method != "textDocument/prepareCallHierarchy" {
            let id = params.pointer("/item/data/uniqueId")?.as_str()?;
            let incoming = method == "callHierarchy/incomingCalls";
            return Some(features::calls(&project, &graph, id, incoming, self.encoding));
        }
        // A document that is no file, such as an unsaved buffer, holds no resource.
        let Some(path) = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str)) else {
            return Some(Value::Null);
        };
        let text = project.text(&path).unwrap_or_default();
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
    fn source_at(&self, params: &Value) -> Option<(Rc<Project>, sources::Target)> {
        let (path, text, offset) = self.document_at(params)?;
        let target = sources::target_at(&path, &text, offset)?;
        let project = self.project().filter(|project| project.files.contains_key(&path))?;
        Some((project, target))
    }

    /// How the proxy answers the client's request METHOD: itself, as an owned
    /// method, its own commands, or a targeted request on one of its targets
    /// in its mode; with nothing, where the mode says so, and without a
    /// server for a definition and `shutdown`; else by forwarding it.
    fn route(&self, method: &str, params: &Value) -> Route {
        // Before a project, null to all but `initialize`: a valid result of
        // every method the proxy announces, and of each an agent was seen to
        // send.
        if self.root.is_none() && method != "initialize" {
            return Route::Answer(Value::Null);
        }
        if let Some(owned) = self.owned.get(method) {
            return Route::Here(owned);
        }
        let command = params["command"].as_str().filter(|_| method == "workspace/executeCommand");
        if command == Some(config::NODE_INFO) {
            return Route::Here(config::NODE_INFO);
        }
        if let (Some(shadow::SUMMARY), Some(shadow)) = (command, &self.shadow) {
            return Route::Answer(shadow.counts.summary());
        }
        if let Some(own) = command.and_then(commands::own) {
            return Route::Command(own);
        }
        // In `dbt`, the proxy looks at nothing.
        let mine = self.table.mode(method) != Mode::Dbt;
        if let Some(targeted) = mine.then(|| self.targeted(method, params)).flatten() {
            return Route::Here(targeted);
        }
        // Without a server, a definition with no target is null, not a failure, and `shutdown` is null as LSP asks.
        let lifecycle = ["textDocument/definition", "shutdown"].contains(&method);
        let unserved = (lifecycle && self.server.is_none()).then_some(Value::Null);
        self.table.nothing(method).or(unserved).map_or(Route::Forward, Route::Answer)
    }

    /// A request of `TARGETED` that the proxy answers itself, by what its
    /// position holds: references and rename on a source, a rename on a CTE
    /// of a SQL file, or a definition on a target of `navigation`.
    fn targeted(&self, method: &str, params: &Value) -> Option<&'static str> {
        let method = TARGETED.into_iter().find(|targeted| *targeted == method)?;
        let (path, text, offset) = self.document_at(params)?;
        let sql = path.extension().is_some_and(|extension| extension == "sql");
        let answered = match method {
            "textDocument/definition" => navigation::target_at(&path, &text, offset)
                .is_some_and(|target| self.claims("definition", &target, &path)),
            // A CTE lives in its file; a source only the scan sees whole, and so its references.
            "textDocument/rename" if sql && document::cte_at(&text, offset).is_some() => true,
            // References come from the scan, whichever file of the project asks, as agreed with dbt-mode.
            "textDocument/references" => {
                let claimed = navigation::target_at(&path, &text, offset)
                    .is_some_and(|target| self.claims("references", &target, &path));
                claimed && self.root.as_ref().is_some_and(|root| path.starts_with(root))
            }
            _ => self.source_at(params).is_some(),
        };
        answered.then_some(method)
    }

    /// Whether the proxy answers REQUEST itself on TARGET in the file at
    /// PATH, as `ANSWERED` says.
    fn claims(&self, request: &str, target: &navigation::Target, path: &Path) -> bool {
        let Some(place) = place(path) else { return false };
        let listed = |(answered, kind, places): &&(&str, &str, &[&str])| {
            let listed = self.table.lists(&format!("textDocument/{answered}"));
            *answered == request && *kind == target.kind() && places.contains(&place) && listed
        };
        ANSWERED.iter().any(|row| listed(&row))
    }

    /// The definition of the target at the request's position, by dbt
    /// 2.0.9's lookup: its location, several, or null.
    fn on_definition(&self, params: &Value) -> Option<Result<Value, String>> {
        let (path, text, offset) = self.document_at(params)?;
        let target = navigation::target_at(&path, &text, offset);
        let (Some(target), Some(project)) = (target, self.project()) else { return Some(Ok(Value::Null)) };
        let lookup = navigation::Lookup::new(&project, self.parsing.index.as_ref());
        let locations = self.locations(&project, &navigation::resolve(&lookup, &path, &target));
        Some(Ok(match &locations[..] {
            [] => Value::Null,
            [location] => location.clone(),
            _ => Value::Array(locations),
        }))
    }

    /// The references of the source, the node or the macro at the request's
    /// position, with their declaration when the client asks.
    fn on_references(&self, params: &Value) -> Option<Result<Value, String>> {
        let (path, text, offset) = self.document_at(params)?;
        let target = navigation::target_at(&path, &text, offset);
        let (Some(target), Some(project)) = (target, self.project()) else { return Some(Ok(json!([]))) };
        let declaration = params.pointer("/context/includeDeclaration").and_then(Value::as_bool) == Some(true);
        if let navigation::Target::Source(source) = &target {
            let uri_of = |path: &Path| self.uri_of(path);
            return Some(Ok(sources::references(&project, source, declaration, self.encoding, &uri_of)));
        }
        let lookup = navigation::Lookup::new(&project, self.parsing.index.as_ref());
        let found = navigation::references(&lookup, &path, &target, declaration);
        Some(Ok(Value::Array(self.locations(&project, &found))))
    }

    /// The URI of the file at PATH, as the client spelt it if it opened it.
    fn uri_of(&self, path: &Path) -> String {
        self.uris.get(path).cloned().unwrap_or_else(|| features::uri(path))
    }

    /// The locations of FOUND in PROJECT, as the client spelt their URIs.
    fn locations(&self, project: &Project, found: &[navigation::Found]) -> Vec<Value> {
        let location = |found: &navigation::Found| {
            // Up to the range's end only: a macro's references may span thousands of files.
            let text = project.text(&found.path).unwrap_or_default();
            let index = LineIndex::new(text.get(..found.range.end).unwrap_or(text));
            let range = index.range(found.range.start, found.range.end, self.encoding);
            json!({"uri": self.uri_of(&found.path), "range": range})
        };
        found.iter().map(location).collect()
    }

    /// The rename of the source at the request's position, in a file the scan
    /// holds: a rename that left the asking file as it is would be wrong.
    fn on_source(&self, params: &Value) -> Option<Result<Value, String>> {
        let Some((project, target)) = self.source_at(params) else { return Some(Ok(Value::Null)) };
        let uri_of = |path: &Path| self.uri_of(path);
        let new = params.get("newName")?.as_str()?;
        Some(sources::rename(&project, &target, new, self.encoding, &uri_of))
    }

    /// The rename of the source, else of the CTE, at the request's position.
    fn on_rename(&self, params: &Value) -> Option<Result<Value, String>> {
        if self.source_at(params).is_some() {
            return self.on_source(params);
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
            let found =
                navigation::resolve(&navigation::Lookup::new(&project, self.parsing.index.as_ref()), &path, &target);
            return found.first().and_then(|found| index.node(&found.id));
        }
        let relative = path.strip_prefix(self.root.as_ref()?).ok().filter(|_| file)?;
        index.of_file(&relative.to_string_lossy())
    }

    /// The information of a node, named by its id or by a position: the
    /// manifest's node with its relation and the summary a hover shows, null
    /// for no node; a failure while no parse has finished.
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
        let summarised = |node: &Value| {
            let mut answer = node.clone();
            answer["summary"] = json!(index.summary(node));
            answer
        };
        Some(Ok(node.map_or(Value::Null, summarised)))
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
        if let Some(why) = packages::refused(&root, &self.parsing.server, self.parsing.cut, self.standalone) {
            return self.log_parse(&why);
        }
        let cache = self.cache(&root);
        let parse = config::PARSE.map(str::to_owned);
        let Some(command) = config::dbt_command(&self.parsing.server, &root, &cache, self.parsing.cut, &parse) else {
            return self.log_parse("the server command has no lsp, so dbt-lsp-plus runs no dbt parse");
        };
        let Some(events) = self.parsing.events.clone() else { return };
        self.parsing.fresh = true;
        let report = Box::new(move |outcome| drop(events.send(Event::Parsed(outcome))));
        let dir = self.parsing.dir.clone();
        self.parsing.running = Some(config::start(command, dir, cache, config::PARSE_DEADLINE, report));
    }

    /// This process's cache of ROOT, under the user's, made at its first
    /// use, the caches beside it that no parse wrote for a day removed then.
    fn cache(&mut self, root: &Path) -> PathBuf {
        let server = &self.parsing.server;
        let cache = self.parsing.cache.get_or_insert_with(|| {
            let cache = config::cache_dir(&config::cache_base().unwrap_or_else(std::env::temp_dir), root, server);
            config::remove_stale(&cache);
            cache
        });
        cache.clone()
    }

    /// A request of one of the proxy's commands for clients, COMMAND with
    /// PARAMS, kept as PENDING: sent to dbt lsp as its own command, queued
    /// for dbt's CLI, or refused, as `commands::route` decides.
    fn on_command(&mut self, command: &'static str, id: &Value, params: &Value, pending: Pending) {
        if self.shutdown {
            return self.reply_error(id, INVALID_REQUEST, "the client asked for shutdown");
        }
        let Some(arguments) = commands::arguments(command, params) else {
            return self.reply_error(id, INVALID_PARAMS, "invalid parameters");
        };
        let serves = |dbt: &str| self.server.is_some() && self.server_commands.contains(dbt);
        match commands::route(command, &serves, self.runs_cli()) {
            Via::Server(dbt) => {
                let params = json!({"command": dbt, "arguments": arguments});
                let request =
                    json!({"jsonrpc": "2.0", "id": id, "method": "workspace/executeCommand", "params": params});
                self.forwarded.insert(key(id), (id.clone(), pending));
                self.send_server(&serde_json::to_vec(&request).unwrap_or_default());
            }
            Via::Run => self.run_command(command, id, &arguments[0]),
            Via::Refused(failure) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": failure})),
        }
    }

    /// Whether dbt's CLI can do a command for clients: under the cut, by a
    /// server command with `lsp`, from which the run's is made.
    fn runs_cli(&self) -> bool {
        self.parsing.cut && self.parsing.server.iter().any(|word| word == "lsp")
    }

    /// Queues the run of COMMAND by dbt's CLI, under the cut, for the
    /// client's request ID with ARGUMENT; or answers why it cannot run: a
    /// file it cannot compile, packages that need the network, a server
    /// command with no `lsp`.
    fn run_command(&mut self, command: &'static str, id: &Value, argument: &Value) {
        let root = self.root.clone().unwrap_or_default();
        let job = match command {
            commands::COMPILE => {
                let cwd = self.dbt_dir().unwrap_or_default();
                commands::compile_job(
                    self.local_path(argument.as_str()).as_deref(),
                    &root,
                    self.project().as_deref(),
                    cwd,
                )
            }
            _ => Ok(commands::nodes_job(argument.as_str().unwrap_or_default())),
        };
        let refused = packages::refused(&root, &self.parsing.server, true, self.standalone);
        let dir = self.cache(&root).join("run");
        let line =
            job.as_ref().ok().and_then(|job| config::dbt_command(&self.parsing.server, &root, &dir, true, &job.words));
        let failure = match (job, refused, line) {
            (Ok(job), None, Some(line)) => {
                self.runs.push_back((id.clone(), job, line, dir));
                return self.next_run();
            }
            (Err(failure), ..) => failure,
            (_, Some(why), _) => commands::failure(command, &why),
            (_, _, None) => commands::failure(command, "the server command has no lsp, so dbt-lsp-plus runs no dbt"),
        };
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": failure}));
    }

    /// Starts the next run waiting, unless one is under way.
    fn next_run(&mut self) {
        if self.run.is_some() {
            return;
        }
        let (Some(events), Some((id, job, line, dir))) = (self.parsing.events.clone(), self.runs.pop_front()) else {
            return;
        };
        let report = Box::new(move |result| drop(events.send(Event::Ran(id, result))));
        let cwd = self.parsing.dir.clone();
        self.run = Some(commands::start(job, line, cwd, dir, config::PARSE_DEADLINE, report));
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
            (Some(index), Some(last)) if outcome.failure.is_some() => {
                last.overlay(index);
                self.parsing.generation += 1;
            }
            (Some(index), _) => {
                self.parsing.index = Some(index);
                self.parsing.generation += 1;
            }
            (None, _) => {}
        }
        match outcome.failure {
            Some(failure) => self.log_parse(&format!("dbt parse failed: {failure}")),
            None => self.parsing.failure = None,
        }
        self.on_problems(outcome.problems);
        // Alone, a change during this parse wants the next one now: no
        // request may come to ask for it.
        if self.standalone {
            self.want_index();
        }
    }

    /// The errors and warnings of a parse that ran to its end, PROBLEMS, in
    /// place of the last parse's, alone only, since dbt lsp shows its own:
    /// each in its file, the root's if it is there, else the one in dbt's
    /// working directory, else the one package's that holds it; one in no
    /// such file is logged, once.  Each file with problems, before or now,
    /// is published again.
    fn on_problems(&mut self, problems: Option<Vec<parse_log::Problem>>) {
        let (true, Some(problems)) = (self.standalone, problems) else { return };
        // The packages, while the project's configuration still reads.
        let packages: Vec<PathBuf> =
            self.project().map_or_else(Vec::new, |project| project.packages.values().cloned().collect());
        // dbt names a YAML file's error from its working directory, and a
        // package's file from its package.
        let cwd = self.dbt_dir();
        let mut placed: HashMap<PathBuf, Vec<parse_log::Problem>> = HashMap::new();
        for problem in problems {
            let path = problem.place.as_ref().and_then(|place| {
                // dbt names a file from a sibling directory with `..`.
                let file = |dir: &Path| Some(lexical(&dir.join(&place.path))).filter(|path| path.is_file());
                let in_cwd =
                    || cwd.as_deref().and_then(file).and_then(|path| self.local_path(Some(&features::uri(&path))));
                let mut held = packages.iter().filter_map(|dir| file(dir));
                let one_held = || held.next().filter(|_| held.next().is_none());
                self.root.as_deref().and_then(file).or_else(in_cwd).or_else(one_held)
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
    /// client's folder or a document named it, else the proxy's.
    fn dbt_dir(&self) -> Option<PathBuf> {
        self.parsing.dir.clone().or_else(|| std::env::current_dir().ok())
    }

    /// Asks a client that can refresh its inlay hints to do so, since they
    /// show the relations of the index.
    fn refresh_hints(&mut self) {
        if !self.hint_refresh || self.shutdown {
            return;
        }
        self.ask_client("workspace/inlayHint/refresh", None);
    }

    /// Sends the client the proxy's own request METHOD, with PARAMS if any,
    /// under an id of the proxy's own, whose answer stays here.
    fn ask_client(&mut self, method: &str, params: Option<Value>) {
        self.next_id += 1;
        let id = json!(format!("{OWN_ID}{}", self.next_id));
        self.asked.insert(key(&id));
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        self.send_client(&request);
    }

    /// Stops a parse and a run under way and removes this process's cache.
    fn stop_parse(&mut self) {
        let under_way = [self.parsing.running.take(), self.run.take()];
        under_way.into_iter().flatten().for_each(config::Running::stop);
        if let Some(cache) = self.parsing.cache.take() {
            let _ = fs::remove_dir_all(cache);
        }
    }

    /// Answers a request that the proxy answers itself, which it remembers as
    /// PENDING.  A refusal fails it with its reason; a panic while answering
    /// fails that request alone, never the session.
    fn answer(&mut self, method: &str, id: &Value, params: &Value, pending: &Pending) {
        // Alone, the call hierarchy's graph needs the index too.
        let hierarchy = method.ends_with("CallHierarchy") || method.starts_with("callHierarchy/");
        if WANT_INDEX.contains(&method) || (self.standalone && hierarchy) {
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
            "textDocument/references" => self.on_references(params),
            "textDocument/definition" => self.on_definition(params),
            "textDocument/rename" => self.on_rename(params),
            "textDocument/foldingRange" | "textDocument/documentHighlight" | "textDocument/selectionRange" => {
                self.on_document(method, params).map(Ok)
            }
            // As dbt lsp 2.0.9's answer with nothing of its own gets them,
            // null for a hover, no hint: the proxy's part, else that.
            "textDocument/hover" | "textDocument/inlayHint" => {
                Some(Ok(pending.alone(self.markdown).unwrap_or_else(|| modes::empty(method))))
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
                self.nodes_generation += 1;
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
            // The server gets the client's `initialize` without the proxy's own section.
            (Some("initialize"), Some(id)) if params.pointer("/initializationOptions/dbtLspPlus").is_some() => {
                let body = json!({"jsonrpc": "2.0", "id": id, "method": "initialize",
                                  "params": modes::without_section(params)});
                self.on_request("initialize", id, params, body.to_string().as_bytes())
            }
            (Some(method), Some(id)) => self.on_request(method, id, params, &frame.body),
            (Some(method), None) => self.on_notification(method, params, &frame.body),
            // The client's answer to the proxy's own request was never the server's.
            (None, Some(id)) if self.asked.remove(&key(id)) => {}
            _ => self.send_server(&frame.body),
        }
    }

    /// A notification of the client's, METHOD with PARAMS, BODY as it came:
    /// kept and forwarded; when the graph may have changed, a listing, and,
    /// alone, a parse, which nothing else would start.  Alone with a project,
    /// `initialized` registers the watcher that dbt lsp would.  Before a
    /// project, the first document opened in one names it.
    fn on_notification(&mut self, method: &str, params: &Value, body: &[u8]) {
        if method == "textDocument/didOpen" && self.root.is_none() && self.initialized && !self.shutdown {
            self.on_first_project(params);
        }
        self.track(method, params);
        // The modes are fixed with the flag that the client read at `initialize`.
        let setting = params.pointer("/settings/dbtLspPlus").is_some();
        if let ("workspace/didChangeConfiguration", true, false) = (method, setting, self.ignored) {
            self.ignored = true;
            let text =
                "dbt-lsp-plus reads dbtLspPlus from initializationOptions and its command line only, at the start";
            self.send_client(
                &json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 3, "message": text}}),
            );
        }
        // The server gets the change before the listing it prompts.
        self.send_server(body);
        if method == "initialized" && self.standalone && self.root.is_some() {
            self.register_watcher();
        }
        // The first document opened in the project.
        let first_open =
            method == "textDocument/didOpen" && self.root.is_some() && !std::mem::replace(&mut self.opened, true);
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

    /// The project of the document that PARAMS opens, when it lies in one,
    /// before the document is tracked: taken, alone, with the first scan,
    /// the watcher of runs and the watcher of files that `initialize` and
    /// `initialized` start with a project; then the line that says so.
    fn on_first_project(&mut self, params: &Value) {
        let path = uri_path(params.pointer("/textDocument/uri").and_then(Value::as_str));
        let Some(root) = path.and_then(|path| locate::project_of(&path, &self.parsing.server)) else { return };
        self.take_project(root, true);
        self.scan_first();
        self.watch_runs();
        self.register_watcher();
        self.say_found();
    }

    /// Registers, alone, the watcher that dbt lsp would, of every file, for a
    /// client that lets it: an edit outside the editor changes the graph too.
    fn register_watcher(&mut self) {
        if self.watches {
            let watcher = json!({"id": "dbt-lsp-plus-file-watcher", "method": "workspace/didChangeWatchedFiles",
                                 "registerOptions": {"watchers": [{"globPattern": "**/*"}]}});
            self.ask_client("client/registerCapability", Some(json!({"registrations": [watcher]})));
        }
    }

    /// What the proxy keeps of the client's request METHOD, and the lifecycle
    /// it starts: the client's `initialize` and `shutdown`, a listing that the
    /// proxy must not cancel, the summary that a hover's answer gets, a
    /// request that the shadow follows, the nothing that a merged request
    /// falls back to; and whether, once routed, it asks for a fresh index: a
    /// hover keeps the index fresh, an inlay hint only makes one, and in
    /// `dbt` neither; a shadowed request wants what `plus` reads.
    fn remember(&mut self, method: &str, id: &Value, params: &Value) -> (Pending, bool) {
        // Outside `dbt`, a hover, hints, a definition or references with no part of the proxy's.
        let nothing = Pending::Nothing(modes::empty(method));
        let pending = match (method, params["command"].as_str()) {
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
            (_, Some("dbt.listNodes" | commands::NODES)) => Pending::Listing,
            ("textDocument/hover", _) if self.table.mode(method) != Mode::Dbt => {
                let index = self.parsing.index.as_ref();
                let summary =
                    index.and_then(|index| self.node_at(index, params, false).map(|node| (index.summary(node), true)));
                // A doc's or a generic test's block, the project scanned only then.
                let block = || {
                    let (path, text, offset) = self.document_at(params)?;
                    let target = navigation::target_at(&path, &text, offset)
                        .filter(|target| matches!(target, navigation::Target::Named { kind: "doc" | "test", .. }))?;
                    let project = self.project()?;
                    let found = navigation::resolve(
                        &navigation::Lookup::new(&project, self.parsing.index.as_ref()),
                        &path,
                        &target,
                    );
                    navigation::hover(&project, &target, found.first()?, self.markdown).map(|text| (text, false))
                };
                return (summary.or_else(block).map_or(nothing, |(text, hard)| Pending::Hover(text, hard)), true);
            }
            ("textDocument/inlayHint", _) if self.table.mode(method) != Mode::Dbt => {
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
                return (hints.map_or(nothing, Pending::Hints), self.parsing.index.is_none());
            }
            // Followed only once forwarded: one the proxy answers is no one's to compare, and `plus` forwards none.
            ("textDocument/definition" | "textDocument/references", _) => {
                let changes = self.changes;
                let shadowed = self
                    .shadow
                    .as_ref()
                    .filter(|_| self.table.mode(method) != Mode::Plus)
                    .map(|_| Pending::Shadow { method: method.to_owned(), params: params.clone(), changes });
                let fresh = shadowed.is_some();
                let mine = self.table.mode(method) != Mode::Dbt;
                return (shadowed.unwrap_or(if mine { nothing } else { Pending::Request }), fresh);
            }
            _ => Pending::Request,
        };
        (pending, false)
    }

    /// A request of the client: routed by `route`; without a server,
    /// `initialize` is answered here, and any other forwarded request fails.
    fn on_request(&mut self, method: &str, id: &Value, params: &Value, body: &[u8]) {
        // The server would get two requests under one id.
        if self.listing.as_ref().is_some_and(|(own, ..)| id.as_str() == Some(own)) {
            return self.reply_error(id, INVALID_REQUEST, "dbt-lsp-plus has a request in flight under this id");
        }
        let (pending, fresh) = self.remember(method, id, params);
        match (self.route(method, params), &self.server, method) {
            (Route::Here(owned), ..) => self.answer(owned, id, params, &pending),
            (Route::Answer(result), ..) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
            (Route::Command(command), ..) => self.on_command(command, id, params, pending),
            (Route::Forward, Some(_), _) => {
                // dbt lsp adds to the proxy's part, or replaces its nothing, but never holds it up.
                let late = pending.deadline().map(|deadline| (Instant::now() + deadline, method.to_owned()));
                self.deadlines.extend(late.map(|late| (key(id), late)));
                self.forwarded.insert(key(id), (id.clone(), pending));
                self.send_server(body);
            }
            (Route::Forward, None, "initialize") => {
                self.initialize = None;
                self.initialize_alone(id);
            }
            // A merged request gets the proxy's answer alone, any other the failure.
            (Route::Forward, None, _) => match pending.alone(self.markdown) {
                Some(part) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": part})),
                None => self.reply_error(id, REQUEST_FAILED, "dbt lsp is not running"),
            },
        }
        // Once the request is forwarded, so that a listing takes no id of the client's.
        if fresh {
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
        self.deadlines.remove(&key(id));
        // The client had the proxy's part alone: the late answer is no one's.
        if let Some((_, Pending::Answered)) = pending {
            return;
        }
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
            // The shadow follows the server that answered; its log is under the user's cache, if any.
            self.shadow = self.settings.shadow.then(|| {
                let log =
                    config::cache_base().zip(self.root.as_ref()).map(|(base, root)| shadow::Log::open(&base, root));
                shadow::Session::new(log)
            });
            if let Some(capabilities) = message.pointer_mut("/result/capabilities").and_then(Value::as_object_mut) {
                self.add_capabilities(capabilities);
            }
            self.send_client(&message);
            self.tell_settings();
            self.say_found();
            self.shadow_line(&shadow::start(&message["result"]["serverInfo"], &self.table));
            return self.watch_runs();
        }
        self.send_client_body(body);
        if let Some((_, Pending::Shadow { method, params, changes })) = pending {
            self.compare(&method, &params, changes, message);
        }
    }

    /// Compares dbt lsp's answer MESSAGE, which the client already has, to a
    /// request that the shadow follows, METHOD with PARAMS after CHANGES
    /// changes, with what the proxy answers there whatever the flag lists,
    /// but for references outside the root, which it never answers; then
    /// counts it and logs it.  A panic costs the comparison alone.
    fn compare(&mut self, method: &str, params: &Value, changes: u64, message: &Value) {
        let record = panic::catch_unwind(AssertUnwindSafe(|| {
            let root = self.root.as_deref().unwrap_or(Path::new(""));
            // Outside the root, each side may spell a file its own way.
            let path_of = |uri: &str| {
                let path = self.local_path(Some(uri))?;
                Some(if path.starts_with(root) { path } else { fs::canonicalize(&path).unwrap_or(path) })
            };
            let file = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str));
            let inside = file.as_deref().is_some_and(|file| file.starts_with(root));
            let compared = match shadow::read(message, self.changes != changes, &path_of, &|path| path.is_file()) {
                shadow::Answer::Apart(relation) => {
                    shadow::Compared { relation, ours_only: Vec::new(), dbt_only: Vec::new(), both: 0 }
                }
                shadow::Answer::Locations(dbt) => {
                    let ours = match method {
                        "textDocument/references" if inside => self.on_references(params),
                        "textDocument/references" => None,
                        _ => self.on_definition(params),
                    };
                    shadow::compare(&shadow::locations(&ours.and_then(Result::ok).unwrap_or_default(), &path_of), &dbt)
                }
            };
            let read = self.document_at(params);
            let target = read.and_then(|(path, text, offset)| navigation::target_at(&path, &text, offset));
            let relative = file.as_deref().and_then(|file| file.strip_prefix(root).ok());
            let known = self.parsing.index.as_ref().zip(self.project()).zip(relative.zip(target.as_ref()));
            let oracle = known.and_then(|((index, project), (relative, target))| {
                shadow::oracle(index, &project, &relative.to_string_lossy(), target)
            });
            let parsing = &self.parsing;
            let index = json!({"present": parsing.index.is_some(), "fresh": parsing.fresh,
                               "running": parsing.running.is_some()});
            shadow::Record {
                method: method.trim_start_matches("textDocument/").to_owned(),
                kind: target.as_ref().map_or("none", navigation::Target::kind),
                place: file.as_deref().and_then(place).unwrap_or("other"),
                target: target.as_ref().map_or(Value::Null, shadow::target),
                file,
                compared,
                index,
                oracle,
            }
        }));
        let Ok(record) = record else { return };
        let line = record.line(self.root.as_deref().unwrap_or(Path::new("")));
        if let Some(shadow) = &mut self.shadow {
            shadow.counts.add(&record);
        }
        self.shadow_line(&line);
    }

    /// Logs LINE in the shadow, if on; a log that cannot be written is said
    /// once, out of the user's way, and the shadow goes on counting.
    fn shadow_line(&mut self, line: &str) {
        let failed = self.shadow.as_mut().and_then(|shadow| shadow.write(line));
        if let (Some(text), false) = (failed, self.shutdown) {
            self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage",
                                     "params": {"type": 2, "message": text}}));
        }
    }

    /// The server's diagnostics of a file, BODY as it came: forwarded as
    /// they are, unless the last run failed there too.
    fn on_server_diagnostics(&mut self, message: &Value, body: &[u8]) {
        match self.table.mode("textDocument/publishDiagnostics") {
            Mode::Dbt => return self.send_client_body(body),
            Mode::Plus => return,
            Mode::Merge => {}
        }
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
        // In `dbt`, the diagnostics are the server's alone.
        if self.table.mode("textDocument/publishDiagnostics") == Mode::Dbt {
            return;
        }
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

    /// Publishes PATH's diagnostics, the server's last, the last run's
    /// failures and the last parse's problems, under the server's URI for
    /// it, else the client's, else the file's,
    /// with no version; the URI last published under, if another, gets
    /// none.  Nothing after `shutdown`.
    fn publish(&mut self, path: &Path) {
        if self.shutdown || self.table.mode("textDocument/publishDiagnostics") == Mode::Dbt {
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
        // At dbt's line and column, from 1 and in code points; a file's start
        // when dbt gives none.
        let problems = self.diagnostics.parsed.get(path).map_or(&[][..], Vec::as_slice);
        diagnostics.extend(problems.iter().map(|problem| {
            let (line, column) = problem.place.as_ref().and_then(|place| place.at).unwrap_or((1, 1));
            let at = lines.offset(line.saturating_sub(1), column.saturating_sub(1), Encoding::Utf32);
            json!({"range": lines.range(at, at, self.encoding), "severity": problem.severity,
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
        self.deadlines.clear();
        for (_, (id, pending)) in std::mem::take(&mut self.forwarded) {
            let part = pending.alone(self.markdown);
            match (pending, part) {
                (Pending::Shutdown, _) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": null})),
                (Pending::Answered, _) => {}
                (_, Some(part)) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": part})),
                _ => self.reply_error(&id, REQUEST_FAILED, &why),
            }
        }
        self.listing = None;
        self.attempts = 0;
        self.listed.clear();
        self.relist = false;
        self.nodes = None;
        self.nodes_generation += 1;
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
        match spawn_server(&self.parsing.server, &events, self.restart.spawned, self.parsing.dir.as_deref()) {
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
        // A merge past its deadline: the proxy's part or nothing alone, the server's late answer dropped.
        let due: Vec<(String, String)> =
            self.deadlines.iter().filter(|(_, (at, _))| *at <= now).map(|(k, (_, m))| (k.clone(), m.clone())).collect();
        for (late, method) in due {
            self.deadlines.remove(&late);
            let Some((id, pending)) = self.forwarded.get_mut(&late) else { continue };
            let waited = pending.deadline().unwrap_or(MERGE_DEADLINE);
            let (id, part) = (id.clone(), std::mem::replace(pending, Pending::Answered).alone(self.markdown));
            self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": part}));
            if !self.shutdown && self.late.insert(method.clone()) {
                let ms = waited.as_millis();
                let text = format!("dbt lsp did not answer {method} within {ms} ms: dbt-lsp-plus answered alone");
                self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage",
                                         "params": {"type": 4, "message": text}}));
            }
        }
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
            let merge = self.deadlines.values().map(|(at, _)| *at).min();
            let next = [listing, self.restart.due, replay, merge].into_iter().flatten().min();
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
        // A new epoch: the cache looks at the project's files again.
        self.epoch += 1;
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
            Event::Ran(id, result) => self.on_ran(&id, &result),
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

/// The server COMMAND started as process GENERATION, in DIR when given, its
/// output read into EVENTS; else why not.
fn spawn_server(
    command: &[String],
    events: &Sender<Event>,
    generation: u64,
    dir: Option<&Path>,
) -> Result<Server, String> {
    let mut spawned = Command::new(&command[0]);
    spawned.args(&command[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    if let Some(dir) = dir {
        spawned.current_dir(dir);
    }
    let mut child = spawned.spawn().map_err(|error| {
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

/// Runs the proxy on the standard input and output, in front of COMMAND,
/// alone when its dbt has no `lsp`.  Given COMMAND's `--project-dir`, the
/// probe, the server and the cut start now, in the proxy's working
/// directory; else nothing runs before the client's folder or a document
/// names the project.
pub fn run(command: &[String], settings: modes::Settings, backend: modes::Backend) -> ExitCode {
    let (events, inbox) = mpsc::channel();
    // With `never`, nothing of the user's dbt runs but its parse.
    let never = backend == modes::Backend::Never;
    let given = project_dir(command);
    let standalone = never || (given.is_some() && locate::probe(command, None) == Probe::NoLsp);
    let (server, absent) = match (given.is_some() && !standalone).then(|| spawn_server(command, &events, 0, None)) {
        Some(Ok(server)) => (Some(server), None),
        Some(Err(why)) => (None, Some(why)),
        None => (None, None),
    };
    let cut = given.is_some() && config::can_cut();
    if given.is_some() && !cut {
        eprintln!("{NO_CUT}");
    }
    let parsing = Parsing { server: command.to_vec(), events: Some(events.clone()), cut, ..Parsing::default() };
    spawn_reader(io::stdin(), events, Event::Client, Event::ClientClosed);
    let mut proxy = Proxy::new(io::BufWriter::new(io::stdout().lock()), server, absent, given);
    proxy.standalone = standalone;
    (proxy.never, proxy.settings) = (never, settings);
    proxy.parsing = parsing;
    proxy.serve(inbox);
    // The shadow's last line: the session's counts.
    if let Some(end) = proxy.shadow.as_ref().map(|shadow| shadow.counts.end()) {
        proxy.shadow_line(&end);
    }
    proxy.stop_parse();
    proxy.stop_server();
    if proxy.shutdown { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_the_proxy_adds_or_takes_has_a_mode() {
        let mut settings = modes::Settings::default();
        settings.set_default("plus").unwrap();
        let table = modes::Table::new(&settings, &|_| false, false);
        let added = ADDED.iter().flat_map(|(_, methods)| methods.iter());
        for method in added.chain(PROVIDERS.iter().map(|(_, method, _)| method)) {
            assert_eq!(table.mode(method), Mode::Plus, "{method} is not among modes::METHODS");
        }
    }

    #[test]
    fn the_flag_lists_every_row_in_plus_and_merge_with_a_backend_or_none() {
        let every = json!({"version": 2,
            "definition": {"source": ["sql", "yaml", "python"], "ref": ["sql", "yaml", "python"], "macro": ["sql", "yaml"],
                           "doc": ["yaml"], "entry": ["yaml"], "test": ["yaml"], "function": ["sql", "yaml"]},
            "references": {"source": ["sql", "yaml", "python"], "ref": ["sql", "yaml", "python"],
                           "macro": ["sql", "yaml"], "function": ["sql", "yaml"]},
            "rename": {"source": ["sql", "yaml", "python"], "cte": ["sql"]}});
        let mut plus = modes::Settings::default();
        plus.set_default("plus").unwrap();
        for (settings, none) in [(modes::Settings::default(), false), (modes::Settings::default(), true), (plus, false)]
        {
            assert_eq!(modes::Table::new(&settings, &|_| false, none).flag(&ANSWERED), every, "{settings:?} {none}");
        }
        let mut dbt = modes::Settings::default();
        dbt.set("textDocument/definition", "dbt").unwrap();
        dbt.set("textDocument/references", "dbt").unwrap();
        let rename = json!({"version": 2, "rename": every["rename"]});
        assert_eq!(modes::Table::new(&dbt, &|_| false, false).flag(&ANSWERED), rename);
    }

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
