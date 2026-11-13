// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! A client of dbt-lsp-plus for the tests: it starts the proxy in front of a
//! server command and speaks LSP to it.

#![allow(dead_code)]

use dbt_lsp_plus::rpc;
use serde_json::{Value, json};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shop")
}

/// A fresh directory for test NAME under Cargo's temporary directory.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

pub fn uri(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    inbox: Receiver<(Vec<u8>, Value)>,
    next_id: i64,
    pub timeout: Duration,
    /// Every message the client received, in order, parsed.
    pub received: Vec<Value>,
    /// The same, as their bodies arrived.
    pub received_raw: Vec<Vec<u8>>,
    /// Whether the session answers the server's requests by itself.
    pub auto_answer: bool,
}

impl Session {
    /// Starts the proxy in front of SERVER, in DIR when given, from an empty
    /// environment but for PATH and ENV.
    pub fn start(server: &[String], env: &[(&str, &Path)], dir: Option<&Path>) -> Session {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dbt-lsp-plus"));
        command.arg("--").args(server).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
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
            while let Ok(Some(frame)) = rpc::read_frame(&mut stdout) {
                if sender.send((frame.body, frame.value.expect("the proxy sends JSON"))).is_err() {
                    return;
                }
            }
        });
        Session {
            child,
            stdin,
            inbox,
            next_id: 100,
            timeout: Duration::from_secs(10),
            received: Vec::new(),
            received_raw: Vec::new(),
            auto_answer: true,
        }
    }

    /// Starts the proxy in front of the fake server, with SCRIPT and the
    /// --project-dir PROJECT; returns the session and the fake's log.
    pub fn fake_in(name: &str, script: Value, project: &Path) -> (Session, PathBuf) {
        let dir = scratch(name);
        let (script_path, log) = (dir.join("script.json"), dir.join("log.jsonl"));
        std::fs::write(&script_path, script.to_string()).unwrap();
        let server = [env!("CARGO_BIN_EXE_fake-dbt-lsp"), "lsp", "--project-dir"].map(String::from);
        let server = [&server[..], &[project.to_string_lossy().into_owned()]].concat();
        let env = [("FAKE_DBT_LSP_SCRIPT", script_path.as_path()), ("FAKE_DBT_LSP_LOG", log.as_path())];
        (Session::start(&server, &env, None), log)
    }

    /// The same, on the fixture.
    pub fn fake(name: &str, script: Value) -> (Session, PathBuf) {
        Session::fake_in(name, script, &fixture())
    }

    pub fn send(&mut self, message: &Value) {
        rpc::write_value(&mut self.stdin, message).expect("write to the proxy");
    }

    pub fn send_raw(&mut self, body: &[u8]) {
        rpc::write_body(&mut self.stdin, body).expect("write to the proxy");
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
            let (body, message) = self.inbox.recv_timeout(left).unwrap_or_else(|_| {
                panic!("no awaited message within {:?}; received {:#?}", self.timeout, self.received)
            });
            self.received.push(message.clone());
            self.received_raw.push(body);
            if let (true, Some(id), Some(_)) = (self.auto_answer, message.get("id"), message.get("method")) {
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
        assert_eq!(response["result"], Value::Null, "{response}");
        self.notify("exit", Value::Null);
        self.wait()
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

/// The bodies the fake server received, one JSON value per line.
pub fn received_by_server(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_owned).collect()
}

/// Waits until CONDITION holds of the fake's log, within five seconds.
pub fn until_logged(log: &Path, what: &str, condition: impl Fn(&[String]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition(&received_by_server(log)) {
        assert!(Instant::now() < deadline, "the fake server never logged {what}");
        thread::sleep(Duration::from_millis(20));
    }
}
