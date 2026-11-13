// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! A scripted stand-in for `dbt lsp`, on the standard input and output,
//! which behaves as dbt lsp 2.0.9 was seen to:
//!
//! - it answers `initialize` with the capabilities of 2.0.9, unless the
//!   script names others, and after `initialized` asks the client for the
//!   configuration section "dbt" (request id 1);
//! - it compiles, which it announces with `dbt/lspCompileStart` then
//!   `dbt/lspCompileComplete`, after the first `didOpen`, after each
//!   `didSave`, and for each `dbt.listNodes`, whose answer comes between the
//!   two notifications;
//! - a `dbt.listNodes` cancels one still pending, which gets
//!   `{"error": "Operation cancelled", ...}`; with no scripted answer, a
//!   listing finds no node, `{"error": "No nodes found", ...}`;
//! - any other request gets the script's answer for it, else
//!   `{"echo": METHOD, "params": PARAMS}`.
//!
//! Environment:
//! - `FAKE_DBT_LSP_SCRIPT`: a JSON file, optional, with the keys
//!   - `capabilities`;
//!   - `answers`, keyed by method, or by "workspace/executeCommand COMMAND
//!     ARGUMENTS..." for a command, and `answers_once`, likewise, each used
//!     for the first such request only;
//!   - `raw_answers`, likewise, each a body written as it is, `$ID` replaced
//!     by the request's id; `raw_on_initialized`, bodies sent as they are
//!     after `initialized`;
//!   - `silent`, keys whose requests are never answered, and `hold`, keys
//!     whose requests wait for the notification `test/release`;
//!   - `exit_on`, a method on which the server exits with status 3, and
//!     `hang_on`, one on which it stops, neither reading nor exiting;
//!   - `hold_compile`, true when it announces no compilation until the
//!     notification `test/compile`;
//!   - for `fake-dbt-lsp parse ...`, as dbt-lsp-plus runs `dbt parse`:
//!     `manifest`, written as `manifest.json` under `--target-path`;
//!     `parse_exit`, its status (0 by default); `parse_gate`, a file it
//!     waits for before writing; `parse_hang`, true when it never exits;
//! - `FAKE_DBT_LSP_LOG`: a file to which the body of each message received is
//!   appended as one line; a parse appends `{"parse": ARGUMENTS, "pid": PID}`
//!   to that file's name followed by `.parses`, since two processes
//!   appending to one file could interleave their lines.

use dbt_lsp_plus::rpc;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const LISTING: &str = "workspace/executeCommand dbt.listNodes";

fn capabilities_of_2_0_9() -> Value {
    json!({
        "codeActionProvider": {"codeActionKinds": ["source.fixAll", "source.fixAll.dbtLintFix"]},
        "codeLensProvider": {"resolveProvider": false},
        "completionProvider": {"completionItem": {"labelDetailsSupport": true}, "resolveProvider": false,
                               "triggerCharacters": ["{", "'", "."]},
        "definitionProvider": true,
        "documentFormattingProvider": true,
        "documentSymbolProvider": false,
        "executeCommandProvider": {"commands": ["dbt.listNodes", "dbt.getCurrentNode", "dbt.compileFile",
                                                "dbt.compileLsp", "dbt.clearTarget", "dbt.getProjectInfo", "dbt.show"]},
        "hoverProvider": true,
        "inlayHintProvider": true,
        "referencesProvider": true,
        "renameProvider": true,
        "textDocumentSync": {"change": 2, "openClose": true, "save": {}}
    })
}

fn send(message: Value) {
    let mut message = message;
    message["jsonrpc"] = json!("2.0");
    rpc::write_value(&mut io::stdout().lock(), &message).expect("write to the proxy");
}

fn send_raw(body: &str) {
    rpc::write_body(&mut io::stdout().lock(), body.as_bytes()).expect("write to the proxy");
}

fn compile_start() {
    send(json!({"method": "dbt/lspCompileStart", "params": {"cause": "didSave"}}));
}

fn compile_complete() {
    send(json!({"method": "dbt/lspCompileComplete", "params": {"cause": "didSave", "errors": []}}));
}

/// The key under which the script holds the answer to METHOD with PARAMS.
fn answer_key(method: &str, params: &Value) -> String {
    if method != "workspace/executeCommand" {
        return method.to_owned();
    }
    let arguments =
        params["arguments"].as_array().map_or(Vec::new(), |args| args.iter().filter_map(Value::as_str).collect());
    format!("{method} {} {}", params["command"].as_str().unwrap_or_default(), arguments.join(" "))
}

struct Fake {
    script: Value,
    opened: bool,
    /// Held requests: id, method, params.
    held: Vec<(Value, String, Value)>,
}

impl Fake {
    fn lists(&self, list: &str, key: &str) -> bool {
        self.script[list].as_array().is_some_and(|keys| keys.iter().any(|k| k == key))
    }

    /// Answers the request METHOD with id ID, unless the script holds or silences it.
    fn respond(&mut self, method: &str, id: &Value, params: Value) {
        let key = answer_key(method, &params);
        if key.starts_with(LISTING)
            && let Some(at) = self.held.iter().position(|(_, m, p)| answer_key(m, p).starts_with(LISTING))
        {
            let (held, ..) = self.held.remove(at);
            send(json!({"id": held, "result": {"error": "Operation cancelled", "grain": "unknown", "nodes": []}}));
        }
        if self.lists("hold", &key) {
            return self.held.push((id.clone(), method.to_owned(), params));
        }
        if !self.lists("silent", &key) {
            self.answer(method, id, params, &key);
        }
    }

    fn answer(&mut self, method: &str, id: &Value, params: Value, key: &str) {
        if let Some(raw) = self.script["raw_answers"][key].as_str() {
            return send_raw(&raw.replace("$ID", &id.to_string()));
        }
        let once = self.script.get_mut("answers_once").and_then(Value::as_object_mut).and_then(|a| a.remove(key));
        let listing = key.starts_with(LISTING);
        let default = match (method, listing) {
            ("shutdown", _) => Value::Null,
            (_, true) => json!({"error": "No nodes found", "grain": "unknown", "nodes": []}),
            _ => json!({"echo": method, "params": params}),
        };
        let answer = once.or_else(|| self.script["answers"].get(key).cloned()).unwrap_or(default);
        let compiling = listing && self.script["hold_compile"] != json!(true);
        if compiling {
            compile_start();
        }
        send(json!({"id": id, "result": answer}));
        if compiling {
            compile_complete();
        }
    }

    /// Acts on the notification METHOD.
    fn notified(&mut self, method: &str) {
        let compiles = match method {
            "initialized" => {
                send(json!({"id": 1, "method": "workspace/configuration", "params": {"items": [{"section": "dbt"}]}}));
                self.script["raw_on_initialized"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .for_each(send_raw);
                false
            }
            "textDocument/didOpen" => !std::mem::replace(&mut self.opened, true),
            "test/release" => {
                for (id, method, params) in std::mem::take(&mut self.held) {
                    let key = answer_key(&method, &params);
                    self.answer(&method, &id, params, &key);
                }
                false
            }
            "test/compile" => {
                compile_start();
                compile_complete();
                false
            }
            "exit" => std::process::exit(0),
            _ => method == "textDocument/didSave",
        };
        if compiles && self.script["hold_compile"] != json!(true) {
            compile_start();
            compile_complete();
        }
    }
}

/// `dbt parse` as the script plays it, with the arguments ARGUMENTS.
fn parse(script: &Value, arguments: &[String], log: Option<File>) {
    if let Some(mut log) = log {
        let line = json!({"parse": arguments, "pid": std::process::id()});
        writeln!(log, "{line}").expect("write the log");
    }
    // dbt writes on both outputs, which must never reach the client: this
    // noise is a frame whose body is not JSON, which a client would show.
    print!("Content-Length: 5\r\n\r\nnoise");
    eprintln!("noise on stderr");
    if let Some(gate) = script["parse_gate"].as_str() {
        let end = Instant::now() + Duration::from_secs(30);
        while !Path::new(gate).exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    while script["parse_hang"] == json!(true) {
        std::thread::park();
    }
    let target = arguments.iter().position(|argument| argument == "--target-path").map(|at| &arguments[at + 1]);
    if let (Some(target), Some(manifest)) = (target, script.get("manifest")) {
        std::fs::create_dir_all(target).expect("create the target");
        std::fs::write(Path::new(target).join("manifest.json"), manifest.to_string()).expect("write the manifest");
    }
    std::process::exit(script["parse_exit"].as_i64().unwrap_or(0) as i32);
}

fn main() {
    let script: Value = std::env::var("FAKE_DBT_LSP_SCRIPT")
        .ok()
        .map(|path| {
            serde_json::from_str(&std::fs::read_to_string(path).expect("read the script")).expect("parse the script")
        })
        .unwrap_or(json!({}));
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let parsing = arguments.first().map(String::as_str) == Some("parse");
    let mut log = std::env::var("FAKE_DBT_LSP_LOG").ok().map(|path| {
        let path = if parsing { format!("{path}.parses") } else { path };
        OpenOptions::new().create(true).append(true).open(path).expect("open the log")
    });
    if parsing {
        return parse(&script, &arguments, log);
    }
    let mut fake = Fake { script, opened: false, held: Vec::new() };
    let mut input = BufReader::new(io::stdin());
    while let Ok(Some(frame)) = rpc::read_frame(&mut input) {
        if let Some(log) = &mut log {
            log.write_all(&frame.body).and_then(|()| log.write_all(b"\n")).expect("write the log");
        }
        let message = frame.value.unwrap_or(Value::Null);
        let (Some(method), id) = (message["method"].as_str(), message.get("id")) else { continue };
        if fake.script["exit_on"].as_str() == Some(method) {
            std::process::exit(3);
        }
        if fake.script["hang_on"].as_str() == Some(method) {
            loop {
                std::thread::park();
            }
        }
        match (method, id) {
            ("initialize", Some(id)) => {
                let capabilities = fake.script.get("capabilities").cloned().unwrap_or_else(capabilities_of_2_0_9);
                let info = json!({"name": "dbt-lsp", "version": "2.0.9"});
                send(json!({"id": id, "result": {"capabilities": capabilities, "serverInfo": info}}));
            }
            (method, Some(id)) => fake.respond(method, id, message.get("params").cloned().unwrap_or(Value::Null)),
            (method, None) => fake.notified(method),
        }
    }
}
