// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The proxy: forwards every message between the editor and `dbt lsp`
//! byte for byte, and answers itself the requests of the features that the
//! server does not announce.

use crate::graph::{self, Graph, ServerNode};
use crate::project::Project;
use crate::rpc::{self, Frame};
use crate::text::{self, Encoding, LineIndex};
use crate::{features, project};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
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
const ADDED: [(&str, &[&str]); 3] = [
    ("documentSymbolProvider", &["textDocument/documentSymbol"]),
    ("workspaceSymbolProvider", &["workspace/symbol"]),
    (
        "callHierarchyProvider",
        &["textDocument/prepareCallHierarchy", "callHierarchy/incomingCalls", "callHierarchy/outgoingCalls"],
    ),
];

/// The selectors of the graph, one request each: `dbt.listNodes` reads only
/// its first argument, `*` leaves sources out, and a second request in
/// flight cancels the first.
const SELECTORS: [&str; 2] = ["*", "source:*"];
/// How often a listing cancelled by another `dbt.listNodes` is tried again.
const MAX_ATTEMPTS: u32 = 3;
/// A listing unanswered for this long is abandoned.
const LIST_TIMEOUT: Duration = Duration::from_secs(120);
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

enum Event {
    Client(Frame),
    ClientClosed,
    Server(Frame),
    ServerClosed,
}

struct Server {
    child: Child,
    stdin: ChildStdin,
}

/// What the proxy remembers of a request of the client's that it forwarded.
#[derive(Clone, Copy, PartialEq)]
enum Pending {
    Request,
    /// A `dbt.listNodes`: the proxy lists only when none is pending, since a
    /// second listing cancels the first.
    Listing,
    /// Answered with null if the server dies first, as LSP asks of `shutdown`.
    Shutdown,
}

struct Proxy<W: Write> {
    client: W,
    server: Option<Server>,
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
}

/// The path of a `file:` URI.
fn uri_path(uri: Option<&str>) -> Option<PathBuf> {
    Url::parse(uri?).ok()?.to_file_path().ok()
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
    let value = command.iter().enumerate().find_map(|(at, arg)| match arg.strip_prefix("--project-dir") {
        Some("") => command.get(at + 1).cloned(),
        Some(rest) => rest.strip_prefix('=').map(str::to_owned),
        None => None,
    })?;
    let spelt = std::path::absolute(&value).ok()?.components().fold(PathBuf::new(), |mut path, component| {
        match component {
            Component::ParentDir => {
                path.pop();
            }
            other => path.push(other),
        }
        path
    });
    match fs::canonicalize(&value) {
        Ok(resolved) if fs::canonicalize(&spelt).ok().as_ref() != Some(&resolved) => Some(resolved),
        _ => Some(spelt),
    }
}

impl<W: Write> Proxy<W> {
    fn new(client: W, server: Option<Server>, absent: Option<String>, root: Option<PathBuf>) -> Self {
        let mut proxy = Proxy {
            client,
            server,
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
            opened: false,
            next_id: 0,
            listing: None,
            listed: Vec::new(),
            attempts: 0,
            relist: false,
            nodes: None,
            shutdown: false,
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
    }

    /// Answers `initialize` without a server: the proxy's own features only.
    fn initialize_alone(&mut self, id: &Value) {
        let mut capabilities = serde_json::Map::new();
        capabilities.insert("textDocumentSync".into(), json!({"openClose": true, "change": 2}));
        self.add_capabilities(&mut capabilities);
        let info = json!({"name": "dbt-lsp-plus", "version": env!("CARGO_PKG_VERSION")});
        self.send_client(
            &json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": capabilities, "serverInfo": info}}),
        );
        let why = self.absent.take().unwrap_or_else(|| "dbt lsp is not running".into());
        let text = format!("{why}: dbt-lsp-plus serves its outline, symbols and call hierarchy only.");
        self.send_client(
            &json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 2, "message": text}}),
        );
    }

    /// Learns from the client's `initialize`: the outline's shape, and the
    /// root when the server command named none, or the client's spelling of
    /// the one it named.
    fn on_initialize(&mut self, params: &Value) {
        self.hierarchical = params
            .pointer("/capabilities/textDocument/documentSymbol/hierarchicalDocumentSymbolSupport")
            == Some(&json!(true));
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

    /// Keeps the text of the open documents, for the outline of unsaved edits.
    fn track(&mut self, method: &str, params: &Value) {
        let Some(path) = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str)) else { return };
        match method {
            "textDocument/didOpen" => {
                let text = params.pointer("/textDocument/text").and_then(Value::as_str).unwrap_or_default();
                self.documents.insert(path, text.to_owned());
            }
            "textDocument/didChange" => {
                let changes = params.get("contentChanges").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
                if let Some(text) = self.documents.get_mut(&path) {
                    changes.iter().for_each(|change| text::apply_change(text, change, self.encoding));
                }
            }
            "textDocument/didClose" => {
                self.documents.remove(&path);
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
        let text = self
            .local_path(Some(uri))
            .and_then(|path| self.documents.get(&path).cloned().or_else(|| text::read(&path)));
        Some(text.map_or(Value::Null, |text| features::document_symbols(&text, self.encoding, self.hierarchical, uri)))
    }

    /// The answer to the call hierarchy's METHOD, on the graph of the files
    /// with the server's nodes once listed.
    fn hierarchy(&self, method: &str, params: &Value) -> Option<Value> {
        let Some(project) = self.project() else { return Some(Value::Null) };
        let scanned = Graph::scanned(&project);
        let graph = match &self.nodes {
            Some(nodes) => scanned.with_server(nodes, &project),
            None => scanned,
        };
        if method != "textDocument/prepareCallHierarchy" {
            let id = params.pointer("/item/data/uniqueId")?.as_str()?;
            let incoming = method == "callHierarchy/incomingCalls";
            return Some(features::calls(&project, &graph, id, incoming, self.encoding));
        }
        let path = self.local_path(params.pointer("/textDocument/uri").and_then(Value::as_str))?;
        let text = project.texts.get(&path).map_or("", String::as_str);
        let offset = LineIndex::new(text).offset_of(params.get("position")?, self.encoding)?;
        Some(features::prepare_call_hierarchy(&project, &graph, &path, offset, self.encoding))
    }

    /// Answers a request of a feature that the proxy owns.  A panic while
    /// answering fails that request alone, never the session.
    fn answer(&mut self, method: &str, id: &Value, params: &Value) {
        let computed = panic::catch_unwind(AssertUnwindSafe(|| match method {
            "textDocument/documentSymbol" => self.outline(params),
            "workspace/symbol" => params.get("query").and_then(Value::as_str).map(|query| {
                self.project().map_or(json!([]), |project| features::workspace_symbols(&project, query, self.encoding))
            }),
            _ => self.hierarchy(method, params),
        }));
        match computed {
            Ok(Some(result)) => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
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
            // The server would get two requests under one id.
            (Some(_), Some(id)) if self.listing.as_ref().is_some_and(|(own, ..)| id.as_str() == Some(own)) => {
                self.reply_error(id, INVALID_REQUEST, "dbt-lsp-plus has a request in flight under this id")
            }
            (Some(method), Some(id)) => self.on_request(method, id, params, &frame.body),
            (Some(method), None) => {
                self.track(method, params);
                // The server gets the change before the listing it prompts.
                self.send_server(&frame.body);
                let first_open = method == "textDocument/didOpen" && !std::mem::replace(&mut self.opened, true);
                if CHANGES.contains(&method) || first_open {
                    self.list_nodes();
                }
            }
            _ => self.send_server(&frame.body),
        }
    }

    /// A request of the client: answered here when the proxy owns its method,
    /// else forwarded; without a server, the lifecycle requests are answered
    /// as LSP asks and any other fails.
    fn on_request(&mut self, method: &str, id: &Value, params: &Value, body: &[u8]) {
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
            (_, Some("dbt.listNodes")) => Pending::Listing,
            _ => Pending::Request,
        };
        match (self.owned.get(method).copied(), &self.server, method) {
            (Some(owned), ..) => self.answer(owned, id, params),
            (None, Some(_), _) => {
                self.forwarded.insert(key(id), (id.clone(), pending));
                self.send_server(body);
            }
            (None, None, "initialize") => {
                self.initialize = None;
                self.initialize_alone(id);
            }
            (None, None, "shutdown") => self.send_client(&json!({"jsonrpc": "2.0", "id": id, "result": null})),
            (None, None, _) => self.reply_error(id, REQUEST_FAILED, "dbt lsp is not running"),
        }
    }

    /// A message of the server.  A response is the proxy's own only when its
    /// id has the proxy's prefix and answers no request of the client's.
    fn on_server(&mut self, frame: Frame) {
        let Some(message) = &frame.value else { return self.send_client_body(&frame.body) };
        if let (None, Some(id)) = (message.get("method"), message.get("id")) {
            let pending = self.forwarded.remove(&key(id));
            if let (None, Some(own)) = (&pending, id.as_str())
                && own.starts_with(OWN_ID)
            {
                return self.on_own_response(own, message);
            }
            if let Some((_, Pending::Listing)) = pending {
                self.resume();
            }
            if self.initialize.as_ref() == Some(id) {
                self.initialize = None;
                let mut message = message.clone();
                let encoding = message.pointer("/result/capabilities/positionEncoding").and_then(Value::as_str);
                self.encoding = Encoding::parse(encoding);
                if let Some(capabilities) = message.pointer_mut("/result/capabilities").and_then(Value::as_object_mut) {
                    self.add_capabilities(capabilities);
                }
                return self.send_client(&message);
            }
        }
        self.send_client_body(&frame.body);
    }

    /// The server is gone: answer what it left unanswered, once each, and
    /// carry on alone, on the scanned graph.  Before the client's
    /// `initialize`, only remember why, for its answer.
    fn on_server_closed(&mut self) {
        let Some(mut server) = self.server.take() else { return };
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
        self.listed.clear();
        self.relist = false;
        self.nodes = None;
        self.absent = Some(why.clone());
        if let Some(id) = self.initialize.take() {
            return self.initialize_alone(&id);
        }
        if self.initialized && !self.shutdown {
            let text = format!("{why}: dbt-lsp-plus keeps its outline, symbols and call hierarchy.");
            self.send_client(
                &json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": {"type": 1, "message": text}}),
            );
        }
    }

    /// Handles the events of INBOX until `exit`, or until the client closes
    /// its output; a listing past its deadline is abandoned on time.
    fn serve(&mut self, inbox: Receiver<Event>) {
        loop {
            let wait =
                self.listing.as_ref().map_or(Duration::MAX, |(_, _, sent)| LIST_TIMEOUT.saturating_sub(sent.elapsed()));
            let event = match inbox.recv_timeout(wait) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    self.expire_listing();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            match event {
                Event::Client(frame) => {
                    let exit = frame.value.as_ref().and_then(|m| m.get("method")) == Some(&json!("exit"));
                    self.on_client(frame);
                    if exit {
                        return;
                    }
                }
                Event::ClientClosed => return,
                Event::Server(frame) => self.on_server(frame),
                Event::ServerClosed => self.on_server_closed(),
            }
        }
    }

    /// Waits for the server to exit after `exit`, within EXIT_GRACE, then kills it.
    fn stop_server(&mut self) {
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

fn spawn_reader(input: impl io::Read + Send + 'static, events: Sender<Event>, wrap: fn(Frame) -> Event, closed: Event) {
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

/// Runs the proxy on the standard input and output, in front of COMMAND.
pub fn run(command: &[String]) -> ExitCode {
    let (events, inbox) = mpsc::channel();
    let spawned = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let (server, absent) = match spawned {
        Ok(mut child) => {
            let stdout = child.stdout.take().expect("piped stdout");
            spawn_reader(stdout, events.clone(), Event::Server, Event::ServerClosed);
            let stdin = child.stdin.take().expect("piped stdin");
            (Some(Server { child, stdin }), None)
        }
        Err(error) => {
            eprintln!("dbt-lsp-plus: cannot start {}: {error}", command.join(" "));
            (None, Some(format!("dbt lsp could not start ({error})")))
        }
    };
    spawn_reader(io::stdin(), events, Event::Client, Event::ClientClosed);
    let mut proxy = Proxy::new(io::BufWriter::new(io::stdout().lock()), server, absent, project_dir(command));
    proxy.serve(inbox);
    proxy.stop_server();
    if proxy.shutdown { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

#[cfg(test)]
mod tests {
    use super::*;

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
