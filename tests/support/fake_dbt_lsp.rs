// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! A scripted stand-in for `dbt lsp`, on the standard input and output,
//! which behaves as dbt lsp 2.0.9 was seen to:
//!
//! - `lsp --help` prints a help and exits, as 2.0.9 does;
//! - it answers `initialize` with the capabilities of 2.0.9, unless the
//!   script names others, and after `initialized` asks the client for the
//!   configuration section "dbt" (request id 1);
//! - it compiles, which it announces with `dbt/lspCompileStart` then
//!   `dbt/lspCompileComplete`, after the first `didOpen`, after each
//!   `didSave`, after each `workspace/didChangeWatchedFiles` with a change
//!   (type 2) of a `.sql` file under `models/` of its root (its
//!   `--project-dir`, else `initialize`'s `rootUri`), as 2.0.9 compiled on
//!   a changed model, other types and files not observed, but for one
//!   within 1 s of a `didSave` whose every change names the file that
//!   `didSave` named, a window of the fake's own that no live test checks,
//!   and for each `dbt.listNodes`, whose answer comes between the two
//!   notifications;
//!   their params carry the keys of 2.0.9's, `compile_type` `incremental`,
//!   the completion's `errors` `[]` and `models_count` 6 by default;
//! - after each such compilation, its background one, which 2.0.9 sends
//!   about 1.1 s later and once for several: at once here, so that no test
//!   waits, `dbt/lspBackgroundCompileStart` then
//!   `dbt/lspBackgroundCompileComplete`, `compile_type` `full`, the
//!   completion's `errors` and `models_count` the foreground's; where
//!   `models_count` is null, as when 2.0.9 compiled nothing, the
//!   background completion alone, `incremental`, and a listing answered
//!   `{"error": "Compilation failed", "error_kind": "compilation_failed",
//!   ...}`;
//! - a `dbt.listNodes` cancels one still pending, which gets
//!   `{"error": "Operation cancelled", ...}`; with no scripted answer, a
//!   listing finds no node, `{"error": "No nodes found", ...}`;
//! - any other request gets the script's answer for it, else
//!   `{"echo": METHOD, "params": PARAMS}`; an inlay hint request, as 2.0.9
//!   does, `[]` on a `.sql` file and null on any other.
//!
//! Environment:
//! - `FAKE_DBT_LSP_SCRIPT`: a JSON file, optional, with the keys
//!   - `no_lsp`, true when `lsp` is refused as dbt-oss 2.0.11 refuses it,
//!     its help included, with status 2 and its message on the standard
//!     error;
//!   - `capabilities`;
//!   - `answers`, keyed by method, or by "workspace/executeCommand COMMAND
//!     ARGUMENTS..." for a command, and `answers_once`, likewise, each used
//!     for the first such request only;
//!   - `raw_answers`, likewise, each a body written as it is, `$ID` replaced
//!     by the request's id; `raw_on_initialized`, bodies sent as they are
//!     after `initialized`, and `raw_on_compile`, after the compilation
//!     that the notification `test/compile` makes, such as diagnostics,
//!     which 2.0.9 publishes with no version;
//!   - `silent`, keys whose requests are never answered, and `hold`, keys
//!     whose requests wait for the notification `test/release`;
//!   - `exit_on`, a method on which the server exits with status 3;
//!     `hang_on`, one on which it stops, neither reading nor exiting;
//!     `refuse_on`, a request it answers with an error; `orphan_on`, one on
//!     which it leaves a process that holds its output three seconds more,
//!     as a launcher's child would, and writes a `window/showMessage` "from
//!     the orphan" on it after one second; `send_on`, one before whose
//!     handling it sends the bodies listed in `send`, as they are; each only
//!     in the life that `exit_life`, `hang_life`, `refuse_life`,
//!     `orphan_life` or `send_life` numbers, from 1, when given;
//!   - `hold_compile`, true when it announces no compilation until the
//!     notification `test/compile`;
//!   - `compile_complete`, an object laid over the params of each
//!     foreground compilation's completion, which the background one
//!     copies;
//!   - for `fake-dbt-lsp parse ...`, as dbt-lsp-plus runs `dbt parse`:
//!     `manifest`, written as `manifest.json` under `--target-path`;
//!     `parse_log`, a file whose text it writes first on its standard
//!     output, as dbt's log;
//!     `parse_exit`, its status (0 by default); `parse_gate`, a file it
//!     waits for before writing; `parse_hang`, true when it never exits;
//!   - for `fake-dbt-lsp compile ...` and `list ...`, as dbt-lsp-plus runs
//!     dbt's CLI for its commands: `compiled`, the text that a compile
//!     writes for the file its `--select path:R` names, at
//!     `compile_writes` under `--target-path` (`compiled/shop/R` by
//!     default), with a manifest whose node of R in the package shop names
//!     that file as its `compiled_path`, from the working directory as dbt
//!     does when it can, or always, through `..`, when `compiled_relative`
//!     is true; `list`, values that a list prints,
//!     one JSON line each; `run_log`, text that either prints first, as
//!     dbt's log; `run_exit`, the status of either; `run_gate`, a file
//!     either waits for first; `run_hang`, true when either never exits;
//! - `FAKE_DBT_LSP_LOG`: a file to which the body of each message received is
//!   appended as one line; a parse appends `{"parse": ARGUMENTS, "pid": PID,
//!   "interfaces": NAMES, "wrote": WROTE}`, NAMES those of the network it
//!   can reach, WROTE whether it could write into its `--project-dir` when
//!   the script says `parse_write`,
//!   to that file's name followed by `.parses`, since two processes
//!   appending to one file could interleave their lines, and a compile or a
//!   list `{"run": ARGUMENTS, "pid": PID, "interfaces": NAMES}` to its name
//!   followed by `.runs`, and `{"ended": PID}` there as it exits; and each
//!   server
//!   process, a life of the fake, appends its pid to the name followed by
//!   `.lives`, by which it knows its life; and every process of the fake,
//!   whatever it does, `{"argv0": ARGV0, "arguments": ARGUMENTS, "cwd":
//!   DIR}` to the name followed by `.starts`, as it starts.

use dbt_lsp_plus::rpc;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LISTING: &str = "workspace/executeCommand dbt.listNodes";
/// How soon after a `didSave` 2.0.9's compilation of it covers a watched
/// event of its file: it made one compilation of the two back to back, two
/// of them 6 s apart.
const SAVE_WINDOW: Duration = Duration::from_secs(1);

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

/// The params of a compilation's start of COMPILE_TYPE, as 2.0.9 sends
/// them: its `cause` is `didSave` whatever the trigger.
fn compile_params(compile_type: &str) -> Value {
    json!({"adapter_type": "duckdb", "adapter_unique_id": "8ad8a8320c59b06e1dadab91e0b2cfee", "cause": "didSave",
           "compile_type": compile_type, "dbt_version": "2.0.9", "project_id": "fb54f3c5992b96d001bb16e8e92d968d"})
}

fn compile_start() {
    send(json!({"method": "dbt/lspCompileStart", "params": compile_params("incremental")}));
}

/// The params of a foreground compilation's completion: the script's laid
/// over 2.0.9's.
fn completion_params(script: &Value) -> Value {
    let mut params = compile_params("incremental");
    params["duration_ms"] = json!(1);
    params["errors"] = json!([]);
    params["models_count"] = json!(6);
    for (key, value) in script["compile_complete"].as_object().into_iter().flatten() {
        params[key.as_str()] = value.clone();
    }
    params
}

/// The foreground compilation's completion, then the background
/// compilation that follows it.
fn compile_complete(script: &Value) {
    let mut params = completion_params(script);
    send(json!({"method": "dbt/lspCompileComplete", "params": params.clone()}));
    if !params["models_count"].is_null() {
        send(json!({"method": "dbt/lspBackgroundCompileStart", "params": compile_params("full")}));
        params["compile_type"] = json!("full");
    }
    send(json!({"method": "dbt/lspBackgroundCompileComplete", "params": params}));
}

/// What 2.0.9 answers to METHOD with PARAMS, a listing when LISTING, in a
/// project that compiles nothing when FAILED, when the script says nothing
/// of it.
fn answered_as_2_0_9(method: &str, listing: bool, failed: bool, params: Value) -> Value {
    let model = params.pointer("/textDocument/uri").and_then(Value::as_str).is_some_and(|uri| uri.ends_with(".sql"));
    match (method, listing) {
        ("shutdown", _) => Value::Null,
        (_, true) if failed => json!({"error": "Compilation failed", "error_kind": "compilation_failed",
                                      "grain": "unknown", "nodes": [], "root_project": null}),
        (_, true) => json!({"error": "No nodes found", "grain": "unknown", "nodes": []}),
        // Inlay hints: none on a model, null on any other file.
        ("textDocument/inlayHint", _) if model => json!([]),
        ("textDocument/inlayHint", _) => Value::Null,
        _ => json!({"echo": method, "params": params}),
    }
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
    /// Which process of the session this is, from 1.
    life: usize,
    /// The project: its `--project-dir`, else `initialize`'s `rootUri`.
    root: Option<PathBuf>,
    /// The URI that the last `didSave` named, and when it came.
    saved: Option<(Value, Instant)>,
}

impl Fake {
    /// Whether the script makes this life ACT (`exit` or `hang`) on METHOD.
    fn acts_on(&self, act: &str, method: &str) -> bool {
        let life = self.script[format!("{act}_life")].as_u64().is_none_or(|life| life as usize == self.life);
        self.script[format!("{act}_on")].as_str() == Some(method) && life
    }

    /// What the script makes this life do on METHOD, a request when ID,
    /// before or in place of its answer: send bodies, leave an orphan, exit,
    /// hang, or refuse; true when it refused.
    fn misbehaves(&self, method: &str, id: Option<&Value>) -> bool {
        if self.acts_on("send", method) {
            self.script["send"].as_array().into_iter().flatten().filter_map(Value::as_str).for_each(send_raw);
        }
        if self.acts_on("orphan", method) {
            // The shell's background child inherits the output and outlives it.
            let message = json!({"type": 3, "message": "from the orphan"});
            let body = json!({"jsonrpc": "2.0", "method": "window/showMessage", "params": message}).to_string();
            let script =
                format!("(sleep 1; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{body}'; sleep 2) &", body.len());
            std::process::Command::new("sh").args(["-c", &script]).status().expect("leave an orphan");
        }
        if self.acts_on("exit", method) {
            std::process::exit(3);
        }
        if self.acts_on("hang", method) {
            loop {
                std::thread::park();
            }
        }
        let refused = self.acts_on("refuse", method) && id.is_some();
        if refused {
            send(json!({"id": id, "error": {"code": -32603, "message": "refused by the script"}}));
        }
        refused
    }

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
        let answer = once.or_else(|| self.script["answers"].get(key).cloned());
        let failed = completion_params(&self.script)["models_count"].is_null();
        let answer = answer.unwrap_or_else(|| answered_as_2_0_9(method, listing, failed, params));
        let compiling = listing && self.script["hold_compile"] != json!(true);
        if compiling {
            compile_start();
        }
        send(json!({"id": id, "result": answer}));
        if compiling {
            compile_complete(&self.script);
        }
    }

    /// Whether the client's notification METHOD, of PARAMS, makes 2.0.9
    /// compile, apart from what the script makes the fake do.
    fn compiles_as_2_0_9(&mut self, method: &str, params: &Value) -> bool {
        let now = Instant::now();
        match method {
            "textDocument/didOpen" => !std::mem::replace(&mut self.opened, true),
            "textDocument/didSave" => {
                self.saved = Some((params["textDocument"]["uri"].clone(), now));
                true
            }
            "workspace/didChangeWatchedFiles" => {
                let changes = params["changes"].as_array().map_or(&[][..], Vec::as_slice);
                let saved = self.saved.as_ref().filter(|(_, at)| now.duration_since(*at) < SAVE_WINDOW);
                let covered = saved.is_some_and(|(uri, _)| changes.iter().all(|change| change["uri"] == *uri));
                // A model changed, which 2.0.9 was seen to compile on.
                let model = |change: &Value| {
                    let path = change["uri"].as_str().and_then(|uri| url::Url::parse(uri).ok()?.to_file_path().ok());
                    let sql =
                        path.as_ref().is_some_and(|path| path.extension().is_some_and(|extension| extension == "sql"));
                    let root = self.root.as_deref();
                    let under = path.is_some_and(|path| root.is_some_and(|root| path.starts_with(root.join("models"))));
                    change["type"] == 2 && sql && under
                };
                !covered && changes.iter().any(model)
            }
            _ => false,
        }
    }

    /// Acts on the notification METHOD, of PARAMS.
    fn notified(&mut self, method: &str, params: &Value) {
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
            "test/release" => {
                for (id, method, params) in std::mem::take(&mut self.held) {
                    let key = answer_key(&method, &params);
                    self.answer(&method, &id, params, &key);
                }
                false
            }
            "test/compile" => {
                compile_start();
                compile_complete(&self.script);
                self.script["raw_on_compile"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .for_each(send_raw);
                false
            }
            "exit" => std::process::exit(0),
            _ => self.compiles_as_2_0_9(method, params),
        };
        if compiles && self.script["hold_compile"] != json!(true) {
            compile_start();
            compile_complete(&self.script);
        }
    }
}

/// `dbt lsp ARGUMENTS` answered without serving, exiting: refused when the
/// script says `no_lsp`, as dbt-oss 2.0.11 refuses it, its help included;
/// else its help printed, as 2.0.9 does, when asked for it.  A server is
/// refused only once COUNTED as a life, so that a test sees it started.
fn refuse_or_help(script: &Value, arguments: &[String], counted: bool) {
    let help = arguments.iter().any(|argument| argument == "--help");
    if script["no_lsp"] == json!(true) && (help || counted) {
        eprintln!("error: unrecognized subcommand 'lsp'\n\nUsage: dbt [OPTIONS] <COMMAND>");
        std::process::exit(2);
    }
    if help {
        println!("Start the dbt language server");
        std::process::exit(0);
    }
}

/// `dbt parse` as the script plays it, with the arguments ARGUMENTS.
fn parse(script: &Value, arguments: &[String], log: Option<File>) {
    let project = arguments.iter().position(|argument| argument == "--project-dir").map(|at| &arguments[at + 1]);
    let probe = project.filter(|_| script["parse_write"] == json!(true)).map(|dir| Path::new(dir).join(".written"));
    // Removed at once, so that a parse that could write leaves nothing.
    let wrote =
        probe.is_some_and(|probe| std::fs::write(&probe, "").and_then(|()| std::fs::remove_file(&probe)).is_ok());
    if let Some(mut log) = log {
        let line = json!({"parse": arguments, "pid": std::process::id(), "interfaces": interfaces(), "wrote": wrote});
        log.write_all(format!("{line}\n").as_bytes()).expect("write the log");
    }
    if let Some(log) = script["parse_log"].as_str() {
        print!("{}", std::fs::read_to_string(log).expect("read the parse's log"));
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

/// The names of the network interfaces this process can reach.
fn interfaces() -> Vec<String> {
    let devices = std::fs::read_to_string("/proc/self/net/dev").unwrap_or_default();
    devices.lines().skip(2).filter_map(|line| Some(line.split_once(':')?.0.trim().to_owned())).collect()
}

/// The value of FLAG in ARGUMENTS, the word after it.
fn flag<'a>(arguments: &'a [String], flag: &str) -> Option<&'a str> {
    arguments.iter().position(|argument| argument == flag).map(|at| arguments[at + 1].as_str())
}

/// `dbt compile` or `dbt list`, ARGUMENTS' first word, as the script plays
/// it.
fn run(script: &Value, arguments: &[String], mut log: Option<File>) {
    if let Some(log) = &mut log {
        let line = json!({"run": arguments, "pid": std::process::id(), "interfaces": interfaces()});
        log.write_all(format!("{line}\n").as_bytes()).expect("write the log");
    }
    if let Some(gate) = script["run_gate"].as_str() {
        let end = Instant::now() + Duration::from_secs(30);
        while !Path::new(gate).exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    while script["run_hang"] == json!(true) {
        std::thread::park();
    }
    print!("{}", script["run_log"].as_str().unwrap_or_default());
    let selected = flag(arguments, "--select").and_then(|selector| selector.strip_prefix("path:"));
    if let (Some(target), Some(relative), Some(text)) =
        (flag(arguments, "--target-path"), selected, script["compiled"].as_str())
    {
        let written =
            script["compile_writes"].as_str().map_or_else(|| format!("compiled/shop/{relative}"), str::to_owned);
        let file = Path::new(target).join(written);
        std::fs::create_dir_all(file.parent().unwrap()).expect("create the compiled file's directory");
        std::fs::write(&file, text).expect("write the compiled file");
        let cwd = std::env::current_dir().expect("the working directory");
        let compiled_path = match script["compiled_relative"] == json!(true) {
            true => {
                // Up from the working directory to the first directory both share, then down.
                let shared = cwd.components().zip(file.components()).take_while(|(one, other)| one == other).count();
                let up = (shared..cwd.components().count()).map(|_| Path::new(".."));
                up.chain(file.components().skip(shared).map(|part| Path::new(part.as_os_str()))).collect()
            }
            false => file.strip_prefix(&cwd).unwrap_or(&file).to_owned(),
        };
        let node = json!({"original_file_path": relative, "package_name": "shop", "compiled_path": compiled_path});
        let manifest = json!({"nodes": {"model.shop.compiled": node}});
        std::fs::write(Path::new(target).join("manifest.json"), manifest.to_string()).expect("write the manifest");
    }
    let listing = arguments.first().map(String::as_str) == Some("list");
    for node in script["list"].as_array().into_iter().flatten().filter(|_| listing) {
        println!("{node}");
    }
    if let Some(log) = &mut log {
        log.write_all(format!("{}\n", json!({"ended": std::process::id()})).as_bytes()).expect("write the log");
    }
    std::process::exit(script["run_exit"].as_i64().unwrap_or(0) as i32);
}

fn main() {
    let script: Value = std::env::var("FAKE_DBT_LSP_SCRIPT")
        .ok()
        .map(|path| {
            serde_json::from_str(&std::fs::read_to_string(path).expect("read the script")).expect("parse the script")
        })
        .unwrap_or(json!({}));
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let verb = arguments.first().map_or("", String::as_str);
    if let Ok(log) = std::env::var("FAKE_DBT_LSP_LOG") {
        let cwd = std::env::current_dir().expect("the working directory");
        let start = json!({"argv0": std::env::args().next(), "arguments": arguments, "cwd": cwd});
        let mut starts =
            OpenOptions::new().create(true).append(true).open(format!("{log}.starts")).expect("open the starts");
        // One write, so that the lines of two processes never interleave.
        starts.write_all(format!("{start}\n").as_bytes()).expect("log the start");
    }
    let mut log = std::env::var("FAKE_DBT_LSP_LOG").ok().map(|path| {
        let path = match verb {
            "parse" => format!("{path}.parses"),
            "compile" | "list" => format!("{path}.runs"),
            _ => path,
        };
        OpenOptions::new().create(true).append(true).open(path).expect("open the log")
    });
    match verb {
        "parse" => return parse(&script, &arguments, log),
        "compile" | "list" => return run(&script, &arguments, log),
        _ => {}
    }
    refuse_or_help(&script, &arguments, false);
    // Each server process of a session is one life of the fake.
    let life = std::env::var("FAKE_DBT_LSP_LOG").map_or(1, |log| {
        let lives = format!("{log}.lives");
        let mut file = OpenOptions::new().create(true).append(true).open(&lives).expect("open the lives");
        file.write_all(format!("{}\n", std::process::id()).as_bytes()).expect("count a life");
        std::fs::read_to_string(&lives).expect("read the lives").lines().count()
    });
    refuse_or_help(&script, &arguments, true);
    let root = flag(&arguments, "--project-dir").map(PathBuf::from);
    let mut fake = Fake { script, opened: false, held: Vec::new(), life, root, saved: None };
    let mut input = BufReader::new(io::stdin());
    while let Ok(Some(frame)) = rpc::read_frame(&mut input) {
        if let Some(log) = &mut log {
            log.write_all(&frame.body).and_then(|()| log.write_all(b"\n")).expect("write the log");
        }
        let message = frame.value.unwrap_or(Value::Null);
        let (Some(method), id) = (message["method"].as_str(), message.get("id")) else { continue };
        if fake.misbehaves(method, id) {
            continue;
        }
        match (method, id) {
            ("initialize", Some(id)) => {
                let root_uri = message.pointer("/params/rootUri").and_then(Value::as_str);
                let given = root_uri.and_then(|uri| url::Url::parse(uri).ok()?.to_file_path().ok());
                fake.root = fake.root.take().or(given);
                let capabilities = fake.script.get("capabilities").cloned().unwrap_or_else(capabilities_of_2_0_9);
                let info = json!({"name": "dbt-lsp", "version": "2.0.9"});
                send(json!({"id": id, "result": {"capabilities": capabilities, "serverInfo": info}}));
            }
            (method, Some(id)) => fake.respond(method, id, message.get("params").cloned().unwrap_or(Value::Null)),
            (method, None) => fake.notified(method, message.get("params").unwrap_or(&Value::Null)),
        }
    }
}
