// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! A client of dbt-lsp-plus for the tests: it starts the server with a
//! server command and speaks LSP to it.

use serde_json::{Value, json};
use std::fs::{File, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// Reads the next message, null for a body that is not JSON, or `Ok(None)`
/// at the end of the stream.  An error means the frame boundary is lost and
/// the caller must stop reading.
fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let length = length.ok_or_else(|| io::Error::other("no Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/// Writes MESSAGE as one frame.
fn write_value(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop")
}

/// A fresh directory for test NAME under Cargo's temporary directory.  Two
/// runs in one checkout share that directory, so the first call in a process
/// waits for the lock on it, held until the process ends: runs take turns.
pub fn scratch(name: &str) -> PathBuf {
    static TURN: OnceLock<File> = OnceLock::new();
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
    TURN.get_or_init(|| {
        // Cargo makes it when it builds the tests, not when it runs them.
        std::fs::create_dir_all(base).expect("create Cargo's temporary directory");
        let path = base.join("turn.lock");
        let lock = File::create(&path).expect("create the lock of the test runs");
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                // Past the harness's capture of `eprintln!`, so that a run that waits says so.
                let _ = writeln!(std::io::stderr(), "waiting for another test run in this checkout to end");
                lock.lock().expect("wait for the lock of the test runs");
            }
            Err(TryLockError::Error(error)) => panic!("lock {path:?}: {error}"),
        }
        lock
    });
    let dir = base.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// A copy of the fixture, as `shop` in a fresh directory for test NAME.
pub fn copy_fixture(name: &str) -> PathBuf {
    let project = scratch(name).join("shop");
    let status = Command::new("cp").arg("-R").arg(fixture()).arg(&project).status().expect("run cp");
    assert!(status.success(), "copy the fixture to {project:?}");
    project
}

pub fn uri(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

pub struct Session {
    /// The proxy's process.
    pub child: Child,
    stdin: ChildStdin,
    inbox: Receiver<Value>,
    next_id: i64,
    pub timeout: Duration,
    /// Every message the client received, in order, parsed; null for a body
    /// that is not JSON.
    pub received: Vec<Value>,
}

impl Session {
    /// Starts the server with the server command SERVER, in DIR when given,
    /// from an empty environment but for PATH and ENV.
    pub fn start(server: &[String], env: &[(&str, &Path)], dir: Option<&Path>) -> Session {
        Session::start_logged(server, env, dir, None)
    }

    /// The same, the server's standard error written to the file STDERR when
    /// given.
    pub fn start_logged(
        server: &[String],
        env: &[(&str, &Path)],
        dir: Option<&Path>,
        stderr: Option<&Path>,
    ) -> Session {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dbt-lsp-plus"));
        command.arg("--").args(server);
        let stderr =
            stderr.map_or_else(Stdio::inherit, |path| File::create(path).expect("create the stderr's file").into());
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(stderr);
        command.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default());
        for (name, value) in env {
            command.env(name, value);
        }
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let mut child = command.spawn().expect("start dbt-lsp-plus");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, inbox) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(Some(message)) = read_frame(&mut stdout) {
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        Session { child, stdin, inbox, next_id: 100, timeout: Duration::from_secs(10), received: Vec::new() }
    }

    /// Starts the server with the fake dbt, `lsp ARGUMENTS...`, playing
    /// SCRIPT, in DIR when given; returns the session and the fake's log.
    pub fn fake_with(name: &str, script: Value, arguments: &[&Path], dir: Option<&Path>) -> (Session, PathBuf) {
        let scratch = scratch(name);
        let (script_path, log) = (scratch.join("script.json"), scratch.join("log.jsonl"));
        std::fs::write(&script_path, script.to_string()).unwrap();
        let fake = [env!("CARGO_BIN_EXE_fake-dbt"), "lsp"].map(String::from);
        let arguments = arguments.iter().map(|argument| argument.to_string_lossy().into_owned());
        let server: Vec<String> = fake.into_iter().chain(arguments).collect();
        // The server keeps its parses' cache there, not in the system's.
        let cache = scratch.join("cache");
        let env = [
            ("FAKE_DBT_SCRIPT", script_path.as_path()),
            ("FAKE_DBT_LOG", log.as_path()),
            ("XDG_CACHE_HOME", cache.as_path()),
        ];
        (Session::start(&server, &env, dir), log)
    }

    /// The same, with the fixture as --project-dir.
    pub fn fake(name: &str, script: Value) -> (Session, PathBuf) {
        Session::fake_with(name, script, &[Path::new("--project-dir"), &fixture()], None)
    }

    pub fn send(&mut self, message: &Value) {
        let mut frame = Vec::new();
        write_value(&mut frame, message).expect("frame a message");
        self.send_raw(&frame);
    }

    /// Writes BYTES to the proxy as they are, a frame or not.
    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.stdin.write_all(bytes).and_then(|()| self.stdin.flush()).expect("write to the proxy");
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The next message that PREDICATE accepts; the server's requests met on
    /// the way get an answer of nulls, as a client without configuration.
    pub fn wait_for(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.received.iter().find(|m| predicate(m)) {
            return found.clone();
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self.inbox.recv_timeout(left).unwrap_or_else(|_| {
                panic!("no awaited message within {:?}; received {:#?}", self.timeout, self.received)
            });
            assert_eq!(message["jsonrpc"], "2.0", "{message}");
            self.received.push(message.clone());
            if let (Some(id), Some(_)) = (message.get("id"), message.get("method")) {
                let items = message["params"]["items"].as_array().map(|items| vec![Value::Null; items.len()]);
                self.send(&json!({"jsonrpc": "2.0", "id": id, "result": items}));
            }
            if predicate(&message) {
                return message;
            }
        }
    }

    /// Sends a request and returns the whole response.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(self.next_id);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.wait_for(|m| m.get("id") == Some(&id) && m.get("method").is_none())
    }

    /// Sends REQUEST until ACCEPT takes the result of its answer, within
    /// DEADLINE; returns that result.
    pub fn poll(&mut self, deadline: Duration, request: (&str, Value), accept: impl Fn(&Value) -> bool) -> Value {
        let end = Instant::now() + deadline;
        loop {
            let response = self.request(request.0, request.1.clone());
            if accept(&response["result"]) {
                return response["result"].clone();
            }
            assert!(Instant::now() < end, "no accepted answer to {} within {deadline:?}: {response}", request.0);
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Initializes the session on ROOT; returns the result.
    pub fn initialize(&mut self, root: &Path) -> Value {
        self.initialize_with(root, true)
    }

    /// Initializes the session on ROOT, with or without a hierarchical
    /// outline; returns the result.
    pub fn initialize_with(&mut self, root: &Path, hierarchical: bool) -> Value {
        let capabilities =
            json!({"textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": hierarchical}}});
        self.initialize_as(root, capabilities)
    }

    /// Initializes the session on ROOT as a client with CAPABILITIES;
    /// returns the result.
    pub fn initialize_as(&mut self, root: &Path, capabilities: Value) -> Value {
        let params = json!({"processId": null, "rootUri": uri(root), "capabilities": capabilities,
                            "workspaceFolders": [{"uri": uri(root), "name": "shop"}]});
        let response = self.request("initialize", params);
        self.notify("initialized", json!({}));
        response["result"].clone()
    }

    /// Opens the file at PATH, with TEXT or its content on disk.
    pub fn open(&mut self, path: &Path, text: Option<&str>) {
        let text = text.map_or_else(|| std::fs::read_to_string(path).unwrap(), str::to_owned);
        let document = json!({"uri": uri(path), "languageId": "sql", "version": 1, "text": text});
        self.notify("textDocument/didOpen", json!({"textDocument": document}));
    }

    /// Shuts down and exits; returns the proxy's exit status.
    pub fn finish(mut self) -> ExitStatus {
        let response = self.request("shutdown", Value::Null);
        assert_eq!(response.get("result"), Some(&Value::Null), "{response}");
        self.notify("exit", Value::Null);
        self.wait()
    }

    /// Sends `exit`; returns the proxy's exit status and every message it
    /// sent that the session had not read.
    pub fn exit(mut self) -> (ExitStatus, Vec<Value>) {
        self.notify("exit", Value::Null);
        let deadline = Instant::now() + self.timeout;
        let mut unread = Vec::new();
        // Until the proxy closes its output.
        while let Ok(message) = self.inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            unread.push(message);
        }
        (self.wait(), unread)
    }

    /// Waits for the proxy to exit, within the session's timeout.
    pub fn wait(mut self) -> ExitStatus {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the proxy did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Writes at PATH an executable shell script of BODY, by a child process:
/// a file this process held open for writing, inherited by a child another
/// test thread spawns meanwhile, would fail to run with "Text file busy".
pub fn script(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut writer = Command::new("/bin/sh");
    writer.args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"]).arg(path).stdin(Stdio::piped());
    let mut child = writer.spawn().expect("start the writer of a script");
    child.stdin.take().unwrap().write_all(format!("#!/bin/sh\n{body}\n").as_bytes()).unwrap();
    assert!(child.wait().unwrap().success(), "write {path:?}");
}

/// The lines of the fake's log LOG, one JSON value each.
pub fn received_by_server(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_owned).collect()
}

/// Waits until CONDITION holds of the lines of the fake's log LOG, within
/// five seconds.
pub fn until_logged(log: &Path, what: &str, condition: impl Fn(&[String]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition(&received_by_server(log)) {
        assert!(Instant::now() < deadline, "the fake never logged {what}");
        thread::sleep(Duration::from_millis(20));
    }
}
