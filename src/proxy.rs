// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy: forwards every message between the editor and `dbt lsp`
//! byte for byte, and answers itself the requests of the features that the
//! server does not announce.

use crate::answer::{Answer, Reason};
use crate::capability::{self, CAPABILITIES, Provider};
use crate::commands;
use crate::graph::{self, ServerNode};
use crate::locate::{self, Probe};
use crate::modes::{self, Mode};
use crate::project::{Project, Scanned};
use crate::rpc::{self, Frame};
use crate::text::{self, Encoding, LineStarts};
use crate::workspace::{Which, Workspace};
use crate::{
    cache, config, document, features, navigation, packages, parse_log, properties, reference, run, runs, shadow,
    sources,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufReader, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, ExitStatus, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use url::Url;

/// The prefix of the ids of the requests that the proxy itself sends to the
/// server; the client's ids pass unchanged.
pub const OWN_ID: &str = "dbt-lsp-plus/";
/// How long the server has to exit after `exit` before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);
const INVALID_REQUEST: i64 = -32600;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// How the proxy answers a request of the client's.
enum Route {
    /// Itself, as this method.
    Here(&'static str),
    /// Itself, with this answer: null before a project, the shadow's
    /// counts, or the method's nothing.
    Answer(Answer),
    /// By the server: `forward`, which also says what the request gets
    /// when none runs.
    Forward,
    /// As this command of the proxy's for clients.
    Command(&'static str),
}

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
/// How long dbt lsp compiles before the proxy says it has not ended: the
/// longest compilation seen on 2.0.9 took 2.7 s, the first of 3,000 models
/// on DuckDB; a warehouse may take longer, and 60 s is past every client's
/// timeout.
const COMPILE_STALL: Duration = Duration::from_secs(60);
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

/// What a watched file and the manifest give: its failures, or none to keep
/// those known.
type ReadFailures = fn(&Value, &Value) -> Option<Vec<runs::Failure>>;

/// How often the files of the last run are looked at.
const RUNS_EVERY: Duration = Duration::from_secs(1);

/// Why no dbt lsp runs before any project, said to no one.
const NO_PROJECT: &str = "no dbt project yet";

/// The `dbt lsp` process behind the session, at most one, since one
/// `initialize` answer gives one capability set, and what it told the
/// client: the model's Backend.
#[derive(Default)]
struct Backend {
    life: Life,
    /// When each restart of the window began.
    restarts: Vec<Instant>,
    /// The generation of the last process started, from 0.
    spawned: u64,
    /// The ids of its registrations, which the client holds across lives.
    registered: HashSet<String>,
    /// The progress tokens it began and has not ended.
    progress: HashMap<String, Value>,
    /// The commands it listed in its answer to the client's `initialize`.
    commands: HashSet<String>,
    /// How many times its nodes changed or went: the graph holds while this stays.
    nodes_generation: u64,
    /// The failures said this session, each by its first error's key.
    said: HashSet<String>,
    /// Whether a compilation with no end was said: once a session.
    stalled: bool,
}

/// Where the Backend is in its life.
enum Life {
    /// None was started: why, as the client is told (the backend is
    /// never; on the given route `<dbt> has no lsp`, on the folder's `it
    /// has no lsp`; dbt lsp is not started for a project found from a
    /// file), after which none will be; or no project yet, before which
    /// none can be.
    NotStarted(String),
    /// This process runs: the first, which the client's `initialize`
    /// reaches, or a restarted one that answered its own.
    Running(Running),
    /// Lost: a restart is due then.
    Waiting(Instant),
    /// Restarted, until it answers its `initialize`: the process, the id
    /// of that request, the deadline of its answer.
    Starting(Server, String, Instant),
    /// None runs, and none will: why (it could not start, it exited
    /// before the client's `initialize` was answered or after `shutdown`,
    /// or past its restarts).
    Lost(String),
}

impl Default for Life {
    fn default() -> Self {
        Life::NotStarted(NO_PROJECT.to_owned())
    }
}

/// A running `dbt lsp`: its process, and what the proxy asked of it and
/// timed, which end with it.
struct Running {
    server: Server,
    listings: Listings,
    compiling: Compiling,
}

impl Running {
    fn new(server: Server) -> Running {
        Running { server, listings: Listings::default(), compiling: Compiling::default() }
    }

    /// Sends the listing of selector SLOT, under an id past NEXT_ID that no
    /// request of the client's in FORWARDED carries, as the listing in
    /// flight; nothing once the client asked for SHUTDOWN, after which LSP
    /// allows only `exit`.
    fn send_list(
        &mut self,
        slot: usize,
        next_id: &mut u64,
        forwarded: &HashMap<String, (Value, Pending)>,
        shutdown: bool,
    ) {
        if shutdown {
            return;
        }
        let id = loop {
            *next_id += 1;
            let id = format!("{OWN_ID}{next_id}");
            if !forwarded.contains_key(&key(&json!(id))) {
                break id;
            }
        };
        let params = json!({"command": "dbt.listNodes", "arguments": [SELECTORS[slot]]});
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "workspace/executeCommand", "params": params});
        self.listings.listing = Some((id, slot, Instant::now()));
        // A failed write means the server is gone; its reader reports it.
        let _ = rpc::write_body(&mut self.server.stdin, &serde_json::to_vec(&request).unwrap_or_default());
    }
}

/// The proxy's listings of one process, and the nodes they gave: a new
/// process's are the default.
#[derive(Default)]
struct Listings {
    /// The proxy's `dbt.listNodes` in flight: its id, its selector's slot, when it was sent.
    listing: Option<(String, usize, Instant)>,
    /// The nodes of the first selector, while the second is listed.
    listed: Vec<ServerNode>,
    attempts: u32,
    /// The graph may have changed while it could not be listed: list it later.
    relist: bool,
    nodes: Option<Vec<ServerNode>>,
}

impl Backend {
    /// Whether no dbt lsp answers now, nor will in this session: none was
    /// started, or it was lost for good.
    fn alone(&self) -> bool {
        matches!(self.life, Life::NotStarted(_) | Life::Lost(_))
    }

    fn running(&self) -> Option<&Running> {
        match &self.life {
            Life::Running(running) => Some(running),
            _ => None,
        }
    }

    fn running_mut(&mut self) -> Option<&mut Running> {
        match &mut self.life {
            Life::Running(running) => Some(running),
            _ => None,
        }
    }

    /// The next instant at which the life needs the loop: a restart's
    /// due, a restarted process's deadline, a running one's listing's
    /// deadline or its compilation's.
    fn due(&self) -> Option<Instant> {
        match &self.life {
            Life::Waiting(due) => Some(*due),
            Life::Starting(.., deadline) => Some(*deadline),
            Life::Running(running) => {
                let listing = running.listings.listing.as_ref().map(|(_, _, sent)| *sent + LIST_TIMEOUT);
                [listing, running.compiling.due()].into_iter().flatten().min()
            }
            Life::NotStarted(_) | Life::Lost(_) => None,
        }
    }
}

/// The client's, which a restart sends again.
#[derive(Default)]
struct Replay {
    /// The client's `initialize` parameters.
    initialize: Option<Value>,
    /// The language and last version of each open document.
    languages: HashMap<PathBuf, Value>,
    versions: HashMap<PathBuf, Value>,
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

impl Server {
    /// Ends the process: its input closed, GRACE to exit, then killed
    /// unless it exited, and reaped; its status, if known.
    fn reap(self, grace: Duration) -> Option<ExitStatus> {
        let Server { mut child, stdin, .. } = self;
        drop(stdin);
        let deadline = Instant::now() + grace;
        let mut status = child.try_wait().ok().flatten();
        while status.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
            status = child.try_wait().ok().flatten();
        }
        if status.is_none() {
            let _ = child.kill();
        }
        status.or_else(|| child.wait().ok())
    }
}

/// What the proxy remembers of a request of the client's that it forwarded.
#[derive(PartialEq)]
struct Pending {
    /// The method, or for `workspace/executeCommand` the command.
    name: String,
    kept: Kept,
}

/// What the proxy keeps of a forwarded request.
#[derive(PartialEq)]
enum Kept {
    /// Nothing of the proxy's: in `dbt`, a listing, `shutdown`, any other.
    Plain,
    /// A merged request: the proxy's part.
    Part(Part),
    /// Answered with the proxy's part alone once the server was late: its
    /// answer, when it comes, is dropped.
    Answered,
    /// A definition or references that the shadow follows: its parameters,
    /// and the count of the client's changes when it came.
    Shadow(Value, u64),
}

/// The proxy's part of a merged request.
#[derive(Debug, PartialEq)]
enum Part {
    /// Nothing of the proxy's at the position: the request is dbt lsp's,
    /// whose answer passes as it came, and when it gives none, the client
    /// gets why.
    Nothing,
    /// The proxy found nothing, and that is its answer: at a position it
    /// claims, for hints, or where no dbt lsp was to answer.  Its nothing,
    /// null or `[]`, is the answer when the server gives none.
    Empty,
    /// A hover whose answer gets this text first, and whether its line
    /// breaks are hard in markdown: true for a node's summary and facts,
    /// lines of plain text and code spans; false for a docs block's text or
    /// a test's signature, markdown as written.
    Hover(String, bool),
    /// An inlay hint request, whose answer gets these hints after its own.
    Hints(Vec<Value>),
    /// A definition or references at a position the proxy claims, whose
    /// answer gets the server's other locations after these.
    Locations(Value),
    /// A hover whose position names a node while no index exists: why the
    /// proxy has no index yet, where dbt lsp's answer is null or none, and
    /// before why dbt lsp gave none.
    Unindexed,
    /// What the proxy cannot answer, for these reasons: a definition or
    /// references at a position it claims, or a hover at a `ref` or a
    /// `source` once there is an index, over a project that does not read;
    /// a hover, a definition or references at any position of a document
    /// of another project (`on_elsewhere`).  dbt lsp's answer passes as it
    /// came; when it gives none, these reasons, then why.  Alone and in
    /// `plus`, a hover's reasons are its answer.
    Unanswered(Vec<Reason>),
}

impl Pending {
    /// The server's answer, MESSAGE, as this kind of part makes it, for a
    /// client that reads markdown when MARKDOWN, as WORKSPACE stands, its
    /// locations' URIs read as paths by PATH_OF; none to pass it unchanged.
    /// An error to a merged request gives what `unserved` gives for it, and
    /// a null to a hover on a node before any index why the proxy has none.
    fn merged(
        &self,
        message: &Value,
        markdown: bool,
        workspace: &Workspace,
        path_of: &dyn Fn(&str) -> Option<PathBuf>,
    ) -> Option<Value> {
        if let (Some(error), Kept::Part(_)) = (message.get("error"), &self.kept) {
            let failed = Reason::Failed(error["message"].as_str().unwrap_or_default().to_owned());
            return Some(self.unserved(markdown, Some(failed), workspace).frame(&message["id"], &self.name));
        }
        match &self.kept {
            Kept::Part(Part::Hover(text, hard)) => features::merge_hover(message, text, *hard, markdown),
            Kept::Part(Part::Hints(hints)) => features::merge_hints(message, hints),
            Kept::Part(Part::Locations(found)) => Some(features::merge_locations(message, found, path_of)),
            Kept::Part(Part::Unindexed) if message["result"].is_null() && workspace.index.is_none() => {
                Some(Answer::Unanswered(vec![workspace.unindexed()]).frame(&message["id"], &self.name))
            }
            _ => None,
        }
    }

    /// The proxy's part found, by itself, for a client that reads markdown
    /// when MARKDOWN: a hover's text, the hints, the locations; none for its
    /// nothing, and for a request that is not merged.
    fn found(&self, markdown: bool) -> Option<Value> {
        let Kept::Part(part) = &self.kept else { return None };
        match part {
            Part::Nothing | Part::Empty | Part::Unindexed | Part::Unanswered(_) => None,
            Part::Hover(text, hard) => {
                let alone = features::merge_hover(&json!({"result": null}), text, *hard, markdown);
                alone.map(|alone| alone["result"].clone())
            }
            Part::Hints(hints) => Some(Value::Array(hints.clone())),
            Part::Locations(found) => Some(found.clone()),
        }
    }

    /// What the client gets for this request from the proxy's part, as it
    /// reads markdown when MARKDOWN, as WORKSPACE stands, when dbt lsp gives
    /// it no answer, for WHY, or answered whole, with none: the proxy's
    /// part found; its empty; why it could not answer, then WHY; for a
    /// hover on a node before any index, why the proxy has none, then WHY;
    /// else WHY, or with none the method's nothing.  An index made since
    /// the request makes the hover's part a nothing.
    fn unserved(&self, markdown: bool, why: Option<Reason>, workspace: &Workspace) -> Answer {
        match (&self.kept, self.found(markdown)) {
            (_, Some(found)) => Answer::Found(found),
            (Kept::Part(Part::Empty), None) => Answer::Empty,
            (Kept::Part(Part::Unanswered(reasons)), None) => {
                Answer::Unanswered(reasons.iter().cloned().chain(why).collect())
            }
            (Kept::Part(Part::Unindexed), None) if workspace.index.is_none() => {
                Answer::Unanswered([workspace.unindexed()].into_iter().chain(why).collect())
            }
            _ => why.map_or(Answer::Empty, |why| Answer::Unanswered(vec![why])),
        }
    }

    /// How long the server has to answer this request before the client
    /// gets the proxy's answer alone, or why there is none, as its
    /// capability waits: a part is worth showing at once, a nothing or an
    /// empty only once the server is clearly late.
    fn deadline(&self) -> Option<Duration> {
        let Kept::Part(part) = &self.kept else { return None };
        let nothing = matches!(part, Part::Nothing | Part::Empty | Part::Unindexed | Part::Unanswered(_));
        capability::of(&self.name)?.deadline(!nothing)
    }

    /// Whether it is a listing, the server's `dbt.listNodes` or the
    /// proxy's command that it does: the proxy lists only when none is
    /// pending, since a second listing cancels the first.
    fn listing(&self) -> bool {
        self.name == "dbt.listNodes" || self.name == commands::NODES
    }
}

/// When dbt may parse in front of dbt lsp, and where the session's threads
/// report; the parse itself is the Workspace's.
#[derive(Default)]
struct Parsing {
    /// Where a parse reports; none in the unit tests, which parse nothing.
    events: Option<Sender<Event>>,
    /// Whether the server compiled since the last change: dbt has then
    /// resolved the packages itself.
    compiled: bool,
    /// Whether a request wanted an index before the server had compiled:
    /// the server's compilation then starts the parse.
    wanted: bool,
}

/// What a dbt lsp process said of its compilations: whether one runs.
#[derive(Default)]
struct Compiling {
    /// Since when it has compiled, from its last start, with no end since.
    since: Option<Instant>,
    /// Whether that compilation's deadline has passed, said or not.
    told: bool,
}

impl Compiling {
    /// When the compilation under way has gone `COMPILE_STALL` with no end;
    /// none once that is `told`, and with no compilation.
    fn due(&self) -> Option<Instant> {
        self.since.filter(|_| !self.told).map(|since| since + COMPILE_STALL)
    }
}

struct Proxy<W: Write> {
    client: W,
    backend: Backend,
    /// The line that says which project and which dbt the proxy found,
    /// told to the client once it can be.
    line: Option<String>,
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
    /// Whether `--backend` is `never`, which the proxy says once alone.
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
    /// The methods whose late answer the proxy logged, each with the deadline that passed and what the client
    /// got, as the log says it.
    late: HashSet<(String, Duration, &'static str)>,
    documents: HashMap<PathBuf, String>,
    /// The URI under which the client opened each of them, which an edit
    /// of it must name.
    uris: HashMap<PathBuf, String>,
    /// Whether the client opened a document, the first of which prompts a listing.
    opened: bool,
    next_id: u64,
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
    replay: Replay,
    parsing: Parsing,
    /// The core's state of the session's project, which the proxy hands
    /// the open texts: its root, its dbt, its scan, its parse and index,
    /// its graph.
    workspace: Workspace,
    /// The count of each open document's edits, under which the cache keeps
    /// it, and the count so far.
    edits: HashMap<PathBuf, u64>,
    edited: u64,
    /// The count of the client's changes and closes of a document, after
    /// which the shadow compares no answer to an earlier request.
    changes: u64,
    /// The shadow, on from the server's answer to `initialize` when the user
    /// set it.
    shadow: Option<shadow::Shadow>,
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

/// What a compilation that compiled nothing reports, from the PARAMS of
/// its completion, `dbt/lspCompileComplete` or its background one: none
/// unless `models_count` is there and null and `errors` holds an error, as
/// dbt lsp 2.0.9 reports when it compiled nothing (with no error, a first
/// compilation that a save or a second listing cut short); else that first
/// error's message on one line, and its key, that text before the first of
/// dbt's places, ` (in ` or ` --> `.  A warning is never quoted.
fn compile_failure(params: &Value) -> Option<(String, String)> {
    if !params.get("models_count").is_some_and(Value::is_null) {
        return None;
    }
    let errors = params["errors"].as_array().map_or(&[][..], Vec::as_slice);
    let message =
        errors.iter().filter(|entry| entry["severity"] == "Error").find_map(|entry| entry["message"].as_str())?;
    let text = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let place = [" (in ", " --> "].iter().filter_map(|place| text.find(place)).min().unwrap_or(text.len());
    let key = text[..place].to_owned();
    Some((text, key))
}

fn key(id: &Value) -> String {
    id.to_string()
}

/// The `--project-dir` of COMMAND, as `--project-dir X` or
/// `--project-dir=X`, made absolute as spelt, so that the paths the proxy
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
    fn new(client: W, life: Life, root: Option<PathBuf>) -> Self {
        let mut proxy = Proxy {
            client,
            backend: Backend { life, ..Backend::default() },
            line: None,
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
            shutdown: false,
            markdown: false,
            hint_refresh: false,
            watches: false,
            asked: HashSet::new(),
            diagnostics: Diagnostics::default(),
            replay: Replay::default(),
            parsing: Parsing::default(),
            workspace: Workspace::default(),
            edits: HashMap::new(),
            edited: 0,
            changes: 0,
            shadow: None,
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

    fn send_client_body(&mut self, body: &[u8]) {
        let _ = rpc::write_body(&mut self.client, body);
    }

    fn send_server(&mut self, body: &[u8]) {
        if let Some(running) = self.backend.running_mut() {
            // A failed write means the server is gone; its reader reports it.
            let _ = rpc::write_body(&mut running.server.stdin, body);
        }
    }

    fn reply_error(&mut self, id: &Value, code: i64, message: &str) {
        self.send_client(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    /// Adds to CAPABILITIES each capability that the proxy adds and that it
    /// lacks, and owns its methods; announces and owns those in `plus`.
    fn add_capabilities(&mut self, capabilities: &mut serde_json::Map<String, Value>) {
        let lacks = |capabilities: &serde_json::Map<String, Value>, key: &str| {
            capabilities.get(key).is_none_or(|value| value.is_null() || *value == json!(false))
        };
        let announced =
            |method: &str| capability::of(method).and_then(|c| c.key).is_some_and(|key| !lacks(capabilities, key));
        self.table = modes::Table::new(&self.settings, &announced);
        // A method the proxy answers is announced; one left to the server, as the server announced it,
        // and with no backend, not at all.
        let added = CAPABILITIES.iter().filter(|c| c.added && self.table.mode(c.name) != Mode::Dbt);
        for (key, added) in added.filter_map(|c| c.key.map(|key| (key, c))) {
            if lacks(capabilities, key) {
                capabilities.insert(key.to_owned(), json!(true));
            }
            let followers = CAPABILITIES.iter().filter(|c| c.leader == Some(added.name)).map(|c| c.name);
            self.owned.extend(std::iter::once(added.name).chain(followers));
        }
        let unanswered = CAPABILITIES.iter().filter(|c| self.backend.alone() && self.table.mode(c.name) == Mode::Dbt);
        for key in unanswered.filter_map(|c| c.key) {
            capabilities.remove(key);
        }
        // The others in `plus`: the whole of a method the proxy answers whole, else its kinds of target.
        let provided = CAPABILITIES.iter().filter(|c| !c.added && self.table.mode(c.name) == Mode::Plus);
        for (key, provided) in provided.filter_map(|c| c.key.map(|key| (key, c))) {
            if lacks(capabilities, key) {
                capabilities.insert(key.to_owned(), json!(true));
            }
            self.owned.extend(provided.kinds.is_empty().then_some(provided.name));
        }
        // Which requests the proxy answers itself, project-wide, on which kinds
        // of target, beside the server's own experimental capabilities: a
        // client such as dbt-mode steps aside for exactly those.
        let experimental = capabilities.entry("experimental").or_insert(Value::Null);
        if experimental.is_null() {
            *experimental = json!({});
        }
        if let Some(experimental) = experimental.as_object_mut() {
            experimental.insert("dbtLspPlus".into(), self.table.flag());
        }
        // The proxy's own commands, beside the server's, which it may forward its commands for clients to.
        let provider = capabilities.entry("executeCommandProvider").or_insert_with(|| json!({}));
        let listed = provider.get("commands").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
        self.backend.commands = listed.map(str::to_owned).collect();
        let clients = commands::announced(&|dbt| self.backend.commands.contains(dbt));
        let plus = CAPABILITIES.iter().filter(|c| c.is_command() && c.providers == [Provider::Plus]).map(|c| c.name);
        let own = plus.chain(self.shadow.as_ref().map(|_| shadow::SUMMARY));
        if let Some(commands) = provider.as_object_mut().map(|provider| provider.entry("commands").or_insert(json!([])))
        {
            let own: Vec<Value> = own.chain(clients).map(|command| json!(command)).collect();
            commands.as_array_mut().into_iter().for_each(|commands| commands.extend(own.iter().cloned()));
        }
    }

    /// Says once, after `initialize` is answered, what of the user's settings
    /// cannot take effect: each refusal, the methods set to `dbt` that no
    /// backend answers, a shadow with no dbt lsp to compare with, or with
    /// nothing to follow; and a mode named by its former name.
    fn tell_settings(&mut self) {
        let mut said = std::mem::take(&mut self.refused);
        if !said.is_empty() {
            said.push(format!(
                "the methods that take a mode are {}",
                capability::with_mode().collect::<Vec<_>>().join(", ")
            ));
        }
        said.extend(self.settings.ours.then(|| "the mode ours is now named plus, and read so".to_owned()));
        let unanswered = self.table.unanswered(self.backend.alone());
        if !unanswered.is_empty() {
            said.push(format!("no dbt lsp answers {}, set to dbt: they get nothing", unanswered.join(", ")));
        }
        let off = self.settings.shadow && self.shadow.is_none();
        said.extend(off.then(|| "the shadow compares with dbt lsp, which does not run: it is off".to_owned()));
        let followed = ["textDocument/definition", "textDocument/references"].map(|method| self.table.mode(method));
        let idle = self.shadow.is_some() && !followed.contains(&Mode::Dbt);
        said.extend(idle.then(|| {
            "the shadow follows definitions and references in dbt only: with neither in dbt it has nothing to follow"
                .to_owned()
        }));
        if !said.is_empty() {
            let text = format!("dbt-lsp-plus: {}.", said.join("; "));
            self.send_client(&json!({"jsonrpc": "2.0", "method": "window/showMessage",
                                     "params": {"type": 2, "message": text}}));
        }
    }

    /// Answers `initialize` with no dbt lsp running, which leaves the
    /// session alone: the proxy's own features, and hovers and hints from
    /// the parse.
    fn initialize_alone(&mut self, id: &Value) {
        let mut capabilities = serde_json::Map::new();
        // Alone, the proxy still answers references and renames of sources,
        // and definitions on the targets of `navigation`: its kinds.
        let provided = CAPABILITIES.iter().filter(|c| !c.added).filter_map(|c| c.key.map(|key| (key, c)));
        for (key, _) in provided.filter(|(_, c)| !c.kinds.is_empty()) {
            capabilities.insert(key.into(), json!(true));
        }
        // Alone for good, it wants saves, which start a parse, and answers
        // hovers and hints from the parse alone: whole.
        capabilities.insert("textDocumentSync".into(), json!({"openClose": true, "change": 2, "save": {}}));
        for key in CAPABILITIES.iter().filter(|c| c.whole()).filter_map(|c| c.key) {
            capabilities.insert(key.into(), json!(true));
        }
        self.add_capabilities(&mut capabilities);
        self.watch_runs();
        let info = json!({"name": "dbt-lsp-plus", "version": env!("CARGO_PKG_VERSION")});
        self.send_client(
            &json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": capabilities, "serverInfo": info}}),
        );
        self.tell_settings();
        // Without a project yet, nothing is said: there may never be one.
        if self.workspace.root.is_none() {
            return;
        }
        // A dbt without `lsp` is no failure: said once, out of the user's way,
        // by the line of the dbt found, when one was looked for.
        let why = match &self.backend.life {
            Life::NotStarted(why) => {
                self.line.get_or_insert(format!("dbt-lsp-plus: {why}: dbt-lsp-plus answers alone."));
                return self.say_found();
            }
            Life::Lost(why) => why.clone(),
            _ => "dbt lsp is not running".to_owned(),
        };
        let text = format!("{why}: dbt-lsp-plus answers alone.");
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
    /// from the proxy's directory, its `dbt` becomes the project's, the
    /// server started unless alone, and the line that says so is written on
    /// the standard error and kept for the client.
    fn take_project(&mut self, root: PathBuf, document: bool) {
        let git = locate::Git { program: "git".into(), env: std::env::vars_os().collect() };
        // What the user wrote from the proxy's directory still names the same from the project.
        let server = std::env::current_dir().map(|cwd| config::anchored(self.workspace.dbt.command(), &cwd));
        let found = locate::dbt(&root, &server.unwrap_or_else(|_| self.workspace.dbt.command().to_vec()), &git);
        let no_lsp =
            || found.lsp.map_or_else(|| locate::probe(&found.command, Some(&root)) == Probe::NoLsp, |lsp| !lsp);
        let route = if document { locate::Route::Document } else { locate::Route::Folder };
        let alone = locate::alone(self.never, route, no_lsp);
        let skipped: String =
            found.skipped.iter().map(|(path, why)| format!("; skipped {} ({why})", path.display())).collect();
        let tail = alone.map_or_else(|| ".".to_owned(), |why| format!("; {why}: dbt-lsp-plus answers alone."));
        let (root_shown, named, source) = (root.display(), &found.named, found.source);
        let line = format!("dbt-lsp-plus: project {root_shown}, dbt {named} ({source}){skipped}{tail}");
        eprintln!("{line}");
        self.line = Some(line);
        self.workspace.dbt = config::Dbt::new(found.command, Some(root.clone()));
        // Without a channel, in the unit tests, in front with none started.
        self.backend.life = match (alone, self.parsing.events.clone()) {
            (Some(why), _) => Life::NotStarted(why.to_owned()),
            (None, Some(events)) => match spawn_server(self.workspace.dbt.command(), &events, 0, Some(&root)) {
                Ok(server) => Life::Running(Running::new(server)),
                Err(why) => Life::Lost(why),
            },
            (None, None) => Life::Lost("dbt lsp is not running".into()),
        };
        self.set_root(root);
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
        self.replay.initialize = Some(modes::without_section(params));
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
        match (folder, &self.workspace.root) {
            (Some(folder), None) => self.take_project(folder, false),
            (Some(folder), Some(_)) if same(&folder) => self.set_root(folder),
            _ => {}
        }
        self.workspace.warm();
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
        // Where the server has diagnostics, their ranges are of the text
        // before the edit, which the client has moved: its next publication
        // drops the failures instead.  An empty set has no range to move.
        if matches!(method, "textDocument/didChange" | "textDocument/didSave") {
            self.diagnostics.edited.insert(path.clone(), SystemTime::now());
            // A server gone publishes no more: its old set is no reason to wait.
            let server = self.diagnostics.server.get(&path).filter(|_| self.backend.running().is_some());
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
                self.replay.languages.insert(path.clone(), params["textDocument"]["languageId"].clone());
                self.replay.versions.insert(path.clone(), params["textDocument"]["version"].clone());
                self.uris.insert(path, uri.unwrap_or_default().to_owned());
            }
            "textDocument/didChange" => {
                self.replay.versions.insert(path.clone(), params["textDocument"]["version"].clone());
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
                self.replay.languages.remove(&path);
                self.replay.versions.remove(&path);
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

    /// The open documents as the Workspace reads them: their texts and the
    /// count of each one's edits.
    fn open_documents(&self) -> cache::Open<'_> {
        cache::Open { texts: &self.documents, edits: &self.edits }
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
    fn hierarchy(&self, method: &str, params: &Value) -> Option<Answer> {
        let Some(project) = self.project() else { return Some(Answer::Unanswered(vec![self.unreadable()])) };
        // Alone, the Workspace's graph takes the last parse's nodes.
        let listed = self.backend.running().and_then(|running| running.listings.nodes.as_deref());
        let server = (!self.backend.alone()).then_some((self.backend.nodes_generation, listed));
        let graph = self.workspace.graph(&project, server);
        if method != "textDocument/prepareCallHierarchy" {
            let id = params.pointer("/item/data/uniqueId")?.as_str()?;
            let incoming = method == "callHierarchy/incomingCalls";
            return Some(Answer::Found(features::calls(&project, &graph, id, incoming, self.encoding)));
        }
        // A document that is no file, such as an unsaved buffer, holds no resource.
        let Some(path) = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str)) else {
            return Some(Answer::Empty);
        };
        let text = project.text(&path).unwrap_or_default();
        let offset = LineStarts::new(text).offset_of(params.get("position")?, self.encoding)?;
        let prepared = features::prepare_call_hierarchy(&project, &graph, &path, offset, self.encoding);
        Some(prepared.map_or(Answer::Empty, Answer::Found))
    }

    /// The document of a request, its text, open or on disk, and the offset
    /// of the request's position in it.
    fn document_at(&self, params: &Value) -> Option<(PathBuf, String, usize)> {
        let (path, text) = self.text_of(params.pointer("/textDocument/uri")?.as_str()?)?;
        let offset = LineStarts::new(&text).offset_of(params.get("position")?, self.encoding)?;
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
    /// outside the resource paths, there is none, and the request goes to
    /// the server.
    fn source_at(&self, params: &Value) -> Option<(Rc<Project>, reference::Name)> {
        let (path, scanned, offset) = self.scan_at(params, None)?;
        let name = sources::name_at(&scanned, offset)?;
        let project = self.project().filter(|project| project.files.contains_key(&path))?;
        Some((project, name))
    }

    /// How the proxy answers the client's request METHOD: itself, as an owned
    /// method, its own commands, or a targeted request on one of its targets
    /// in its mode; with nothing, where the mode says so, and without a
    /// server for a definition and `shutdown`; else by forwarding it.
    fn route(&self, method: &str, params: &Value) -> Route {
        // Before a project, null to all but `initialize`: a valid result of
        // every method the proxy announces, and of each an agent was seen to
        // send.
        if self.workspace.root.is_none() && method != "initialize" {
            return Route::Answer(Answer::Found(Value::Null));
        }
        // Alone, hovers and hints are the proxy's, whole.
        let whole = || capability::of(method).filter(|c| self.backend.alone() && c.whole()).map(|c| c.name);
        if let Some(owned) = self.owned.get(method).copied().or_else(whole) {
            return Route::Here(owned);
        }
        let command = params["command"].as_str().filter(|_| method == "workspace/executeCommand");
        if let (Some(shadow::SUMMARY), Some(shadow)) = (command, &self.shadow) {
            return Route::Answer(Answer::Found(shadow.counts.summary()));
        }
        // A command of the proxy's alone is answered here, one of dbt's goes by `commands::route`.
        if let Some(own) = command.and_then(capability::of).filter(|own| own.is_command()) {
            return if own.providers == [Provider::Plus] { Route::Here(own.name) } else { Route::Command(own.name) };
        }
        // Where it unites, dbt lsp is asked at every position, and the proxy's part kept.
        if self.table.unites(method, self.backend.alone()) {
            return Route::Forward;
        }
        // In `dbt`, the proxy looks at nothing.
        let mine = self.table.mode(method) != Mode::Dbt;
        if let Some(targeted) = mine.then(|| self.targeted(method, params)).flatten() {
            return Route::Here(targeted);
        }
        // Without a server, a definition with no target is null, not a failure, and `shutdown` is null as LSP asks.
        let lifecycle = ["textDocument/definition", "shutdown"].contains(&method);
        let unserved = (lifecycle && self.backend.running().is_none()).then_some(Answer::Empty);
        let nothing = self.table.answers_nothing(method, self.backend.alone()).then_some(Answer::Empty);
        nothing.or(unserved).map_or(Route::Forward, Route::Answer)
    }

    /// A request of a capability with kinds of target that the proxy
    /// answers itself, by what its position holds: references and rename on
    /// a source, a rename on a CTE of a SQL file, or a definition on a
    /// target of `navigation`.
    fn targeted(&self, method: &str, params: &Value) -> Option<&'static str> {
        let method = capability::of(method).filter(|capability| !capability.kinds.is_empty())?.name;
        let (path, scanned, offset) = self.scan_at(params, None)?;
        let sql = path.extension().is_some_and(|extension| extension == "sql");
        let answered = match method {
            "textDocument/definition" => {
                navigation::reference_at(&path, &scanned, offset).is_some_and(|name| self.claims(method, &name, &path))
            }
            // A CTE lives in its file; a source only the scan sees whole, and so its references.
            "textDocument/rename" if sql && document::cte_at(&scanned.text, offset).is_some() => true,
            // References come from the scan, whichever file of the project asks, as agreed with dbt-mode.
            "textDocument/references" => {
                let claimed = navigation::reference_at(&path, &scanned, offset)
                    .is_some_and(|name| self.claims(method, &name, &path));
                claimed && self.workspace.root.as_ref().is_some_and(|root| path.starts_with(root))
            }
            _ => self.source_at(params).is_some(),
        };
        answered.then_some(method)
    }

    /// Whether the proxy answers METHOD itself on NAME in the file at
    /// PATH, as its capability's kinds say, when the table lists it.
    fn claims(&self, method: &str, name: &reference::Name, path: &Path) -> bool {
        let Some(place) = place(path) else { return false };
        let kinds = capability::of(method).map_or(&[][..], |capability| capability.kinds);
        let listed = |(kind, places): &&(&str, &[&str])| *kind == name.kind.flag() && places.contains(&place);
        self.table.lists(method) && kinds.iter().any(|kind| listed(&kind))
    }

    /// The definition of the target at the request's position, by dbt
    /// 2.0.9's lookup: its location, several, or null, found; none where
    /// the position names no target.
    fn on_definition(&self, params: &Value) -> Option<Answer> {
        let project = self.project();
        let (path, scanned, offset) = self.scan_at(params, project.as_deref())?;
        let Some(name) = navigation::reference_at(&path, &scanned, offset) else { return Some(Answer::Empty) };
        let Some(project) = project else { return Some(Answer::Unanswered(vec![self.unreadable()])) };
        let lookup = navigation::Lookup::new(&project, self.workspace.index.as_ref());
        let locations = self.locations(&project, &navigation::resolve(&lookup, &path, &name));
        Some(match &locations[..] {
            [] => Answer::Found(Value::Null),
            [location] => Answer::Found(location.clone()),
            _ => Answer::Found(Value::Array(locations)),
        })
    }

    /// The references of the source, the node or the macro at the request's
    /// position, with their declaration when the client asks.
    fn on_references(&self, params: &Value) -> Option<Answer> {
        let project = self.project();
        let (path, scanned, offset) = self.scan_at(params, project.as_deref())?;
        let Some(name) = navigation::reference_at(&path, &scanned, offset) else { return Some(Answer::Empty) };
        let Some(project) = project else { return Some(Answer::Unanswered(vec![self.unreadable()])) };
        let declaration = params.pointer("/context/includeDeclaration").and_then(Value::as_bool) == Some(true);
        if name.source_table().is_some() {
            let uri_of = |path: &Path| self.uri_of(path);
            return Some(Answer::Found(sources::references(&project, &name, declaration, self.encoding, &uri_of)));
        }
        let lookup = navigation::Lookup::new(&project, self.workspace.index.as_ref());
        let found = navigation::references(&lookup, &path, &name, declaration);
        Some(Answer::Found(Value::Array(self.locations(&project, &found))))
    }

    /// Why the proxy cannot answer from the project: the root's
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
            let range = starts.range(found.range.start, found.range.end, self.encoding);
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
        Some(Answer::of_rename(sources::rename(&project, &name, new, self.encoding, &uri_of)))
    }

    /// The rename of the source, else of the CTE, at the request's position.
    fn on_rename(&self, params: &Value) -> Option<Answer> {
        if self.source_at(params).is_some() {
            return self.on_source(params);
        }
        let (_, text, offset) = self.document_at(params)?;
        let (uri, new) = (params.pointer("/textDocument/uri")?.as_str()?, params.get("newName")?.as_str()?);
        Some(document::rename_cte(&text, uri, offset, new, self.encoding).map_or(Answer::Empty, Answer::of_rename))
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
        let starts = LineStarts::new(&text);
        let positions = params.get("positions")?.as_array()?.iter().map(|p| starts.offset_of(p, self.encoding));
        Some(document::selection_ranges(&text, sql, &positions.collect::<Option<Vec<usize>>>()?, self.encoding))
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
    /// parse: with no server running, or with one that compiled since the
    /// last change; no shutdown.  Its outcome comes back
    /// as an event; why none could start is logged.
    fn want_index(&mut self) {
        // After `shutdown`, neither a listing nor a parse: the session ends.
        if self.shutdown {
            return;
        }
        // A listing makes dbt lsp compile, as the first document opened does.
        let listing = self.backend.running().is_some_and(|running| running.listings.listing.is_some());
        if !self.parsing.compiled && !listing {
            self.list_nodes();
        }
        self.parsing.wanted |= !self.parsing.compiled;
        // With no server, none resolves the packages first: their guard is enough.
        if self.backend.running().is_some() && !self.parsing.compiled {
            return;
        }
        let events = self.parsing.events.clone();
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

    /// A request of one of the proxy's commands for clients, COMMAND with
    /// PARAMS, kept as PENDING: sent to dbt lsp as its own command, queued
    /// for dbt's CLI, answered here, or refused, as `commands::route`
    /// decides.
    fn on_command(&mut self, command: &'static str, id: &Value, params: &Value, pending: Pending) {
        if self.shutdown {
            return self.reply_error(id, INVALID_REQUEST, "the client asked for shutdown");
        }
        let Some(arguments) = commands::arguments(command, params) else {
            return self.reply_error(id, INVALID_PARAMS, "invalid parameters");
        };
        let serves = |dbt: &str| self.backend.running().is_some() && self.backend.commands.contains(dbt);
        match commands::route(command, &serves) {
            Ok(Provider::DbtLsp) => {
                let dbt = capability::of(command).and_then(|own| own.server);
                let params = json!({"command": dbt, "arguments": arguments});
                let request =
                    json!({"jsonrpc": "2.0", "id": id, "method": "workspace/executeCommand", "params": params});
                self.forwarded.insert(key(id), (id.clone(), pending));
                self.send_server(&serde_json::to_vec(&request).unwrap_or_default());
            }
            Ok(Provider::DbtCli) => self.run_command(command, id, &arguments[0]),
            Ok(Provider::Plus) => self.answer(command, id, params, &pending),
            Err(failure) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": failure})),
        }
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
        let (Some(events), Some((id, job, run))) = (self.parsing.events.clone(), self.runs.pop_front()) else {
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
        // Alone, a change during this parse wants the next one now: no
        // request may come to ask for it.
        if self.backend.alone() {
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
        let (true, Some(problems)) = (self.backend.alone(), problems) else { return };
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
    /// client's folder or a document named it, else the proxy's.
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

    /// Stops a run under way, then the Workspace's parse, which removes this
    /// process's cache.
    fn stop_parse(&mut self) {
        if let Some(run) = self.run.take() {
            run.stop();
        }
        self.workspace.stop();
    }

    /// Answers a request that the proxy answers itself, which it remembers as
    /// PENDING, with `plus_answer`'s Answer, after asking for the index where
    /// that answer reads it, so that a reason is that of the parse it may
    /// start: as its capability says, and for a hover whose part names a node
    /// before any index.  A refusal fails it with its reason; a panic while
    /// answering fails that request alone, never the session.
    fn answer(&mut self, method: &str, id: &Value, params: &Value, pending: &Pending) {
        let alone = self.backend.alone();
        let reads = capability::of(method).is_some_and(|c| c.wants_index || (alone && c.reads_graph));
        if reads || pending.kept == Kept::Part(Part::Unindexed) {
            self.ask_index();
        }
        let computed = panic::catch_unwind(AssertUnwindSafe(|| self.plus_answer(method, params, pending)));
        match computed {
            Ok(Some(answer)) => self.send_client(&answer.frame(id, method)),
            Ok(None) => self.reply_error(id, INVALID_PARAMS, "invalid parameters"),
            Err(_) => self.reply_error(id, INTERNAL_ERROR, "dbt-lsp-plus failed to answer this request"),
        }
    }

    /// The Answer of the `plus` provider, the proxy itself, to METHOD at
    /// PARAMS, which it remembers as PENDING, by the method's handler: for a
    /// hover or inlay hints, the part kept, alone; none for invalid
    /// parameters.  It sends nothing.
    fn plus_answer(&self, method: &str, params: &Value, pending: &Pending) -> Option<Answer> {
        match method {
            "textDocument/documentSymbol" => self.outline(params).map(Answer::Found),
            "workspace/symbol" => self.symbols(params),
            config::NODE_INFO => self.node_info(params),
            "textDocument/references" => self.on_references(params),
            "textDocument/definition" => self.on_definition(params),
            "textDocument/rename" => self.on_rename(params),
            "textDocument/foldingRange" | "textDocument/documentHighlight" | "textDocument/selectionRange" => {
                self.on_document(method, params).map(Answer::Found)
            }
            // As dbt lsp 2.0.9's answer with nothing of its own gets them,
            // null for a hover, no hint: the proxy's part, else that; why
            // the proxy could not answer, as over a project that does not
            // read; for a hover on a node before any index, why there is none yet.
            "textDocument/hover" | "textDocument/inlayHint" => {
                Some(pending.unserved(self.markdown, None, &self.workspace))
            }
            _ => self.hierarchy(method, params),
        }
    }

    /// The workspace's symbols that match the request's query; why none
    /// over a project that does not read.
    fn symbols(&self, params: &Value) -> Option<Answer> {
        let query = params.get("query")?.as_str()?;
        let Some(project) = self.project() else { return Some(Answer::Unanswered(vec![self.unreadable()])) };
        Some(Answer::Found(features::workspace_symbols(&project, query, self.encoding)))
    }

    fn client_listing(&self) -> bool {
        self.forwarded.values().any(|(_, pending)| pending.listing())
    }

    /// Asks the server for the graph, unless a listing is under way, the
    /// proxy's own or the client's, or there is no server.
    fn list_nodes(&mut self) {
        let client = self.client_listing();
        let Some(running) = self.backend.running_mut() else { return };
        if running.listings.listing.is_some() || client {
            running.listings.relist = true;
        } else {
            running.send_list(0, &mut self.next_id, &self.forwarded, self.shutdown);
        }
    }

    /// Lists the graph if a change came while it could not be.
    fn resume(&mut self) {
        if self.backend.running_mut().is_some_and(|running| std::mem::take(&mut running.listings.relist)) {
            self.list_nodes();
        }
    }

    /// Abandons a listing unanswered for LIST_TIMEOUT; the graph stays.
    fn expire_listing(&mut self) {
        let Some(listings) = self.backend.running_mut().map(|running| &mut running.listings) else { return };
        if listings.listing.as_ref().is_some_and(|(_, _, sent)| sent.elapsed() >= LIST_TIMEOUT) {
            listings.listing = None;
            listings.listed.clear();
            listings.attempts = 0;
            self.resume();
        }
    }

    /// Takes the server's answer to the proxy's listing in flight; the answer
    /// to an abandoned one is dropped.  A failed listing keeps the graph.
    fn on_own_response(&mut self, id: &str, message: &Value) {
        if matches!(&self.backend.life, Life::Starting(_, own, _) if own == id) {
            return self.on_restarted(message);
        }
        let client = self.client_listing();
        // The process's listings beside the Backend's count of their nodes.
        let Life::Running(running) = &mut self.backend.life else { return };
        let listings = &mut running.listings;
        let Some((_, slot, _)) = listings.listing.take_if(|(own, ..)| own == id) else { return };
        let result = message.get("result").unwrap_or(&Value::Null);
        if result.get("error") == Some(&json!("Operation cancelled")) && listings.attempts < MAX_ATTEMPTS {
            listings.attempts += 1;
            listings.listed.clear();
            listings.relist = true;
            return self.resume();
        }
        // `*` lists nothing only when the project failed to compile: the scan found resources.
        let listed = graph::parse_list_nodes(result).into_nodes().filter(|nodes| slot == 1 || !nodes.is_empty());
        match (slot, listed) {
            (0, Some(nodes)) if !client => {
                listings.listed = nodes;
                return running.send_list(1, &mut self.next_id, &self.forwarded, self.shutdown);
            }
            // The client's own listing would cancel the second selector: start again after it.
            (0, Some(_)) => listings.relist = true,
            (_, Some(sources)) => {
                let mut nodes = std::mem::take(&mut listings.listed);
                nodes.extend(sources);
                listings.nodes = Some(nodes);
                self.backend.nodes_generation += 1;
            }
            (_, None) => listings.listed.clear(),
        }
        listings.attempts = 0;
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
        if method == "textDocument/didOpen" && self.workspace.root.is_none() && self.initialized && !self.shutdown {
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
        if method == "initialized" && self.backend.alone() && self.workspace.root.is_some() {
            self.register_watcher();
        }
        // The first document opened in the project.
        let first_open = method == "textDocument/didOpen"
            && self.workspace.root.is_some()
            && !std::mem::replace(&mut self.opened, true);
        // dbt lsp's output, as spelt and as resolved: a link out of the project included.
        let output = || {
            let spelt = self.workspace.root.iter().flat_map(|root| OUTPUT.map(|dir| root.join(dir)));
            spelt.flat_map(|dir| [fs::canonicalize(&dir).ok(), Some(dir)]).flatten().collect()
        };
        if changes_graph(method, params, output, |uri| self.local_path(uri)) || first_open {
            self.workspace.fresh = false;
            self.parsing.compiled = false;
            self.list_nodes();
            if self.backend.alone() {
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
        let Some(root) = path.and_then(|path| locate::project_of(&path, self.workspace.dbt.command())) else { return };
        self.take_project(root, true);
        self.workspace.warm();
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

    /// What the answer to a hover at PARAMS gets first, outside `dbt`: the
    /// summary and facts of the node named there, else a doc's or a generic
    /// test's block; else nothing, null, which a node named before any index
    /// turns into why the proxy has none, and a node named by a `ref`, a
    /// `source` or an entry's `name:` over a project that does not read,
    /// once there is an index, into its reason.
    fn hover_part(&self, params: &Value) -> Part {
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
            (Some(_), None) => Part::Unindexed,
            (Some(None), Some(_)) if self.project().is_none() => Part::Unanswered(vec![self.unreadable()]),
            _ => Part::Nothing,
        };
        summary.or_else(block).map_or(nothing, |(text, hard)| Part::Hover(text, hard))
    }

    /// What a definition or references at PARAMS gets first when the proxy
    /// unites dbt lsp's answer with its own: where it claims the position,
    /// its locations, its empty, or why it cannot answer, as over a project
    /// that does not read; nothing where it panics; none where it claims
    /// nothing.
    fn union_part(&self, method: &str, params: &Value) -> Option<Part> {
        let claimed = self.targeted(method, params)?;
        let found = panic::catch_unwind(AssertUnwindSafe(|| match claimed {
            "textDocument/references" => self.on_references(params),
            _ => self.on_definition(params),
        }));
        Some(match found.ok().flatten() {
            Some(Answer::Found(found)) if !found.is_null() && found != json!([]) => Part::Locations(found),
            Some(Answer::Found(_) | Answer::Empty) => Part::Empty,
            Some(Answer::Unanswered(reasons)) => Part::Unanswered(reasons),
            None => Part::Nothing,
        })
    }

    /// What the proxy keeps of the client's request METHOD, and the lifecycle
    /// it starts: the client's `initialize` and `shutdown`, a listing that the
    /// proxy must not cancel, the summary that a hover's answer gets, the
    /// locations that a united one's gets, a request that the shadow
    /// follows, the nothing that a merged request falls back to; and
    /// whether, once routed, it asks for a fresh index: a hover keeps the
    /// index fresh, an inlay hint only makes one, and in `dbt` neither; a
    /// united request at a position the proxy claims wants what its answer
    /// reads, and a shadowed one what `plus` reads.
    fn remember(&mut self, method: &str, id: &Value, params: &Value) -> (Pending, bool) {
        let command = params["command"].as_str().filter(|_| method == "workspace/executeCommand");
        let name = command.unwrap_or(method).to_owned();
        // Outside `dbt`, hints with none of the proxy's, and a definition or references it does not unite: its empty.
        let empty = Kept::Part(Part::Empty);
        let (kept, fresh) = match method {
            "initialize" => {
                self.on_initialize(params);
                self.initialized = true;
                self.initialize = Some(id.clone());
                (Kept::Plain, false)
            }
            "shutdown" => {
                self.shutdown = true;
                (Kept::Plain, false)
            }
            "textDocument/hover" if self.table.mode(method) != Mode::Dbt => (Kept::Part(self.hover_part(params)), true),
            "textDocument/inlayHint" if self.table.mode(method) != Mode::Dbt => {
                // A panic leaves the server's answer as it is.
                let hints = panic::catch_unwind(AssertUnwindSafe(|| {
                    let index = self.workspace.index.as_ref()?;
                    let project = self.project()?;
                    let (path, text) = self.text_of(params.pointer("/textDocument/uri")?.as_str()?)?;
                    let lines = LineStarts::new(&text);
                    let start = lines.offset_of(params.pointer("/range/start")?, self.encoding)?;
                    let end = lines.offset_of(params.pointer("/range/end")?, self.encoding)?;
                    let calls = sources::file_calls(&path, &text);
                    let lookup = navigation::Lookup::new(&project, Some(index));
                    Some(features::inlay_hints(&text, calls, &lookup, &path, start..end, self.encoding))
                }));
                let hints = hints.ok().flatten().filter(|hints| !hints.is_empty());
                (hints.map_or(empty, |hints| Kept::Part(Part::Hints(hints))), self.workspace.index.is_none())
            }
            "textDocument/definition" | "textDocument/references"
                if self.table.unites(method, self.backend.alone()) =>
            {
                let part = self.union_part(method, params);
                let fresh = part.is_some() && capability::of(method).is_some_and(|capability| capability.wants_index);
                (Kept::Part(part.unwrap_or(Part::Nothing)), fresh)
            }
            // Followed in `dbt` only: one the proxy answers or unites is no one's to compare.
            "textDocument/definition" | "textDocument/references" => {
                let (changes, dbt) = (self.changes, self.table.mode(method) == Mode::Dbt);
                let shadowed = self.shadow.as_ref().filter(|_| dbt).map(|_| Kept::Shadow(params.clone(), changes));
                let fresh = shadowed.is_some();
                (shadowed.unwrap_or(if dbt { Kept::Plain } else { empty }), fresh)
            }
            _ => (Kept::Plain, false),
        };
        (Pending { name, kept }, fresh)
    }

    /// A request of the client: refused under the id of the proxy's listing
    /// in flight; `on_elsewhere`'s on a document of another project; else
    /// remembered, and handled where `route` sends it: `answer`, its answer
    /// sent, `on_command`, or `forward`, which asks for the index itself;
    /// then the index, when it asks for a fresh one.
    fn on_request(&mut self, method: &str, id: &Value, params: &Value, body: &[u8]) {
        // The server would get two requests under one id.
        let listing = self.backend.running().and_then(|running| running.listings.listing.as_ref());
        if listing.is_some_and(|(own, ..)| id.as_str() == Some(own)) {
            return self.reply_error(id, INVALID_REQUEST, "dbt-lsp-plus has a request in flight under this id");
        }
        if let Some(why) = self.outside(method, params) {
            return self.on_elsewhere(method, id, body, why);
        }
        let (pending, fresh) = self.remember(method, id, params);
        match self.route(method, params) {
            Route::Here(owned) => self.answer(owned, id, params, &pending),
            Route::Answer(answer) => self.send_client(&answer.frame(id, method)),
            Route::Command(command) => self.on_command(command, id, params, pending),
            Route::Forward => return self.forward(id, method, pending, body, fresh),
        }
        if fresh {
            self.ask_index();
        }
    }

    /// Forwards the client's request METHOD of ID, its BODY as it came, and
    /// keeps PENDING for dbt lsp's answer, which adds to the proxy's part,
    /// or replaces its nothing, but never holds it up; then, when FRESH,
    /// asks for the index, once the request is forwarded, so that a listing
    /// takes no id of the client's.  With no dbt lsp running, nothing is
    /// forwarded: `initialize` is answered alone, and any other request gets
    /// what `Pending::unserved` gives for why none runs, for good or for
    /// now, the index asked for first when FRESH, so that a reason is that
    /// of the parse it may start.
    fn forward(&mut self, id: &Value, method: &str, pending: Pending, body: &[u8], fresh: bool) {
        if self.backend.running().is_none() {
            if method == "initialize" {
                self.initialize = None;
                return self.initialize_alone(id);
            }
            if fresh {
                self.ask_index();
            }
            let why =
                if let Life::Lost(why) = &self.backend.life { Reason::Lost(why.clone()) } else { Reason::NotRunning };
            let answer = pending.unserved(self.markdown, Some(why), &self.workspace);
            return self.send_client(&answer.frame(id, &pending.name));
        }
        let late = pending.deadline().map(|deadline| (Instant::now() + deadline, method.to_owned()));
        self.deadlines.extend(late.map(|late| (key(id), late)));
        self.forwarded.insert(key(id), (id.clone(), pending));
        self.send_server(body);
        if fresh {
            self.ask_index();
        }
    }

    /// Why the request METHOD at PARAMS is not the session's project's: its
    /// document, named by `textDocument.uri`, lies in another project
    /// (`locate::elsewhere`), for a method that reads the project; none
    /// before a project, in `dbt`, where the request goes to dbt lsp as it
    /// came, and for any other request.
    fn outside(&self, method: &str, params: &Value) -> Option<Reason> {
        let root = self.workspace.root.as_ref()?;
        let reads = capability::of(method).is_some_and(|capability| capability.reads_project);
        if !reads || self.table.mode(method) == Mode::Dbt {
            return None;
        }
        let document = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str))?;
        let project = locate::elsewhere(&document, root, self.workspace.dbt.command())?;
        Some(Reason::Elsewhere { document, project, served: root.clone() })
    }

    /// Answers the request METHOD of ID, its BODY as it came, on a document
    /// of another project, for WHY, computing nothing on it: in `merge` in
    /// front of dbt lsp, a method the proxy does not own is forwarded, whose
    /// answer stands, and when it gives none, WHY comes first; else WHY at
    /// once.  Inlay hints, a decoration, get none of the proxy's.
    fn on_elsewhere(&mut self, method: &str, id: &Value, body: &[u8], why: Reason) {
        let hints = method == "textDocument/inlayHint";
        let merged = self.table.mode(method) == Mode::Merge && !self.owned.contains(method);
        if merged && self.backend.running().is_some() {
            let part = if hints { Part::Empty } else { Part::Unanswered(vec![why]) };
            return self.forward(id, method, Pending { name: method.to_owned(), kept: Kept::Part(part) }, body, false);
        }
        let answer = if hints { Answer::Empty } else { Answer::Unanswered(vec![why]) };
        self.send_client(&answer.frame(id, method));
    }

    /// A message of the server.  A response is the proxy's own only when its
    /// id has the proxy's prefix and answers no request of the client's.
    fn on_server(&mut self, frame: Frame) {
        let Some(message) = &frame.value else { return self.send_client_body(&frame.body) };
        match (message.get("method").and_then(Value::as_str), message.get("id")) {
            (None, Some(id)) => self.on_server_response(id, message, &frame.body),
            // Two compilations that overlap are timed from the last start.
            (Some("dbt/lspCompileStart" | "dbt/lspBackgroundCompileStart"), _) => {
                // A process starting again arms none: its frames pass `handle`'s filter too.
                if let Some(running) = self.backend.running_mut() {
                    running.compiling = Compiling { since: Some(Instant::now()), told: false };
                }
                self.send_client_body(&frame.body);
            }
            (Some(method @ ("dbt/lspCompileComplete" | "dbt/lspBackgroundCompileComplete")), _) => {
                self.on_compiled(method == "dbt/lspCompileComplete", message, &frame.body)
            }
            (Some("textDocument/publishDiagnostics"), None) => self.on_server_diagnostics(message, &frame.body),
            (Some("client/registerCapability"), Some(id)) => self.on_registration(id, message, &frame.body),
            (Some("$/progress"), None) => self.on_progress(message, &frame.body),
            _ => self.send_client_body(&frame.body),
        }
    }

    /// The completion of a compilation of the server's, MESSAGE, BODY as it
    /// came, forwarded, of the FOREGROUND one or the background one: the
    /// compilation's deadline ends, and one that compiled nothing is said
    /// once for each first error, across restarts, but after `shutdown`.
    fn on_compiled(&mut self, foreground: bool, message: &Value, body: &[u8]) {
        // The server has resolved the packages, from the disk as it is: dbt
        // may parse, for a request that wanted an index before.
        if foreground {
            self.parsing.compiled = true;
            if std::mem::take(&mut self.parsing.wanted) {
                self.want_index();
            }
        }
        // A process starting again has no timer to end: its frames pass `handle`'s filter too.
        if let Some(running) = self.backend.running_mut() {
            running.compiling.since = None;
        }
        self.send_client_body(body);
        let Some((text, key)) = compile_failure(&message["params"]) else { return };
        if self.shutdown || !self.backend.said.insert(key) {
            return;
        }
        let text = format!("dbt lsp compiled nothing: {text} (its log: {})", self.server_log().display());
        self.send_client(&json!({"jsonrpc": "2.0", "method": "window/showMessage",
                                 "params": {"type": 1, "message": text}}));
    }

    /// A compilation past its deadline, which the caller marks `told`: said
    /// once a session, with dbt lsp's log, but after `shutdown`.
    fn tell_stalled(&mut self) {
        if self.backend.stalled || self.shutdown {
            return;
        }
        self.backend.stalled = true;
        let (stall, log) = (COMPILE_STALL.as_secs(), self.server_log());
        let text = format!("dbt lsp has been compiling for {stall} s with no end yet (its log: {})", log.display());
        self.send_client(&json!({"jsonrpc": "2.0", "method": "window/showMessage",
                                 "params": {"type": 2, "message": text}}));
    }

    /// dbt lsp's log, which 2.0.9 writes under its project whatever its
    /// `--log-path` says.
    fn server_log(&self) -> PathBuf {
        self.workspace.root.as_deref().unwrap_or(Path::new("")).join("logs/dbt-lsp.log")
    }

    /// The server's `$/progress`, BODY as it came, forwarded: the tokens it
    /// begins and has not ended are kept, since a server that dies ends none.
    fn on_progress(&mut self, message: &Value, body: &[u8]) {
        let (token, kind) = (&message["params"]["token"], message.pointer("/params/value/kind"));
        match kind.and_then(Value::as_str) {
            Some("begin") => drop(self.backend.progress.insert(key(token), token.clone())),
            Some("end") => drop(self.backend.progress.remove(&key(token))),
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
        if !ids.is_empty() && ids.iter().all(|id| self.backend.registered.contains(id)) {
            let answer = json!({"jsonrpc": "2.0", "id": id, "result": null}).to_string();
            return self.send_server(answer.as_bytes());
        }
        self.backend.registered.extend(ids);
        self.send_client_body(body);
    }

    /// A response of the server, BODY as it came: to a request of the
    /// client's, forwarded as its pending kind makes it, or to one of the
    /// proxy's own, kept.
    fn on_server_response(&mut self, id: &Value, message: &Value, body: &[u8]) {
        let pending = self.forwarded.remove(&key(id));
        self.deadlines.remove(&key(id));
        // The client had the proxy's part alone: the late answer is no one's.
        if pending.as_ref().is_some_and(|(_, pending)| pending.kept == Kept::Answered) {
            return;
        }
        if let (None, Some(own)) = (&pending, id.as_str())
            && own.starts_with(OWN_ID)
        {
            return self.on_own_response(own, message);
        }
        if pending.as_ref().is_some_and(|(_, pending)| pending.listing()) {
            self.resume();
        }
        let path_of = |uri: &str| self.path_of(uri);
        let merged =
            pending.as_ref().and_then(|(_, kind)| kind.merged(message, self.markdown, &self.workspace, &path_of));
        if let Some(merged) = merged {
            return self.send_client(&merged);
        }
        if self.initialize.as_ref() == Some(id) {
            self.initialize = None;
            let mut message = message.clone();
            let encoding = message.pointer("/result/capabilities/positionEncoding").and_then(Value::as_str);
            self.encoding = Encoding::parse(encoding);
            // The shadow follows the server that answered; its log is under the user's cache, if any.
            self.shadow = self.settings.shadow.then(|| {
                let log = config::cache_base()
                    .zip(self.workspace.root.as_ref())
                    .map(|(base, root)| shadow::Log::open(&base, root));
                shadow::Shadow::new(log)
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
        if let Some((_, Pending { name, kept: Kept::Shadow(params, changes) })) = pending {
            self.compare(&name, &params, changes, message);
        }
    }

    /// Compares dbt lsp's answer MESSAGE, which the client already has, to a
    /// request that the shadow follows, METHOD with PARAMS after CHANGES
    /// changes, with what the proxy answers there whatever the flag lists,
    /// but for references outside the root, which it never answers; then
    /// counts it and logs it.  A panic costs the comparison alone.
    fn compare(&mut self, method: &str, params: &Value, changes: u64, message: &Value) {
        let record = panic::catch_unwind(AssertUnwindSafe(|| {
            let root = self.workspace.root.as_deref().unwrap_or(Path::new(""));
            let path_of = |uri: &str| self.path_of(uri);
            let file = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str));
            let inside = file.as_deref().is_some_and(|file| file.starts_with(root));
            let compared = match shadow::read(message, self.changes != changes, &path_of, &|path| path.is_file()) {
                shadow::Response::Apart(relation) => {
                    shadow::Compared { relation, ours_only: Vec::new(), dbt_only: Vec::new(), both: 0 }
                }
                shadow::Response::Locations(dbt) => {
                    let ours = match method {
                        "textDocument/references" if inside => self.on_references(params),
                        "textDocument/references" => None,
                        _ => self.on_definition(params),
                    };
                    // The proxy's nothing and its failure hold no location, as null.
                    let ours = match ours {
                        Some(Answer::Found(found)) => found,
                        _ => Value::Null,
                    };
                    shadow::compare(&shadow::locations(&ours, &path_of), &dbt)
                }
            };
            let project = self.project();
            let read = self.scan_at(params, project.as_deref());
            let name = read.and_then(|(path, scanned, offset)| navigation::reference_at(&path, &scanned, offset));
            let relative = file.as_deref().and_then(|file| file.strip_prefix(root).ok());
            let known = self.workspace.index.as_ref().zip(project).zip(relative.zip(name.as_ref()));
            let oracle = known.and_then(|((index, project), (relative, name))| {
                shadow::oracle(index, &project, &relative.to_string_lossy(), name)
            });
            let workspace = &self.workspace;
            let index = json!({"present": workspace.index.is_some(), "fresh": workspace.fresh,
                               "running": workspace.running.is_some()});
            shadow::Record {
                method: method.trim_start_matches("textDocument/").to_owned(),
                kind: name.as_ref().map_or("none", |name| name.kind.flag()),
                place: file.as_deref().and_then(place).unwrap_or("other"),
                target: name.as_ref().map_or(Value::Null, shadow::target),
                file,
                compared,
                index,
                oracle,
            }
        }));
        let Ok(record) = record else { return };
        let line = record.line(self.workspace.root.as_deref().unwrap_or(Path::new("")));
        if let Some(shadow) = &mut self.shadow {
            shadow.counts.add(&record);
        }
        self.shadow_line(&line);
    }

    /// The path of URI as both answers of a comparison or a union read it:
    /// under the root as the client spells it, resolved elsewhere, where
    /// each side may spell a file its own way.
    fn path_of(&self, uri: &str) -> Option<PathBuf> {
        let root = self.workspace.root.as_deref().unwrap_or(Path::new(""));
        let path = self.local_path(Some(uri))?;
        Some(if path.starts_with(root) { path } else { fs::canonicalize(&path).unwrap_or(path) })
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
        let (Some(root), Some(events)) = (self.workspace.root.clone(), self.parsing.events.clone()) else { return };
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
        let lines = LineStarts::new(&text);
        let (entries, tests) = (properties::entries(&text), properties::tests(&text));
        diagnostics.extend(failures.iter().map(|failure| {
            let range = runs::locate(&text, &entries, &tests, &failure.locator);
            json!({"range": lines.range(range.start, range.end, self.encoding), "severity": failure.severity,
                   "source": "dbt-lsp-plus", "code": failure.code, "message": failure.message})
        }));
        // At dbt's line and column, from 1 and in code points; a file's start
        // when dbt gives none.
        let problems = self.diagnostics.parsed.get(path).map_or(&[][..], Vec::as_slice);
        diagnostics.extend(problems.iter().map(|problem| {
            let (line, column) = problem.spot.as_ref().and_then(|spot| spot.at).unwrap_or((1, 1));
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

    /// The server is gone: answer the client's `initialize`, alone, if it
    /// is pending; else restart it or lose it for good (`lost_server`);
    /// then what it left unanswered, once each, so that a reason is that of
    /// the parse a give-up starts.  Before the client's `initialize`, only
    /// remember why, for its answer.
    fn on_server_closed(&mut self) {
        let server = match std::mem::take(&mut self.backend.life) {
            Life::Running(Running { server, .. }) | Life::Starting(server, ..) => server,
            other => {
                self.backend.life = other;
                return;
            }
        };
        let status = server.reap(Duration::ZERO);
        let exited = Reason::Exited(status.map_or("unknown status".into(), |s| s.to_string()));
        let why = exited.to_string();
        self.backend.life = Life::Lost(why.clone());
        if let Some(id) = &self.initialize {
            self.forwarded.remove(&key(id));
        }
        self.deadlines.clear();
        let left = std::mem::take(&mut self.forwarded);
        self.backend.nodes_generation += 1;
        if let Some(id) = self.initialize.take() {
            self.initialize_alone(&id);
        } else {
            self.lost_server(&why);
        }
        for (_, (id, pending)) in left {
            match (pending.name.as_str(), &pending.kept) {
                ("shutdown", _) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": null})),
                (_, Kept::Answered) => {}
                _ => {
                    let answer = pending.unserved(self.markdown, Some(exited.clone()), &self.workspace);
                    self.send_client(&answer.frame(&id, &pending.name))
                }
            }
        }
    }

    /// The server lost for WHY: during a session, the progress it began is
    /// ended, and a restart follows after its delay, else the proxy is alone
    /// from now on; the client is told which.  Lost for good, its last
    /// diagnostics go and the project is parsed again.  Before the client's
    /// `initialize` or after `shutdown`, lost for good, and nothing said.
    fn lost_server(&mut self, why: &str) {
        self.backend.life = Life::Lost(why.to_owned());
        if !self.initialized || self.shutdown {
            return;
        }
        for (_, token) in std::mem::take(&mut self.backend.progress) {
            self.send_client(&json!({"jsonrpc": "2.0", "method": "$/progress",
                                     "params": {"token": token, "value": {"kind": "end"}}}));
        }
        let now = Instant::now();
        self.backend.restarts.retain(|at| now.saturating_duration_since(*at) < RESTART_WINDOW);
        let delay = restart_delay(&self.backend.restarts, now);
        let text = match delay {
            Some(delay) => format!("{why}: restarting it in {} s.", delay.as_secs()),
            None => format!("{why}: dbt-lsp-plus answers alone."),
        };
        self.send_client(
            &json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 1, "message": text}}),
        );
        let Some(delay) = delay else {
            // None will publish dbt lsp's last diagnostics again, and the parse's would double them.
            let mut published: Vec<PathBuf> = std::mem::take(&mut self.diagnostics.server).into_keys().collect();
            published.sort();
            published.iter().for_each(|path| self.publish(path));
            // As after a change alone, with no request: the problems of a parse made in front were never kept.
            // A parse under way, or whose end is queued behind this, is of the project as it is: no second one.
            self.workspace.fresh &= self.workspace.running.is_some();
            return self.want_index();
        };
        self.backend.life = Life::Waiting(now + delay);
    }

    /// Starts the server again with the command of the session, and sends it
    /// the client's `initialize` under an id of the proxy's own; a failure to
    /// start it is a server lost again.  After `shutdown`, or where none can
    /// start (no client's parameters, no channel in the unit tests), lost
    /// for good, so that its due never wakes the loop again.
    fn start_restart(&mut self) {
        let (Some(params), Some(events)) = (self.replay.initialize.clone(), self.parsing.events.clone()) else {
            self.backend.life = Life::Lost("dbt lsp is not running".into());
            return;
        };
        if self.shutdown {
            self.backend.life = Life::Lost("the client asked for shutdown".into());
            return;
        }
        self.backend.restarts.push(Instant::now());
        self.backend.spawned += 1;
        match spawn_server(self.workspace.dbt.command(), &events, self.backend.spawned, self.workspace.dbt.dir()) {
            Ok(mut server) => {
                self.next_id += 1;
                let id = format!("{OWN_ID}{}", self.next_id);
                let initialize = json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params});
                let _ = rpc::write_value(&mut server.stdin, &initialize);
                self.backend.life = Life::Starting(server, id, Instant::now() + REPLAY_TIMEOUT);
            }
            Err(why) => self.lost_server(&why),
        }
    }

    /// The restarted server's answer to its `initialize`, MESSAGE: it becomes
    /// the server, initialized, given the client's open documents as they
    /// stand, and listed; refused, or after `shutdown`, it is stopped.
    fn on_restarted(&mut self, message: &Value) {
        let server = match std::mem::take(&mut self.backend.life) {
            Life::Starting(server, ..) => server,
            other => {
                self.backend.life = other;
                return;
            }
        };
        if message.get("error").is_some() || self.shutdown {
            server.reap(Duration::ZERO);
            return self.lost_server("dbt lsp refused its initialize");
        }
        self.backend.life = Life::Running(Running::new(server));
        self.send_server(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}).to_string().as_bytes());
        let mut documents: Vec<(&PathBuf, &String)> = self.documents.iter().collect();
        documents.sort();
        let opened: Vec<Value> = documents
            .into_iter()
            .map(|(path, text)| {
                let document = json!({"uri": self.uris.get(path), "languageId": self.replay.languages.get(path),
                                      "version": self.replay.versions.get(path), "text": text});
                json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": document}})
            })
            .collect();
        opened.iter().for_each(|open| self.send_server(open.to_string().as_bytes()));
        self.list_nodes();
    }

    /// What is due when the loop wakes with no event: the listing's deadline,
    /// a restart after its delay, a restarted server that did not answer, a
    /// compilation with no end.
    fn on_time(&mut self) {
        self.expire_listing();
        let now = Instant::now();
        // A merge past its deadline: the proxy's part or empty alone, else why, the server's late answer dropped.
        let due: Vec<(String, String)> =
            self.deadlines.iter().filter(|(_, (at, _))| *at <= now).map(|(k, (_, m))| (k.clone(), m.clone())).collect();
        // Compiling while a compilation runs whose stall is not told: past it, dbt lsp may never end it.
        let compiling = self.backend.running().is_some_and(|running| running.compiling.due().is_some());
        for (late, method) in due {
            self.deadlines.remove(&late);
            let Some((id, pending)) = self.forwarded.get_mut(&late) else { continue };
            let Some(waited) = pending.deadline() else { continue };
            let why = Reason::Late { waited, compiling };
            let answer = pending.unserved(self.markdown, Some(why), &self.workspace);
            let got = if let Answer::Unanswered(_) = answer {
                "dbt-lsp-plus answered with the reason"
            } else {
                "dbt-lsp-plus answered alone"
            };
            let answer = answer.frame(id, &pending.name);
            pending.kept = Kept::Answered;
            self.send_client(&answer);
            if !self.shutdown && self.late.insert((method.clone(), waited, got)) {
                let ms = waited.as_millis();
                let text = format!("dbt lsp did not answer {method} within {ms} ms: {got}");
                self.send_client(&json!({"jsonrpc": "2.0", "method": "window/logMessage",
                                         "params": {"type": 4, "message": text}}));
            }
        }
        let stalled =
            self.backend.running_mut().filter(|running| running.compiling.due().is_some_and(|due| due <= now));
        if let Some(running) = stalled {
            running.compiling.told = true;
            self.tell_stalled();
        }
        // A restart's due, or a restarted process's deadline: a running process's deadlines are spent above,
        // and its life is put back as it was.
        if self.backend.due().is_some_and(|due| due <= now) {
            match std::mem::take(&mut self.backend.life) {
                Life::Waiting(_) => self.start_restart(),
                Life::Starting(server, ..) => {
                    server.reap(Duration::ZERO);
                    self.lost_server("dbt lsp did not answer its initialize");
                }
                life => self.backend.life = life,
            }
        }
    }

    /// Handles the events of INBOX until `exit`, or until the client closes
    /// its output; a listing past its deadline is abandoned on time.
    fn serve(&mut self, inbox: Receiver<Event>) {
        loop {
            let merge = self.deadlines.values().map(|(at, _)| *at).min();
            let next = [self.backend.due(), merge].into_iter().flatten().min();
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
        self.workspace.epoch += 1;
        // The last frames and the end of a process the proxy let go of.
        if let Event::Server(generation, _) | Event::ServerClosed(generation) = &event {
            let (life, current) = (&self.backend.life, |server: &Server| server.generation == *generation);
            if !matches!(life, Life::Running(Running { server, .. }) | Life::Starting(server, ..) if current(server)) {
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
        match std::mem::replace(&mut self.backend.life, Life::Lost("the session ended".into())) {
            Life::Starting(server, ..) => drop(server.reap(Duration::ZERO)),
            Life::Running(running) => drop(running.server.reap(EXIT_GRACE)),
            _ => {}
        }
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
/// probe and the server start now, in the proxy's working
/// directory; else nothing runs before the client's folder or a document
/// names the project.
pub fn run(command: &[String], settings: modes::Settings, launch: modes::Launch) -> ExitCode {
    let (events, inbox) = mpsc::channel();
    // With `never`, nothing of the user's dbt runs but its parse.
    let never = launch == modes::Launch::Never;
    let given = project_dir(command);
    let dbt = command.iter().take_while(|word| *word != "lsp").cloned().collect::<Vec<_>>().join(" ");
    let no_lsp = || locate::probe(command, None) == Probe::NoLsp;
    // Without a project yet, `take_project` decides once one is named.
    let life = match given.as_ref().map(|_| locate::alone(never, locate::Route::Given, no_lsp)) {
        None => Life::default(),
        Some(Some(why)) => Life::NotStarted(if never { why.to_owned() } else { format!("{dbt} has no lsp") }),
        Some(None) => match spawn_server(command, &events, 0, None) {
            Ok(server) => Life::Running(Running::new(server)),
            Err(why) => Life::Lost(why),
        },
    };
    let dbt = config::Dbt::new(command.to_vec(), None);
    let parsing = Parsing { events: Some(events.clone()), ..Parsing::default() };
    spawn_reader(io::stdin(), events, Event::Client, Event::ClientClosed);
    let mut proxy = Proxy::new(io::BufWriter::new(io::stdout().lock()), life, given);
    (proxy.never, proxy.settings) = (never, settings);
    (proxy.parsing, proxy.workspace.dbt) = (parsing, dbt);
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
    use crate::project;

    #[test]
    fn the_agent_page_lists_the_kinds_that_the_flag_announces() {
        use std::collections::{BTreeMap, BTreeSet};
        type Kinds = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;
        let page = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/how-to/use-with-an-agent.md"));
        let (page, heading) = (page.unwrap(), "### Where dbt-lsp-plus answers definitions and references");
        let section =
            page.lines().skip_while(|line| *line != heading).skip(1).take_while(|line| !line.starts_with('#'));
        let mut table = section.skip_while(|line| !line.starts_with('|')).take_while(|line| line.starts_with('|'));
        let cells = |line: &str| -> Vec<String> {
            let cells = line.trim().trim_matches('|').split('|');
            cells.map(|cell| cell.trim_matches(|c| c == ' ' || c == '`').to_string()).collect()
        };
        let columns = ["request", "kind", "places", "meaning"].map(String::from).to_vec();
        assert_eq!(table.next().map(cells), Some(columns), "the table in the section {heading}");
        assert!(table.next().is_some_and(|line| line.starts_with("|--")), "the table's separator under {heading}");
        let mut listed = Kinds::new();
        for row in table.map(cells) {
            assert_eq!(row.len(), 4, "a row of four cells under {heading}: {row:?}");
            let places = listed.entry(row[0].clone()).or_default().entry(row[1].clone()).or_default();
            places.extend(row[2].split(',').map(|place| place.trim().to_string()));
        }
        let flag = modes::Table::new(&modes::Settings::default(), &|_| false).flag();
        let known = ["version", "definition", "references", "rename"];
        assert!(flag.as_object().unwrap().keys().all(|key| known.contains(&key.as_str())), "{flag}");
        let announced = ["definition", "references"].map(|request| (request.to_string(), flag[request].clone()));
        let announced: Kinds = serde_json::from_value(json!(BTreeMap::from(announced))).unwrap();
        assert!(!listed.is_empty(), "no row under {heading}");
        assert_eq!(listed, announced);
    }

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
                "dbt parse has not finished: waiting for dbt lsp to compile the project",
                r#"(false, None) => "waiting for dbt lsp to compile the project""#,
            ),
            ("dbt parse has not finished: dbt parse failed:", r#"format!("dbt parse failed: {failure}")"#),
            // The arm that puts a failure after the prefix, which the quotes of a failure rest on.
            ("dbt parse has not finished: dbt parse failed:", "(false, Some(failure)) => failure,"),
            ("`dbt lsp is not running`", r#"Reason::NotRunning => f.write_str("dbt lsp is not running")"#),
            ("ends `dbt-lsp-plus answers alone.`", r#"format!("; {why}: dbt-lsp-plus answers alone.")"#),
            // The warnings of a dbt lsp that could not start or exited first, and of one lost for good.
            (
                "`window/showMessage` ending `dbt-lsp-plus answers alone.`",
                r#"let text = format!("{why}: dbt-lsp-plus answers alone.");"#,
            ),
            (
                "`window/showMessage` ending `dbt-lsp-plus answers alone.`",
                r#"None => format!("{why}: dbt-lsp-plus answers alone."),"#,
            ),
            (
                r#""alone":"dbt lsp is not started for a project found from a file""#,
                r#"(_, Route::Document) => Some("dbt lsp is not started for a project found from a file")"#,
            ),
            (
                "no dbt_project.yml at or above <root>; give a path within the project",
                r#"Missing::Above => format!("no dbt_project.yml at or above {shown}; give a path within the project")"#,
            ),
            ("then `... run dbt deps`", r#""dbt parse would install it, rewriting the install path; run dbt deps""#),
            (
                "A session finds one project, once",
                r#"method == "textDocument/didOpen" && self.workspace.root.is_none()"#,
            ),
            // The reasons that the proxy gives where no one answers, one arm each of `Reason`'s words.
            (
                "`dbt lsp did not answer within 2 s`",
                r#"write!(f, "dbt lsp did not answer within {} s{compiling}", waited.as_secs_f64())"#,
            ),
            ("`: it is compiling the project`", r#"if *compiling { ": it is compiling the project" } else { "" }"#),
            ("`dbt lsp failed: ...`", r#"Reason::Failed(message) => write!(f, "dbt lsp failed: {message}")"#),
            ("`dbt lsp is lost for good: ...`", r#"Reason::Lost(why) => write!(f, "dbt lsp is lost for good: {why}")"#),
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

    /// The errors of a compilation, as dbt lsp 2.0.9 reports them: E, a
    /// missing `env_var` placed by `(in`; E2, the same at another place; F,
    /// an error placed only by `-->`; W, a warning; H, a hook's error placed
    /// both ways, as S2's.
    fn reported() -> [Value; 5] {
        let e = "[InvalidConfig (dbt1005)]: Jinja render error: invalid operation: 'env_var': environment variable 'X' not found";
        let f = "[DependencyNotFound (dbt1048)]: Ref 'no_such_model' not found\n  --> models/x.sql:3:10";
        let w = "[InvalidConfig (dbt1005)]: doc macro reference 'no_such_doc' not found\n  --> models/y.yml:7:22";
        let h = "[DbDriverFailed (dbt1308)]: Catalog Error: Table with name no_such_table does not exist!\nDid you mean \"pg_tables\"?\n\nLINE 4: select * from no_such_table\n                      ^\n(in <RUN_OPERATION>:1:9)\n  --> <RUN_OPERATION>:1:9";
        [
            json!({"code": "1005", "message": format!("{e}\n(in :1:1)"), "severity": "Error"}),
            json!({"code": "1005", "message": format!("{e}\n(in :2:1)"), "severity": "Error"}),
            json!({"code": "1048", "message": f, "severity": "Error"}),
            json!({"code": "1005", "message": w, "severity": "Warning"}),
            json!({"code": "1308", "message": h, "severity": "Error"}),
        ]
    }

    /// The frame of MESSAGE, its body spaced as no serialiser would.
    fn frame_of(message: &Value) -> Frame {
        let body = format!("{{\"jsonrpc\":  \"2.0\", {}", &message.to_string()[1..]);
        Frame { body: body.into_bytes(), value: Some(message.clone()) }
    }

    /// The frames that the proxy wrote to its client, CLIENT.
    fn written(client: &[u8]) -> Vec<Frame> {
        let mut client = client;
        std::iter::from_fn(|| rpc::read_frame(&mut client).unwrap()).collect()
    }

    /// A running life, its process a shell that reads what it is sent,
    /// until `stop_server` or `on_server_closed` reaps it.
    fn a_running_life() -> Life {
        let mut child = Command::new("sh").args(["-c", "cat > /dev/null"]).stdin(Stdio::piped()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        Life::Running(Running::new(Server { child, stdin, generation: 0 }))
    }

    #[test]
    fn reads_what_a_compilation_that_compiled_nothing_reports() {
        let [e, _, f, w, h] = reported();
        let e_text = "[InvalidConfig (dbt1005)]: Jinja render error: invalid operation: 'env_var': environment variable 'X' not found (in :1:1)";
        let e_key = "[InvalidConfig (dbt1005)]: Jinja render error: invalid operation: 'env_var': environment variable 'X' not found";
        let f_text = "[DependencyNotFound (dbt1048)]: Ref 'no_such_model' not found --> models/x.sql:3:10";
        let f_key = "[DependencyNotFound (dbt1048)]: Ref 'no_such_model' not found";
        let h_key = "[DbDriverFailed (dbt1308)]: Catalog Error: Table with name no_such_table does not exist! Did you mean \"pg_tables\"? LINE 4: select * from no_such_table ^";
        let h_text = format!("{h_key} (in <RUN_OPERATION>:1:9) --> <RUN_OPERATION>:1:9");
        let said = |text: &str, key: &str| Some((text.to_owned(), key.to_owned()));
        let cases = [
            ("some models compiled", json!({"models_count": 6, "errors": [e]}), None),
            ("no count", json!({"errors": [e]}), None),
            ("a warning, then an error", json!({"models_count": null, "errors": [w, e]}), said(e_text, e_key)),
            ("an error placed by -->", json!({"models_count": null, "errors": [f]}), said(f_text, f_key)),
            (
                "an error placed both ways, cut at the first",
                json!({"models_count": null, "errors": [h]}),
                said(&h_text, h_key),
            ),
            ("a warning alone", json!({"models_count": null, "errors": [w]}), None),
            ("no error", json!({"models_count": null, "errors": []}), None),
            ("errors that are no list", json!({"models_count": null, "errors": 2}), None),
            // As dbt lsp 2.0.9 ends a first compilation that a save or a second listing cut short.
            (
                "cut short",
                json!({"compile_type": "incremental", "duration_ms": 344, "errors": [], "models_count": null}),
                None,
            ),
        ];
        for (name, params, expected) in cases {
            assert_eq!(compile_failure(&params), expected, "{name}");
        }
    }

    #[test]
    fn a_failure_is_said_once_per_first_error() {
        let [e, e2, f, w, h] = reported();
        let mut proxy = Proxy::new(Vec::new(), Life::Lost("dbt lsp is not running".into()), Some(PathBuf::from("/p")));
        let completion = |method: &str, count: Value, errors: &[&Value]| {
            frame_of(&json!({"method": method, "params": {"models_count": count, "errors": errors}}))
        };
        let (foreground, background) = ("dbt/lspCompileComplete", "dbt/lspBackgroundCompileComplete");
        let fed = [
            completion(foreground, json!(6), &[&e]),
            completion(foreground, Value::Null, &[&w, &e]),
            completion(foreground, Value::Null, &[&e2]),
            completion(background, Value::Null, &[&e]),
            completion(foreground, Value::Null, &[&f]),
            completion(foreground, Value::Null, &[&w]),
        ];
        let mut bodies: Vec<Vec<u8>> = fed.iter().map(|frame| frame.body.clone()).collect();
        fed.into_iter().for_each(|frame| proxy.on_server(frame));
        // After `shutdown`, a new first error is forwarded and not said.
        proxy.shutdown = true;
        let after = completion(foreground, Value::Null, &[&h]);
        bodies.push(after.body.clone());
        proxy.on_server(after);
        let said = |text: &str| json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 1, "message": text}});
        let e_said = "dbt lsp compiled nothing: [InvalidConfig (dbt1005)]: Jinja render error: invalid operation: 'env_var': environment variable 'X' not found (in :1:1) (its log: /p/logs/dbt-lsp.log)";
        let f_said = "dbt lsp compiled nothing: [DependencyNotFound (dbt1048)]: Ref 'no_such_model' not found --> models/x.sql:3:10 (its log: /p/logs/dbt-lsp.log)";
        let frames = written(&proxy.client);
        let messages: Vec<(usize, &Value)> = frames
            .iter()
            .enumerate()
            .filter_map(|(at, frame)| {
                frame.value.as_ref().filter(|m| m["method"] == "window/showMessage").map(|m| (at, m))
            })
            .collect();
        // Each after the completion that brought it.
        assert_eq!(messages, [(2, &said(e_said)), (6, &said(f_said))]);
        let forwarded: Vec<&Vec<u8>> = frames
            .iter()
            .filter(|frame| frame.value.as_ref().is_some_and(|m| m["method"] != "window/showMessage"))
            .map(|frame| &frame.body)
            .collect();
        assert_eq!(forwarded, bodies.iter().collect::<Vec<_>>(), "each completion as it came");
    }

    #[test]
    fn dbt_lsp_s_frames_set_and_clear_the_compilation_s_deadline() {
        let waiting = json!({"method": "$/progress", "params": {"token": "t:dbt/progress/references",
                             "value": {"kind": "begin", "title": "Finding All References",
                                       "message": "Waiting for compilation to complete...", "cancellable": true}}});
        let compiling = |method: &str| json!({"method": method, "params": {"models_count": 6, "errors": []}});
        // Each frame, after `told` is set as given, and whether a deadline and `parsing.compiled` follow it.
        let steps = [
            ("a progress that awaits a compilation", waiting, false, (false, false)),
            ("a background start", compiling("dbt/lspBackgroundCompileStart"), false, (true, false)),
            (
                "its completion, which leaves compiled as it was",
                compiling("dbt/lspBackgroundCompileComplete"),
                false,
                (false, false),
            ),
            ("a start", compiling("dbt/lspCompileStart"), false, (true, false)),
            ("its completion", compiling("dbt/lspCompileComplete"), false, (false, true)),
            ("a start after one told", compiling("dbt/lspCompileStart"), true, (true, true)),
        ];
        let mut proxy = Proxy::new(Vec::new(), a_running_life(), Some(PathBuf::from("/p")));
        for (name, message, told, expected) in steps {
            proxy.backend.running_mut().unwrap().compiling.told = told;
            let before = Instant::now();
            proxy.on_server(frame_of(&message));
            let due = proxy.backend.running().unwrap().compiling.due();
            let after = Instant::now();
            assert!(due.is_none_or(|due| before + COMPILE_STALL <= due && due <= after + COMPILE_STALL), "{name}");
            assert_eq!((due.is_some(), proxy.parsing.compiled), expected, "{name}");
        }
        proxy.stop_server();
    }

    #[test]
    fn a_compilation_without_its_end_is_told_after_its_deadline() {
        let said = |proxy: &Proxy<Vec<u8>>| -> Vec<Value> {
            let frames = written(&proxy.client).into_iter().filter_map(|frame| frame.value);
            frames.filter(|m| m["method"] == "window/showMessage").collect()
        };
        // A start AGO, as dbt lsp sends it.
        let started = |proxy: &mut Proxy<Vec<u8>>, ago: Duration| {
            proxy.on_server(frame_of(&json!({"method": "dbt/lspCompileStart", "params": {}})));
            proxy.backend.running_mut().unwrap().compiling.since = Some(Instant::now() - ago);
        };
        let due = |proxy: &Proxy<Vec<u8>>| proxy.backend.running().unwrap().compiling.due();
        let mut proxy = Proxy::new(Vec::new(), a_running_life(), Some(PathBuf::from("/p")));
        started(&mut proxy, Duration::ZERO);
        proxy.on_time();
        assert!(said(&proxy).is_empty() && due(&proxy).is_some(), "started just now");
        started(&mut proxy, COMPILE_STALL);
        proxy.on_time();
        let text = "dbt lsp has been compiling for 60 s with no end yet (its log: /p/logs/dbt-lsp.log)";
        let told = json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": text}});
        assert_eq!(said(&proxy), std::slice::from_ref(&told));
        proxy.on_time();
        assert_eq!(said(&proxy), std::slice::from_ref(&told), "told once");
        started(&mut proxy, COMPILE_STALL);
        proxy.on_time();
        assert_eq!((said(&proxy), due(&proxy)), (vec![told.clone()], None), "once a session");
        proxy.stop_server();
        let mut after = Proxy::new(Vec::new(), a_running_life(), Some(PathBuf::from("/p")));
        after.shutdown = true;
        started(&mut after, COMPILE_STALL);
        after.on_time();
        assert_eq!((said(&after), due(&after)), (Vec::new(), None), "nothing after shutdown");
        after.stop_server();
        // `serve` wakes for that deadline when nothing else comes, here a client that closes once written to.
        struct Closing(Vec<u8>, Sender<Event>);
        impl Write for Closing {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let _ = self.1.send(Event::ClientClosed);
                self.0.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (events, inbox) = mpsc::channel();
        let watchdog = events.clone();
        // Only a `serve` that never wakes waits for it, and ends untold.
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(10));
            let _ = watchdog.send(Event::ClientClosed);
        });
        let mut serving = Proxy::new(Closing(Vec::new(), events), a_running_life(), Some(PathBuf::from("/p")));
        serving.backend.running_mut().unwrap().compiling.since = Some(Instant::now() - COMPILE_STALL);
        serving.serve(inbox);
        serving.stop_server();
        let frames: Vec<Value> = written(&serving.client.0).into_iter().filter_map(|frame| frame.value).collect();
        assert_eq!(frames, [told], "told through serve");
    }

    #[test]
    fn a_lost_server_keeps_the_failures_said_and_clears_its_deadline() {
        let mut proxy = Proxy::new(Vec::new(), a_running_life(), Some(PathBuf::from("/p")));
        let [e, ..] = reported();
        let failed = json!({"method": "dbt/lspCompileComplete", "params": {"models_count": null, "errors": [e]}});
        let shown = |proxy: &Proxy<Vec<u8>>| {
            let frames = written(&proxy.client).into_iter().filter_map(|frame| frame.value);
            frames.filter(|m| m["method"] == "window/showMessage").count()
        };
        proxy.on_server(frame_of(&failed));
        assert_eq!(shown(&proxy), 1);
        proxy.on_server(frame_of(&json!({"method": "dbt/lspCompileStart", "params": {}})));
        assert!(proxy.backend.running().unwrap().compiling.due().is_some());
        proxy.on_server_closed();
        assert!(proxy.backend.running().is_none() && proxy.backend.due().is_none(), "no deadline of a lost server");
        // The next server's first failure, the same.
        proxy.on_server(frame_of(&failed));
        assert_eq!(shown(&proxy), 1);
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

    #[test]
    fn a_loss_waits_for_a_restart_while_one_is_left_and_is_lost_for_good_after() {
        let why = "dbt lsp exited (x)";
        let mut proxy = Proxy::new(Vec::new(), Life::default(), None);
        proxy.initialized = true;
        let before = Instant::now();
        proxy.lost_server(why);
        let after = Instant::now();
        let Life::Waiting(due) = proxy.backend.life else { panic!("no restart awaited") };
        let second = Duration::from_secs(1);
        assert!(before + second <= due && due <= after + second && !proxy.backend.alone(), "{due:?}");
        // Nor is a restart under way, its process reaped once checked.
        let Life::Running(Running { server, .. }) = a_running_life() else { panic!("a running life") };
        proxy.backend.life = Life::Starting(server, "dbt-lsp-plus/1".into(), Instant::now() + REPLAY_TIMEOUT);
        assert!(!proxy.backend.alone(), "starting again");
        proxy.stop_server();
        // Three restarts in the window: none is left.
        let ago = |seconds| Instant::now().checked_sub(Duration::from_secs(seconds)).unwrap();
        proxy.backend.restarts = vec![ago(30), ago(20), ago(10)];
        proxy.lost_server(why);
        assert!(matches!(&proxy.backend.life, Life::Lost(lost) if lost == why) && proxy.backend.alone());
        let mut after_shutdown = Proxy::new(Vec::new(), Life::default(), None);
        (after_shutdown.initialized, after_shutdown.shutdown) = (true, true);
        after_shutdown.lost_server(why);
        assert!(matches!(&after_shutdown.backend.life, Life::Lost(lost) if lost == why));
        assert!(after_shutdown.client.is_empty(), "nothing after shutdown");
    }

    #[test]
    fn a_give_up_during_a_parse_starts_no_second_one() {
        // Whether the index is still fresh once the last restart of the window is spent, a parse under way or not.
        let fresh_after_give_up = |running: Option<run::Running>| {
            let mut proxy = Proxy::new(Vec::new(), Life::default(), None);
            proxy.initialized = true;
            let ago = |seconds| Instant::now().checked_sub(Duration::from_secs(seconds)).unwrap();
            proxy.backend.restarts = vec![ago(30), ago(20), ago(10)];
            (proxy.workspace.fresh, proxy.workspace.running) = (true, running);
            proxy.lost_server("dbt lsp exited (x)");
            assert!(matches!(proxy.backend.life, Life::Lost(_)), "given up");
            let fresh = proxy.workspace.fresh;
            proxy.stop_parse();
            fresh
        };
        // A parse in flight, whose `Parsed` event `on_parsed` has yet to take.
        assert!(fresh_after_give_up(Some(run::Running::spawn(|_| {}))), "a parse under way");
        assert!(!fresh_after_give_up(None), "no parse under way");
    }

    #[test]
    fn lost_for_good_a_hover_on_a_node_before_any_index_says_why() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let orders = features::uri(&root.join("models/marts/orders.sql"));
        let inside_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}});
        // No `initialize` answered: only the life says who answers.  The parse failed, as with a missing dbt.
        let hover = |life: Life| {
            let mut proxy = Proxy::new(Vec::new(), life, Some(root.clone()));
            proxy.table = modes::Table::new(&modes::Settings::default(), &|_| false);
            (proxy.workspace.fresh, proxy.workspace.failure) = (true, Some("dbt parse failed: x".into()));
            proxy.on_request("textDocument/hover", &json!(1), &inside_ref, b"");
            written(&proxy.client).into_iter().filter_map(|frame| frame.value).find(|m| m["id"] == 1).unwrap()
        };
        let lost = hover(Life::Lost("dbt lsp exited (x)".into()));
        assert_eq!(
            lost["error"],
            json!({"code": -32803, "message": "dbt parse has not finished: dbt parse failed: x"})
        );
        // In front, a restart awaited: the proxy's reason, then dbt lsp's.
        let waiting = hover(Life::Waiting(Instant::now() + Duration::from_secs(60)));
        let message = "dbt parse has not finished: dbt parse failed: x; dbt lsp is not running";
        assert_eq!(waiting["error"], json!({"code": -32803, "message": message}), "{waiting}");
    }

    #[test]
    fn with_no_dbt_lsp_a_hover_before_any_index_says_why_after_asking_for_one() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let orders = features::uri(&root.join("models/marts/orders.sql"));
        let inside_ref = json!({"textDocument": {"uri": orders}, "position": {"line": 4, "character": 28}});
        let error = |proxy: &Proxy<Vec<u8>>, id: i64| {
            let frames = written(&proxy.client).into_iter().filter_map(|frame| frame.value);
            frames
                .filter(|m| m["id"] == id)
                .map(|m| m["error"]["message"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        };
        // A restart awaited, no index, none asked for yet: the hover asks for one, then says why there is none,
        // as nodeInfo would now; the parse cannot start here, the server command having no `lsp`.
        let mut proxy =
            Proxy::new(Vec::new(), Life::Waiting(Instant::now() + Duration::from_secs(60)), Some(root.clone()));
        proxy.table = modes::Table::new(&modes::Settings::default(), &|_| false);
        proxy.on_request("textDocument/hover", &json!(1), &inside_ref, b"");
        let asked = format!("{}; dbt lsp is not running", proxy.workspace.unindexed());
        assert!(!asked.contains("waiting for dbt lsp"), "{asked}");
        assert_eq!(error(&proxy, 1), [asked]);
        // A give-up, which parses again, then answers the hover that dbt lsp left.
        let mut proxy = Proxy::new(Vec::new(), a_running_life(), Some(root));
        proxy.initialized = true;
        let ago = |seconds| Instant::now().checked_sub(Duration::from_secs(seconds)).unwrap();
        proxy.backend.restarts = vec![ago(30), ago(20), ago(10)];
        let hover = Pending { name: "textDocument/hover".into(), kept: Kept::Part(Part::Unindexed) };
        proxy.forwarded.insert(key(&json!(2)), (json!(2), hover));
        proxy.on_server_closed();
        let left = error(&proxy, 2);
        let given_up = format!("{}; dbt lsp exited (", proxy.workspace.unindexed());
        assert!(
            !given_up.contains("waiting for dbt lsp") && left.len() == 1 && left[0].starts_with(&given_up),
            "{left:?}"
        );
    }

    #[test]
    fn a_late_answer_says_it_is_compiling_only_until_the_stall_is_told() {
        // The message of a definition that dbt lsp left past its deadline, its compilation as given.
        let late = |since: Option<Instant>, told: bool| {
            let mut proxy = Proxy::new(Vec::new(), a_running_life(), Some(PathBuf::from("/p")));
            proxy.backend.running_mut().unwrap().compiling = Compiling { since, told };
            let pending = Pending { name: "textDocument/definition".into(), kept: Kept::Part(Part::Nothing) };
            proxy.forwarded.insert(key(&json!(1)), (json!(1), pending));
            proxy.deadlines.insert(key(&json!(1)), (Instant::now(), "textDocument/definition".into()));
            proxy.on_time();
            proxy.stop_server();
            let answer = written(&proxy.client).into_iter().filter_map(|frame| frame.value).find(|m| m["id"] == 1);
            answer.unwrap()["error"]["message"].clone()
        };
        let plain = "dbt lsp did not answer within 2 s";
        assert_eq!(late(None, false), plain, "no compilation");
        assert_eq!(late(Some(Instant::now()), false), format!("{plain}: it is compiling the project"));
        assert_eq!(late(Some(Instant::now() - COMPILE_STALL), true), plain, "a stall told");
    }

    #[test]
    fn a_request_reads_the_held_scan_until_the_open_text_differs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let path = root.join("models/marts/orders.sql");
        let params = json!({"textDocument": {"uri": features::uri(&path)}, "position": {"line": 0, "character": 0}});
        let mut proxy = Proxy::new(Vec::new(), Life::default(), Some(root));
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
        let mut proxy = Proxy::new(Vec::new(), Life::default(), Some(root.clone()));
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
            let mut proxy = Proxy::new(Vec::new(), Life::default(), Some(root.to_owned()));
            proxy.table = modes::Table::new(&modes::Settings::default(), &|_| false);
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
        let mut proxy = proxy_on(&root);
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
        let hovered = calls.map(|needle| match proxy.hover_part(&at(&root, needle).0) {
            Part::Hover(text, _) => text,
            _ => String::new(),
        });
        let described = hovered.iter().zip(expected).all(|(text, (id, _))| text.contains(&format!("`{id}`")));
        assert!(described, "{hovered:?}");
        let Kept::Part(Part::Hints(found)) = proxy.remember("textDocument/inlayHint", &json!(1), &hints(&root)).0.kept
        else {
            panic!("no hints in uses.sql")
        };
        let labels: Vec<&Value> = found.iter().map(|hint| &hint["label"]).collect();
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
        assert!(proxy.hover_part(&dim) == Part::Unanswered(vec![Reason::Unreadable(broken.clone())]), "a hover's part");
        let kept = proxy.remember("textDocument/inlayHint", &json!(1), &hints(&broken)).0.kept;
        assert!(kept == Kept::Part(Part::Empty), "the hints' part");
        // An entry's `name:` reads as a `ref` there: `dim` of `name: dim`.
        let entry = json!({"line": 2, "character": 10});
        let dim_yml = features::uri(&broken.join("models/_dim.yml"));
        let (on_entry, entry_info) = (
            json!({"textDocument": {"uri": dim_yml}, "position": entry}),
            json!({"arguments": [{"uri": dim_yml, "position": entry}]}),
        );
        let unreadable = (
            Some(Answer::Unanswered(vec![Reason::Unreadable(broken.clone())])),
            Part::Unanswered(vec![Reason::Unreadable(broken.clone())]),
        );
        let entry_read = (proxy.node_info(&entry_info), proxy.hover_part(&on_entry));
        proxy.workspace.index = None;
        assert!(proxy.hover_part(&dim) == Part::Unindexed, "a hover's part before any index");
        let entry_before = proxy.hover_part(&on_entry);
        assert_eq!(
            (entry_read, entry_before),
            (unreadable, Part::Unindexed),
            "an entry's, with an index then before any"
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(broken).unwrap();
    }

    #[test]
    fn a_position_that_names_nothing_is_empty_and_a_target_found_nowhere_is_found() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop");
        let proxy_on = |text: &str, path: &Path| {
            let mut proxy = Proxy::new(Vec::new(), Life::default(), Some(root.clone()));
            proxy.documents.insert(path.to_owned(), text.to_owned());
            proxy
        };
        let at = |path: &Path, character: usize| json!({"textDocument": {"uri": features::uri(path)}, "position": {"line": 0, "character": character}});
        // An unsaved document's ref to a model that no file holds, and a keyword before it.
        let scratch = root.join("models/marts/scratch.sql");
        let text = "select * from {{ ref('nowhere') }}";
        let proxy = proxy_on(text, &scratch);
        let (keyword, nowhere) = (at(&scratch, 1), at(&scratch, text.find("nowhere").unwrap() + 2));
        assert_eq!(
            [proxy.on_definition(&keyword), proxy.on_references(&keyword)],
            [Some(Answer::Empty), Some(Answer::Empty)]
        );
        let found = [Some(Answer::Found(Value::Null)), Some(Answer::Found(json!([])))];
        assert_eq!([proxy.on_definition(&nowhere), proxy.on_references(&nowhere)], found);
        let outside = at(&root.join("packages.yml"), 0);
        assert_eq!(proxy.hierarchy("textDocument/prepareCallHierarchy", &outside), Some(Answer::Empty));
    }

    #[test]
    fn a_request_dbt_lsp_left_gets_its_part_its_empty_or_why() {
        let hint = json!({"position": {"line": 0, "character": 0}, "label": "x"});
        let location = json!({"uri": "file:///p/models/a.sql", "range": {}});
        let pending = |name: &str, kept: Kept| Pending { name: name.to_owned(), kept };
        let part = |name: &str, part: Part| pending(name, Kept::Part(part));
        let why = || vec![Reason::NotRunning];
        // No index: why the proxy has none, as nodeInfo says it.
        let unindexed = Reason::Unindexed("waiting for dbt lsp to compile the project".into());
        let hover = json!({"contents": {"kind": "plaintext", "value": "a summary"}});
        let cases = [
            (part("textDocument/hover", Part::Hover("a summary".into(), true)), Answer::Found(hover)),
            (part("textDocument/inlayHint", Part::Hints(vec![hint.clone()])), Answer::Found(json!([hint]))),
            (part("textDocument/definition", Part::Locations(location.clone())), Answer::Found(location)),
            (part("textDocument/references", Part::Empty), Answer::Empty),
            (part("textDocument/definition", Part::Nothing), Answer::Unanswered(why())),
            (
                part("textDocument/definition", Part::Unanswered(vec![Reason::Unreadable("/p".into())])),
                Answer::Unanswered(vec![Reason::Unreadable("/p".into()), Reason::NotRunning]),
            ),
            (part("textDocument/hover", Part::Unindexed), Answer::Unanswered(vec![unindexed, Reason::NotRunning])),
            (pending("textDocument/completion", Kept::Plain), Answer::Unanswered(why())),
        ];
        let parsing = Workspace::default();
        for (pending, expected) in cases {
            assert_eq!(pending.unserved(false, Some(Reason::NotRunning), &parsing), expected, "{}", pending.name);
        }
        // Once an index exists, a hover's part taken before it is a nothing.
        let mut indexed = Workspace::default();
        indexed.index = Some(config::ParseIndex::of(&json!({})));
        let late = part("textDocument/hover", Part::Unindexed).unserved(false, Some(Reason::NotRunning), &indexed);
        assert_eq!(late, Answer::Unanswered(why()));
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
        let mut proxy = Proxy::new(Vec::new(), a_running_life(), None);
        let listings = &mut proxy.backend.running_mut().unwrap().listings;
        listings.nodes = Some(Vec::new());
        listings.listing = Some(("dbt-lsp-plus/1".into(), 1, Instant::now() - LIST_TIMEOUT));
        listings.listed = vec![ServerNode {
            id: "model.shop.a".into(),
            name: "a".into(),
            resource_type: "model".into(),
            package: "shop".into(),
            original_file_path: "models/a.sql".into(),
            depends_on: Vec::new(),
        }];
        proxy.expire_listing();
        let listings = &proxy.backend.running().unwrap().listings;
        assert!(listings.listing.is_none() && listings.listed.is_empty());
        assert_eq!(listings.nodes, Some(Vec::new()));
        // A listing within its deadline stays.
        proxy.backend.running_mut().unwrap().listings.listing = Some(("dbt-lsp-plus/2".into(), 0, Instant::now()));
        proxy.expire_listing();
        assert!(proxy.backend.running().unwrap().listings.listing.is_some());
        proxy.stop_server();
    }
}
