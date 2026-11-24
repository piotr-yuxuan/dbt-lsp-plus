// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The command line: the server's arguments, and the subcommands,
//! `doctor`, `select`, `node` and `lineage`, what an agent in a shell asks
//! without an LSP client, each answered on one line of compact JSON; and
//! `mcp`, which serves the rows that are tools, node and lineage, to an MCP
//! client.  The project is
//! found on the server's routes by `locate::project`, the CLI's own; its
//! dbt by `locate::dbt_of`, which the server shares.

use crate::locate;
use crate::text::{self, Encoding, LineStarts};
use crate::workspace::{self, Answer, Which, Workspace};
use crate::{cache, commands, config, packages};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;

/// A subcommand, and the MCP tool of the same name where it is one: its
/// run, from the arguments that its words or a tool's call give and the
/// server command, gives its answer and whether it worked; an answer of
/// null, mcp's, writes nothing, the run having written its own lines.
pub struct Row {
    name: &'static str,
    usage: &'static str,
    /// The operand's name among the arguments, and, without one, its
    /// default, else what it needs, said in the usage error.
    operand: (&'static str, Result<&'static str, &'static str>),
    /// The option of a whole number, `--NAME N`, after the operand.
    option: Option<&'static str>,
    /// The tool's description, for a row that MCP serves.
    tool: Option<&'static str>,
    run: fn(&Value, &[String]) -> (Value, bool),
}

const SUBCOMMANDS: [Row; 5] = [
    Row {
        name: "doctor",
        usage: "\
usage: dbt-lsp-plus doctor [PATH] [-- ARGUMENTS...]

Which project and dbt the server would find from PATH, on one line of JSON.
Exit status 0 when it finds a dbt 2, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: ("path", Ok(".")),
        option: None,
        tool: None,
        run: |arguments, command| doctor(Path::new(arguments["path"].as_str().unwrap_or_default()), command),
    },
    Row {
        name: "select",
        usage: "\
usage: dbt-lsp-plus select SELECTOR [-- ARGUMENTS...]

The nodes that SELECTOR, a dbt selector, selects, on one line of JSON.
Exit status 0 when nodes are listed, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: ("selector", Err("a SELECTOR")),
        option: None,
        tool: None,
        run: |arguments, command| by_error(select(arguments["selector"].as_str().unwrap_or_default(), command)),
    },
    Row {
        name: "node",
        usage: "\
usage: dbt-lsp-plus node ID|PATH[:LINE[:COLUMN]] [-- ARGUMENTS...]

What nodeInfo gives of the node that ID, or a place in PATH, names, on one
line of JSON; LINE and COLUMN from 1, COLUMN in characters.
Exit status 0 when `error` is null, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: ("node", Err("an ID or a PATH")),
        option: None,
        tool: Some(
            "The node of the dbt project that `node` names, as dbt's manifest gives it, with `summary` and `children`, the unique ids of the enabled nodes that depend on it.  `node` is a unique id, as `model.shop.orders`, or a place `PATH[:LINE[:COLUMN]]`, PATH from the server's directory, LINE and COLUMN from 1.  Answers {\"error\": E, \"node\": N}: N null and E why when there is none; E the last parse's failure beside N.  A call after a change to the project's files waits for a dbt parse.",
        ),
        run: |arguments, command| by_error(node(arguments, command)),
    },
    Row {
        name: "lineage",
        usage: "\
usage: dbt-lsp-plus lineage ID [--depth N] [-- ARGUMENTS...]

The nodes up and down from the node ID, within N hops, every hop by default,
on one line of JSON.
Exit status 0 when `error` is null, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: ("id", Err("an ID")),
        option: Some("depth"),
        tool: Some(
            "The nodes up and down from the node `id`, a unique id, within `depth` hops, from 1, every hop without it: `upstream` along `depends_on.nodes`, `downstream` along the nodes that depend on it, tests aside, each a map of unique id to its fewest hops.  Answers {\"error\": E, \"lineage\": {\"upstream\": U, \"downstream\": D}}: the lineage null and E why when there is none.  A call after a change to the project's files waits for a dbt parse.",
        ),
        run: |arguments, command| by_error(lineage(arguments, command)),
    },
    Row {
        name: "mcp",
        usage: "\
usage: dbt-lsp-plus mcp [PATH] [-- ARGUMENTS...]

An MCP server on the standard input and output, run from PATH, `.` by
default, its tools node and lineage, the subcommands' answers.
Exit status 0 when its input ends, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: ("path", Ok(".")),
        option: None,
        tool: None,
        run: |arguments, command| mcp(arguments["path"].as_str().unwrap_or_default(), command),
    },
];

/// The MCP revisions that `initialize` agrees on, the latest first.
const VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2024-11-05"];

/// The subcommand that WORDS, the command line after the program's name,
/// name by their first word; none when it names none, and the words are the
/// server's.
pub fn named(words: &[String]) -> Option<&'static Row> {
    let first = words.first()?;
    SUBCOMMANDS.iter().find(|row| row.name == first)
}

/// What the server's command line asks.
#[derive(Debug, PartialEq)]
pub enum Arguments {
    Help,
    Version,
    /// Serve, with this server command.
    Run(Vec<String>),
}

/// The server's command line, WORDS, after its name: `-h` or `-V` as its
/// first word, else the server command after any `--`, `dbt lsp` when
/// none; or why it cannot be read, a first word that is another option.
pub fn arguments(words: &[String]) -> Result<Arguments, String> {
    if let Some(option) = words.first().filter(|word| word.starts_with('-') && *word != "--") {
        return match option.as_str() {
            "-h" | "--help" => Ok(Arguments::Help),
            "-V" | "--version" => Ok(Arguments::Version),
            other => Err(format!("{other} is no option of dbt-lsp-plus")),
        };
    }
    let command: Vec<String> = words.iter().skip_while(|word| *word == "--").cloned().collect();
    let command =
        Some(command).filter(|command| !command.is_empty()).unwrap_or_else(|| vec!["dbt".into(), "lsp".into()]);
    Ok(Arguments::Run(command))
}

#[derive(Debug, PartialEq)]
enum Asked {
    Help,
    /// Its arguments, as `operands` reads them, and the server command.
    Run {
        arguments: Value,
        command: Vec<String>,
    },
}

/// What WORDS, after ROW's name, ask: its usage, by `-h` or `--help`
/// first, whatever follows; else its arguments, as `operands` reads them,
/// then, after `--`, the server's, as `arguments` reads them.  Or why they
/// cannot be read.
fn asked(row: &Row, words: &[String]) -> Result<Asked, String> {
    if let Some(first) = words.first()
        && (first == "-h" || first == "--help")
    {
        return Ok(Asked::Help);
    }
    let split = words.iter().position(|word| word == "--").unwrap_or(words.len());
    let server = words.get(split + 1..).unwrap_or_default();
    let asked = operands(row, &words[..split])?;
    match arguments(server)? {
        Arguments::Run(command) => Ok(Asked::Run { arguments: asked, command }),
        _ => Err("-h and -V are not the server's arguments here".into()),
    }
}

/// ROW's arguments from WORDS, those before `--`: the operand under its
/// name, or its default, beside the row's option, as `option` reads it.
/// Or why they cannot be read.
fn operands(row: &Row, words: &[String]) -> Result<Value, String> {
    let (words, mut arguments) = option(row, words);
    let (name, default) = row.operand;
    let operand = match (words, default) {
        ([], Ok(default)) => default.to_owned(),
        ([], Err(needed)) => return Err(format!("{} needs {needed}", row.name)),
        ([word], _) if !word.starts_with('-') => word.clone(),
        ([word], _) => return Err(format!("{word} is no option of the subcommand")),
        ([_, extra, ..], _) => return Err(format!("{extra}: the server's arguments follow --")),
    };
    arguments[name] = json!(operand);
    Ok(arguments)
}

/// WORDS without the row's option after the operand, `--NAME N`, and that
/// option as arguments: N a whole number, else its text, which the run
/// refuses.
fn option<'a>(row: &Row, words: &'a [String]) -> (&'a [String], Value) {
    match (row.option, words) {
        (Some(name), [operand, flag, value]) if flag.strip_prefix("--") == Some(name) => {
            let value = value.parse::<u64>().map_or_else(|_| json!(value), Value::from);
            (std::slice::from_ref(operand), json!({name: value}))
        }
        _ => (words, json!({})),
    }
}

/// Runs ROW's subcommand with WORDS, those after its name: its answer on the
/// standard output, one line of compact JSON, or its usage; a usage error on
/// the standard error, with status 2.
pub fn main(row: &Row, words: &[String]) -> ExitCode {
    let (arguments, command) = match asked(row, words) {
        Ok(Asked::Run { arguments, command }) => (arguments, command),
        Ok(Asked::Help) => {
            println!("{}", row.usage);
            return ExitCode::SUCCESS;
        }
        Err(why) => {
            eprintln!("dbt-lsp-plus: {why}\n\n{}", row.usage);
            return ExitCode::from(2);
        }
    };
    let (answer, ok) = (row.run)(&arguments, &command);
    // A reader that closed the pipe, as `| head`, wants no more and no panic.
    if !answer.is_null() {
        let _ = writeln!(std::io::stdout(), "{answer}");
    }
    ExitCode::from(u8::from(!ok))
}

fn by_error(answer: Value) -> (Value, bool) {
    let ok = answer["error"].is_null();
    (answer, ok)
}

/// What the server would find from PATH with the server command COMMAND,
/// and whether it would work: its answer, and whether a project was found
/// whose dbt answers as a dbt 2 does.
fn doctor(path: &Path, command: &[String]) -> (Value, bool) {
    let version = env!("CARGO_PKG_VERSION");
    // `..` resolved as written, as a client's URI has none: the walk up to a
    // project then never passes through a directory the file lies outside.
    let path = locate::lexical(&std::path::absolute(path).unwrap_or_else(|_| path.to_owned()));
    let shown = path.display().to_string();
    let (project, route) = match locate::project(&path, command, "give a path within the project") {
        Ok(found) => found,
        Err(error) => return (json!({"version": version, "path": shown, "project": null, "error": error}), false),
    };
    let (dbt, dir) = locate::dbt_of(&project, route, command);
    // What `locate::dbt` left unprobed is probed as it probes a candidate.
    let probed = if dbt.probed { Ok(()) } else { locate::probe(&dbt.command, dir.as_deref()) };
    // With the server command as given, as select asks it: finding the dbt
    // changes only a bare `dbt`, and the guard reads none of it.
    let packages = packages::refused(&project, command);
    let skipped = dbt.skipped.iter().map(|(path, why)| json!({"path": path.display().to_string(), "why": why}));
    let found = json!({"command": dbt.command, "named": dbt.named, "source": dbt.source,
                       "skipped": skipped.collect::<Vec<_>>(), "why": probed.as_ref().err()});
    // The route's name as the enum spells it, in lower case.
    let route = format!("{route:?}").to_lowercase();
    let answer = json!({"version": version, "path": shown, "project": project.display().to_string(), "route": route,
                        "dbt": found, "packages": packages});
    (answer, probed.is_ok())
}

/// The nodes that SELECTOR selects in the project of the current directory
/// with the server command COMMAND, as `dbt-lsp-plus.nodes` answers them: by
/// `dbt list`, in a run directory removed afterwards; or why it did not
/// run.  Refused before anything runs without a project, or by the guard.
fn select(selector: &str, command: &[String]) -> Value {
    let cwd = std::env::current_dir().unwrap_or_default();
    let advice = "run it within the project, or name it with -- dbt lsp --project-dir DIR";
    let (project, route) = match locate::project(&cwd, command, advice) {
        Ok(found) => found,
        Err(why) => return commands::failure(commands::NODES, &why),
    };
    // The guard reads the server command's install path, which finding its
    // dbt leaves as is.
    if let Some(why) = packages::refused(&project, command) {
        return commands::failure(commands::NODES, &why);
    }
    let (dbt, dir) = locate::dbt_of(&project, route, command);
    let cache = config::cache_dir(&config::cache_base(), &project, &dbt.command);
    // What a select or a proxy killed by a signal left, swept after a day, as `Workspace::cache_dir` does.
    config::remove_stale(&cache);
    let job = commands::nodes_job(selector);
    let run = config::Dbt::new(dbt.command, dir).run(&job.words, &project, cache.join("run"), config::RUN_LOG);
    let Some(run) = run else {
        return commands::failure(commands::NODES, commands::NO_LSP_WORD);
    };
    let (sender, answer) = mpsc::channel();
    let report = Box::new(move |result| drop(sender.send(result)));
    let running = commands::start(job, run, report);
    let ended = || commands::failure(commands::NODES, "the run ended without an answer");
    let result = answer.recv().unwrap_or_else(|_| ended());
    running.stop();
    let _ = std::fs::remove_dir_all(&cache);
    result
}

/// What OPERAND names from the directory DIR: a place where READ gives the
/// text of a file at its PATH, tried as `PATH:LINE:COLUMN`, `PATH:LINE`,
/// then `PATH`, LINE and COLUMN positive integers from 1, COLUMN in
/// characters, PATH made absolute from DIR, each `..` dropping the
/// component before it, as written; else a unique id, as written.
fn operand(operand: &str, dir: &Path, read: impl Fn(&Path) -> Option<String>) -> Which {
    let (mut path, mut numbers) = (operand, Vec::new());
    let mut candidates = vec![(path, numbers.clone())];
    while let Some((rest, number)) = path.rsplit_once(':').filter(|_| numbers.len() < 2)
        && let Ok(number @ 1..) = number.parse::<u64>()
    {
        (path, numbers) = (rest, [vec![number - 1], numbers].concat());
        candidates.push((path, numbers.clone()));
    }
    for (path, numbers) in candidates.into_iter().rev() {
        let path = locate::lexical(&dir.join(path));
        if let Some(text) = read(&path) {
            let (line, column) = (numbers.first().copied().unwrap_or(0), numbers.get(1).copied().unwrap_or(0));
            let offset = LineStarts::new(&text).offset(line, column, Encoding::Utf32);
            return Which::At((path, text, offset));
        }
    }
    Which::Id(operand.to_owned())
}

/// The CLI's index of the project at ROOT, in WORKSPACE: the manifest kept
/// for the project as it lies on disk, else that of a parse, waited for,
/// and kept when it did not fail; a parse that ended
/// well with no manifest that reads fails here, where no server's wait
/// could explain the missing index.
fn indexed(workspace: &mut Workspace, root: &Path) {
    let stamp = cache::disk(root, workspace.dbt.command());
    let cache = workspace.cache_dir(root);
    let kept = fs::read(config::kept(&cache, stamp)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if let Some(manifest) = kept {
        workspace.parsed(Some(config::ParseIndex::of(&manifest)), None);
        return;
    }
    let (sender, outcomes) = mpsc::channel();
    workspace.parse(false, Box::new(move |outcome| drop(sender.send(outcome))));
    // None started: no report kept the channel open.
    let Ok(outcome) = outcomes.recv() else { return };
    let failure = outcome.failure.or(outcome.index.is_none().then_some("it left no manifest that reads".to_owned()));
    let failed = failure.is_some();
    workspace.parsed(outcome.index, failure);
    if !failed && stamp != 0 {
        config::keep(&cache, stamp);
    }
}

/// KEY's answer to QUESTION, asked of the Workspace of the project found
/// from FROM with the server command COMMAND, its index the CLI's:
/// `{"error": E, KEY: V}`, V what it found, else null, E why, or the last
/// parse's failure beside V, as `workspace::reply` words them for ASKED.
/// Only the kept manifest outlives the call.
fn answered(
    key: &str,
    from: &Path,
    asked: &str,
    command: &[String],
    question: impl FnOnce(&Workspace) -> Answer,
) -> Value {
    let advice = "give a PATH within one, run it within one, or name it with -- dbt lsp --project-dir DIR";
    let (root, route) = match locate::project(from, command, advice) {
        Ok(found) => found,
        Err(why) => return json!({"error": why, key: null}),
    };
    let (dbt, dir) = locate::dbt_of(&root, route, command);
    let mut workspace = Workspace::default();
    (workspace.root, workspace.dbt) = (Some(root.clone()), config::Dbt::new(dbt.command, dir));
    indexed(&mut workspace, &root);
    let (found, error) = workspace::reply(question(&workspace), workspace.failure.as_deref(), asked);
    workspace.stop();
    json!({"error": error, key: found})
}

/// The node that ARGUMENTS' `node` names, as nodeInfo gives it: by its
/// unique id in the project of the current directory, or at a place in the
/// project of its file.
fn node(arguments: &Value, command: &[String]) -> Value {
    let Some(asked) = arguments["node"].as_str() else {
        return json!({"error": "node needs a string `node`", "node": null});
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    // An unreadable file reads as empty.
    let which = operand(asked, &cwd, |path| path.is_file().then(|| text::read(path).unwrap_or_default()));
    let from = match &which {
        Which::At((path, ..)) => path,
        Which::Id(_) => &cwd,
    };
    answered("node", from, asked, command, |workspace| workspace.node(&which))
}

/// The lineage that ARGUMENTS ask, of the node `id` in the project of the
/// current directory, within `depth` hops, every hop without one.
fn lineage(arguments: &Value, command: &[String]) -> Value {
    let depth = match &arguments["depth"] {
        Value::Null => Some(u64::MAX),
        depth => depth.as_u64().filter(|depth| *depth >= 1),
    };
    let (Some(id), Some(depth)) = (arguments["id"].as_str(), depth) else {
        let why = "lineage needs a string `id`, and a `depth`, if any, a whole number from 1";
        return json!({"error": why, "lineage": null});
    };
    // A depth past the platform's words is no bound.
    let depth = usize::try_from(depth).unwrap_or(usize::MAX);
    let cwd = std::env::current_dir().unwrap_or_default();
    answered("lineage", &cwd, id, command, |workspace| workspace.lineage(id, depth))
}

/// Serves MCP from PATH, made the working directory before any thread or
/// child, until the input ends: each line one JSON-RPC message, each
/// request answered on one line, in order, its id as sent, a line that is
/// no JSON with error -32700 and a null id, any other message with
/// nothing.  Nothing else goes on the standard output; PATH's error goes
/// on the standard error, with status 1.
fn mcp(path: &str, command: &[String]) -> (Value, bool) {
    if let Err(error) = std::env::set_current_dir(path) {
        eprintln!("dbt-lsp-plus: {path}: {error}");
        return (Value::Null, false);
    }
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let (id, outcome) = match serde_json::from_str::<Value>(&line) {
            Ok(message) => match (message.get("id"), message["method"].as_str()) {
                (Some(id), Some(method)) => (id.clone(), respond(method, &message["params"], command)),
                _ => continue,
            },
            Err(_) => (Value::Null, Err(json!({"code": -32700, "message": "a line that is no JSON"}))),
        };
        let (key, value) = match outcome {
            Ok(result) => ("result", result),
            Err(error) => ("error", error),
        };
        // A client that closed its end wants no more and no panic.
        let _ = writeln!(std::io::stdout(), "{}", json!({"jsonrpc": "2.0", "id": id, key: value}));
    }
    (Value::Null, true)
}

/// The result of the MCP request METHOD with PARAMS, else its error, whether
/// `initialize` came or not; the version agreed is the client's when it is
/// one of VERSIONS, else the latest.
fn respond(method: &str, params: &Value, command: &[String]) -> Result<Value, Value> {
    match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str();
            let version = VERSIONS.into_iter().find(|version| Some(*version) == asked).unwrap_or(VERSIONS[0]);
            let server = json!({"name": "dbt-lsp-plus", "version": env!("CARGO_PKG_VERSION")});
            Ok(json!({"protocolVersion": version, "capabilities": {"tools": {}}, "serverInfo": server}))
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            // Each row that is a tool: its operand a required string, its option a whole number from 1.
            let tools = SUBCOMMANDS.iter().filter_map(|row| {
                let mut properties = json!({row.operand.0: {"type": "string"}});
                if let Some(option) = row.option {
                    properties[option] = json!({"type": "integer", "minimum": 1});
                }
                let schema = json!({"type": "object", "properties": properties, "required": [row.operand.0]});
                Some(json!({"name": row.name, "description": row.tool?, "inputSchema": schema}))
            });
            Ok(json!({"tools": tools.collect::<Vec<_>>()}))
        }
        "tools/call" => call(params, command),
        _ => Err(json!({"code": -32601, "message": format!("no method {method}")})),
    }
}

/// The result of `tools/call` with PARAMS: the run of the row that is the
/// tool `name`, its answer as the text and `isError` as the CLI's status 1;
/// error -32602 for no such tool.
fn call(params: &Value, command: &[String]) -> Result<Value, Value> {
    let name = params["name"].as_str().unwrap_or_default();
    let Some(row) = SUBCOMMANDS.iter().find(|row| row.tool.is_some() && row.name == name) else {
        return Err(json!({"code": -32602, "message": format!("no tool {name}")}));
    };
    let (answer, ok) = (row.run)(&params["arguments"], command);
    Ok(json!({"content": [{"type": "text", "text": answer.to_string()}], "isError": !ok}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn the_first_word_alone_names_a_subcommand() {
        let ours: [(&[&str], &str); 6] = [
            (&["doctor"], "doctor"),
            (&["doctor", "x"], "doctor"),
            (&["select", "+m"], "select"),
            (&["node", "x"], "node"),
            (&["lineage", "x"], "lineage"),
            (&["mcp"], "mcp"),
        ];
        for (line, name) in ours {
            assert_eq!(named(&words(line)).map(|row| row.name), Some(name), "{line:?}");
        }
        let servers: [(&[&str], &[&str]); 4] = [
            (&["--", "doctor"], &["doctor"]),
            (&["dbt", "lsp"], &["dbt", "lsp"]),
            (&["--", "/p/dbt", "lsp", "--project-dir", "/p"], &["/p/dbt", "lsp", "--project-dir", "/p"]),
            (&[], &["dbt", "lsp"]),
        ];
        for (line, server) in servers {
            assert!(named(&words(line)).is_none(), "{line:?}");
            let Ok(Arguments::Run(command)) = arguments(&words(line)) else { panic!("{line:?}") };
            assert_eq!(command, words(server), "{line:?}");
        }
        let version = words(&["-V", "select"]);
        assert!(named(&version).is_none(), "an option first");
        assert_eq!(arguments(&version), Ok(Arguments::Version), "an option first");
    }

    #[test]
    fn the_command_line_reads_options_then_the_server_command() {
        let run = |line: &[&str]| arguments(&words(line));
        let command = words(&["dbt", "lsp", "--project-dir", "p"]);
        assert_eq!(run(&["dbt", "lsp", "--project-dir", "p"]), Ok(Arguments::Run(command)));
        assert_eq!(run(&[]), Ok(Arguments::Run(words(&["dbt", "lsp"]))), "dbt lsp by default");
        assert_eq!(
            run(&["--", "-strange", "lsp"]),
            Ok(Arguments::Run(words(&["-strange", "lsp"]))),
            "-- before a dash"
        );
        for (line, asked) in [(&["-h", "x"][..], Arguments::Help), (&["--help"], Arguments::Help)] {
            assert_eq!(run(line), Ok(asked), "{line:?}");
        }
        for (line, asked) in [(&["-V", "--mode"][..], Arguments::Version), (&["--version"], Arguments::Version)] {
            assert_eq!(run(line), Ok(asked), "{line:?}");
        }
        for bad in [&["--mode", "textDocument/hover=dbt"][..], &["--default-mode", "plus"], &["--colour", "x"]] {
            assert!(run(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_subcommand_reads_its_operand_then_the_server_s_arguments_after_dashes() {
        let run = |arguments: Value, command: &[&str]| Ok(Asked::Run { arguments, command: words(command) });
        let default = ["dbt", "lsp"];
        assert_eq!(asked(&SUBCOMMANDS[0], &[]), run(json!({"path": "."}), &default), "doctor's default");
        assert_eq!(asked(&SUBCOMMANDS[0], &words(&["x.sql"])), run(json!({"path": "x.sql"}), &default));
        let helped: [(&Row, &[&str]); 4] = [
            (&SUBCOMMANDS[0], &["-h"]),
            (&SUBCOMMANDS[1], &["--help"]),
            (&SUBCOMMANDS[0], &["--help", "--", "dbt", "lsp"]),
            (&SUBCOMMANDS[1], &["-h", "x"]),
        ];
        for (row, line) in helped {
            assert_eq!(asked(row, &words(line)), Ok(Asked::Help), "{} {line:?}", row.name);
        }
        let given = words(&["--", "--", "uvx", "dbt", "lsp", "--target", "prod"]);
        assert_eq!(
            asked(&SUBCOMMANDS[0], &given),
            run(json!({"path": "."}), &["uvx", "dbt", "lsp", "--target", "prod"])
        );
        let selected = words(&["+m+", "--", "dbt", "lsp"]);
        assert_eq!(asked(&SUBCOMMANDS[1], &selected), run(json!({"selector": "+m+"}), &default));
        let refused: [(&Row, &[&str]); 9] = [
            (&SUBCOMMANDS[0], &["--bogus"]),
            (&SUBCOMMANDS[0], &["x.sql", "-h"]),
            (&SUBCOMMANDS[0], &["a", "b"]),
            (&SUBCOMMANDS[1], &[]),
            (&SUBCOMMANDS[2], &[]),
            (&SUBCOMMANDS[1], &["--", "dbt", "lsp"]),
            (&SUBCOMMANDS[0], &["--", "--mode", "x"]),
            (&SUBCOMMANDS[0], &["--", "-V"]),
            (&SUBCOMMANDS[0], &["--", "--backend", "never"]),
        ];
        for (row, line) in refused {
            assert!(asked(row, &words(line)).is_err(), "{} {line:?}", row.name);
        }
    }

    #[test]
    fn a_row_without_its_operand_takes_its_default_or_says_what_it_needs() {
        let operands: Vec<(&str, Result<&str, &str>)> =
            SUBCOMMANDS.iter().map(|row| (row.name, row.operand.1)).collect();
        let needs = [("node", Err("an ID or a PATH")), ("lineage", Err("an ID")), ("mcp", Ok("."))];
        assert_eq!(operands, [[("doctor", Ok(".")), ("select", Err("a SELECTOR"))].as_slice(), &needs].concat());
        let none: Vec<Result<Asked, String>> = SUBCOMMANDS.iter().map(|row| asked(row, &[])).collect();
        let here = || Asked::Run { arguments: json!({"path": "."}), command: words(&["dbt", "lsp"]) };
        let needed: [Result<Asked, String>; 5] = [
            Ok(here()),
            Err("select needs a SELECTOR".into()),
            Err("node needs an ID or a PATH".into()),
            Err("lineage needs an ID".into()),
            Ok(here()),
        ];
        assert_eq!(none, needed);
    }

    #[test]
    fn lineage_reads_its_depth_after_its_id_and_no_other_row_an_option() {
        let (node, lineage) = (&SUBCOMMANDS[2], &SUBCOMMANDS[3]);
        assert_eq!((node.name, lineage.name), ("node", "lineage"));
        let run = |arguments: Value, command: &[&str]| Ok(Asked::Run { arguments, command: words(command) });
        let default = ["dbt", "lsp"];
        let read = [
            (&["model.shop.a"][..], run(json!({"id": "model.shop.a"}), &default)),
            (&["model.shop.a", "--depth", "2"], run(json!({"id": "model.shop.a", "depth": 2}), &default)),
            // A depth that is no whole number is the run's to refuse, as a tool's call would give it.
            (
                &["a", "--depth", "x", "--", "uvx", "dbt", "lsp"],
                run(json!({"id": "a", "depth": "x"}), &["uvx", "dbt", "lsp"]),
            ),
        ];
        for (line, expected) in read {
            assert_eq!(asked(lineage, &words(line)), expected, "{line:?}");
        }
        let refused: [(&Row, &[&str]); 5] = [
            (lineage, &["--depth", "2"]),
            (lineage, &["a", "--depth"]),
            (lineage, &["a", "--deep", "2"]),
            (lineage, &["a", "b"]),
            (node, &["a", "--depth", "2"]),
        ];
        for (row, line) in refused {
            assert!(asked(row, &words(line)).is_err(), "{} {line:?}", row.name);
        }
    }

    #[test]
    fn mcp_answers_initialize_ping_and_the_tools_of_the_rows() {
        let none: &[String] = &[];
        let agreed = |asked: Value| respond("initialize", &json!({"protocolVersion": asked}), none);
        for (asked, version) in [
            (json!("2025-11-25"), "2025-11-25"),
            (json!("2024-11-05"), "2024-11-05"),
            (json!("2099-01-01"), "2025-11-25"),
            (Value::Null, "2025-11-25"),
        ] {
            let server = json!({"name": "dbt-lsp-plus", "version": env!("CARGO_PKG_VERSION")});
            let expected = json!({"protocolVersion": version, "capabilities": {"tools": {}}, "serverInfo": server});
            assert_eq!(agreed(asked.clone()), Ok(expected), "{asked}");
        }
        assert_eq!(respond("ping", &json!({}), none), Ok(json!({})));
        let listed = respond("tools/list", &json!({}), none).unwrap();
        let tools: Vec<(&Value, &Value)> =
            listed["tools"].as_array().unwrap().iter().map(|tool| (&tool["name"], &tool["inputSchema"])).collect();
        let node = json!({"type": "object", "properties": {"node": {"type": "string"}}, "required": ["node"]});
        let lineage = json!({"type": "object", "required": ["id"],
                             "properties": {"id": {"type": "string"}, "depth": {"type": "integer", "minimum": 1}}});
        assert_eq!(tools, [(&json!("node"), &node), (&json!("lineage"), &lineage)]);
        let described = listed["tools"].as_array().unwrap().iter().all(|tool| tool["description"].as_str().is_some());
        assert!(described, "{listed}");
        for method in ["server/discover", "resources/list"] {
            let unknown = json!({"code": -32601, "message": format!("no method {method}")});
            assert_eq!(respond(method, &json!({}), none), Err(unknown));
        }
    }

    #[test]
    fn a_tool_is_a_row_with_one_and_its_arguments_that_do_not_read_are_its_error() {
        let none: &[String] = &[];
        let call =
            |name: &str, arguments: Value| respond("tools/call", &json!({"name": name, "arguments": arguments}), none);
        for name in ["select", "doctor", "mcp", "nowhere"] {
            assert_eq!(call(name, json!({})), Err(json!({"code": -32602, "message": format!("no tool {name}")})));
        }
        // The text is the answer, compact, as the CLI writes it; isError is its status 1.
        let refused =
            |answer: Value| Ok(json!({"content": [{"type": "text", "text": answer.to_string()}], "isError": true}));
        let node = json!({"error": "node needs a string `node`", "node": null});
        assert_eq!(call("node", json!({"node": 1})), refused(node));
        let why = "lineage needs a string `id`, and a `depth`, if any, a whole number from 1";
        let lineage = json!({"error": why, "lineage": null});
        for depth in [json!(0), json!(-1), json!("2"), json!(1.5)] {
            assert_eq!(
                call("lineage", json!({"id": "model.shop.a", "depth": depth})),
                refused(lineage.clone()),
                "{depth}"
            );
        }
        assert_eq!(call("lineage", json!({"depth": 2})), refused(lineage));
    }

    #[test]
    fn a_node_s_operand_is_a_file_s_place_else_a_unique_id() {
        // Three lines; the third opens with a character of two UTF-16 units and four bytes, then one of two
        // bytes: its column 3 lies six bytes in.
        const A: &str = "select 1\nfrom x\n\u{1F600}é z";
        let read = |path: &Path| match path.to_str() {
            Some("/w/shop/models/a.sql") => Some(A.to_owned()),
            Some("/w/shop/x:1") => Some(String::new()),
            _ => None,
        };
        let place = |path: &str, text: &str, offset: usize| Which::At((path.into(), text.to_owned(), offset));
        let a = "/w/shop/models/a.sql";
        let third = A.find('\u{1F600}').unwrap();
        let places = [
            ("models/a.sql", place(a, A, 0)),
            ("models/a.sql:2", place(a, A, 9)),
            ("models/a.sql:2:4", place(a, A, 12)),
            ("models/a.sql:3:3", place(a, A, third + 4 + 2)),
            ("./models/../models/a.sql:1:1", place(a, A, 0)),
            ("/w/shop/models/a.sql", place(a, A, 0)),
            ("x:1", place("/w/shop/x:1", "", 0)),
        ];
        let ids = ["model.shop.a", "models/a.sql:0", "models/a.sql:3:x", "models/a.sql:1:2:3", "models/b.sql"];
        let expected: Vec<(&str, Which)> =
            places.into_iter().chain(ids.map(|id| (id, Which::Id(id.to_owned())))).collect();
        // Each operand asked beside what it reads as, so that a failure names it.
        let found: Vec<(&str, Which)> =
            expected.iter().map(|(asked, _)| (*asked, operand(asked, Path::new("/w/shop"), read))).collect();
        assert_eq!(found, expected);
    }
}
