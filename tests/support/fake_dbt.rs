// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! A scripted stand-in for dbt, as dbt-lsp-plus runs it: `lsp`, its probe
//! `lsp --help` included, refused as dbt-oss 2.0.11 refuses it, with status
//! 2 and its message on the standard error; `parse`, `compile` and `list` as
//! the script plays them.
//!
//! Environment:
//! - `FAKE_DBT_SCRIPT`: a JSON file, optional, with the keys
//!   - for `fake-dbt parse ...`, as dbt-lsp-plus runs `dbt parse`:
//!     `manifest`, written as `manifest.json` under `--target-path`;
//!     `parse_log`, a file whose text it writes first on its standard
//!     output, as dbt's log;
//!     `parse_exit`, its status (0 by default); `parse_gate`, a file it
//!     waits for before writing; `parse_hang`, true when it never exits;
//!   - for `fake-dbt compile ...` and `list ...`, as dbt-lsp-plus runs
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
//! - `FAKE_DBT_LOG`: a file name, to which a parse appends `{"parse":
//!   ARGUMENTS, "pid": PID}` followed by `.parses`, since two processes
//!   appending to one file could interleave their lines, and a compile or a
//!   list `{"run": ARGUMENTS, "pid": PID}` followed by `.runs`, and
//!   `{"ended": PID}` there as it exits; and every process of the fake,
//!   whatever it does, `{"argv0": ARGV0, "arguments": ARGUMENTS, "cwd":
//!   DIR}` followed by `.starts`, as it starts.

use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

/// `dbt parse` as the script plays it, with the arguments ARGUMENTS.
fn parse(script: &Value, arguments: &[String], log: Option<File>) {
    if let Some(mut log) = log {
        let line = json!({"parse": arguments, "pid": std::process::id()});
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

/// The value of FLAG in ARGUMENTS, the word after it.
fn flag<'a>(arguments: &'a [String], flag: &str) -> Option<&'a str> {
    arguments.iter().position(|argument| argument == flag).map(|at| arguments[at + 1].as_str())
}

/// `dbt compile` or `dbt list`, ARGUMENTS' first word, as the script plays
/// it.
fn run(script: &Value, arguments: &[String], mut log: Option<File>) {
    if let Some(log) = &mut log {
        let line = json!({"run": arguments, "pid": std::process::id()});
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
    let script: Value = std::env::var("FAKE_DBT_SCRIPT")
        .ok()
        .map(|path| {
            serde_json::from_str(&std::fs::read_to_string(path).expect("read the script")).expect("parse the script")
        })
        .unwrap_or(json!({}));
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let verb = arguments.first().map_or("", String::as_str);
    let log = std::env::var("FAKE_DBT_LOG").ok();
    if let Some(log) = &log {
        let cwd = std::env::current_dir().expect("the working directory");
        let start = json!({"argv0": std::env::args().next(), "arguments": arguments, "cwd": cwd});
        let mut starts =
            OpenOptions::new().create(true).append(true).open(format!("{log}.starts")).expect("open the starts");
        // One write, so that the lines of two processes never interleave.
        starts.write_all(format!("{start}\n").as_bytes()).expect("log the start");
    }
    let opened = |suffix: &str| {
        let path = log.as_ref().map(|log| format!("{log}{suffix}"));
        path.map(|path| OpenOptions::new().create(true).append(true).open(path).expect("open the log"))
    };
    match verb {
        "parse" => parse(&script, &arguments, opened(".parses")),
        "compile" | "list" => run(&script, &arguments, opened(".runs")),
        _ => {
            eprintln!("error: unrecognized subcommand '{verb}'\n\nUsage: dbt [OPTIONS] <COMMAND>");
            std::process::exit(2);
        }
    }
}
