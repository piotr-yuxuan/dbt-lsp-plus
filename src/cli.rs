// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The command line: the server's arguments, and the subcommands,
//! `doctor`, `select` and `node`, what an agent in a shell asks without an
//! LSP client, each answered on one line of compact JSON.  The project and
//! its dbt are found as the server finds them.

use crate::locate;
use crate::text::{self, Encoding, LineStarts};
use crate::workspace::{self, Which, Workspace};
use crate::{cache, commands, config, packages};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;

/// A subcommand: its name, its usage, its operand, and its run, which from its operand and the server command gives its answer
/// and whether it worked.
pub struct Row {
    name: &'static str,
    usage: &'static str,
    /// Without an operand: its default, else what it needs, said in the
    /// usage error.
    operand: Result<&'static str, &'static str>,
    run: fn(String, &[String]) -> (Value, bool),
}

const SUBCOMMANDS: [Row; 3] = [
    Row {
        name: "doctor",
        usage: "\
usage: dbt-lsp-plus doctor [PATH] [-- ARGUMENTS...]

Which project and dbt the server would find from PATH, on one line of JSON.
Exit status 0 when it finds a dbt 2, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: Ok("."),
        run: |path, command| doctor(Path::new(&path), command),
    },
    Row {
        name: "select",
        usage: "\
usage: dbt-lsp-plus select SELECTOR [-- ARGUMENTS...]

The nodes that SELECTOR, a dbt selector, selects, on one line of JSON.
Exit status 0 when nodes are listed, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: Err("a SELECTOR"),
        run: |selector, command| by_error(select(&selector, command)),
    },
    Row {
        name: "node",
        usage: "\
usage: dbt-lsp-plus node ID|PATH[:LINE[:COLUMN]] [-- ARGUMENTS...]

What nodeInfo gives of the node that ID, or a place in PATH, names, on one
line of JSON; LINE and COLUMN from 1, COLUMN in characters.
Exit status 0 when `error` is null, 1 otherwise, 2 for a usage error.
https://github.com/piotr-yuxuan/dbt-lsp-plus/blob/main/docs/reference/cli.md",
        operand: Err("an ID or a PATH"),
        run: |operand, command| by_error(node(&operand, command)),
    },
];

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
    /// Its OPERAND, doctor's PATH, select's SELECTOR or node's ID or PATH,
    /// and the server command.
    Run {
        operand: String,
        command: Vec<String>,
    },
}

/// What WORDS, after ROW's name, ask: its usage, by `-h` or
/// `--help` first, whatever follows; else its operand, or the row's
/// default, then, after `--`, the server's
/// arguments, as `arguments` reads them.  Or why they cannot be read.
fn asked(row: &Row, words: &[String]) -> Result<Asked, String> {
    if let Some(first) = words.first()
        && (first == "-h" || first == "--help")
    {
        return Ok(Asked::Help);
    }
    let split = words.iter().position(|word| word == "--").unwrap_or(words.len());
    let server = words.get(split + 1..).unwrap_or_default();
    let operand = match (&words[..split], row.operand) {
        ([], Ok(default)) => default.to_owned(),
        ([], Err(needed)) => return Err(format!("{} needs {needed}", row.name)),
        ([word], _) if !word.starts_with('-') => word.clone(),
        ([word], _) => return Err(format!("{word} is no option of the subcommand")),
        ([_, extra, ..], _) => return Err(format!("{extra}: the server's arguments follow --")),
    };
    match arguments(server)? {
        Arguments::Run(command) => Ok(Asked::Run { operand, command }),
        _ => Err("-h and -V are not the server's arguments here".into()),
    }
}

/// Runs ROW's subcommand with WORDS, those after its name: its answer on the
/// standard output, one line of compact JSON, or its usage; a usage error on
/// the standard error, with status 2.
pub fn main(row: &Row, words: &[String]) -> ExitCode {
    let (operand, command) = match asked(row, words) {
        Ok(Asked::Run { operand, command }) => (operand, command),
        Ok(Asked::Help) => {
            println!("{}", row.usage);
            return ExitCode::SUCCESS;
        }
        Err(why) => {
            eprintln!("dbt-lsp-plus: {why}\n\n{}", row.usage);
            return ExitCode::from(2);
        }
    };
    let (answer, ok) = (row.run)(operand, &command);
    // A reader that closed the pipe, as `| head`, wants no more and no panic.
    let _ = writeln!(std::io::stdout(), "{answer}");
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
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

/// The node that ASKED, node's operand, names, with the server command
/// COMMAND, as nodeInfo gives it: by its unique id in the project of the
/// current directory, or at a place in the project of its file; or why
/// none, the last parse's failure among them.  Only the kept manifest
/// outlives the run.
fn node(asked: &str, command: &[String]) -> Value {
    let cwd = std::env::current_dir().unwrap_or_default();
    // An unreadable file reads as empty.
    let which = operand(asked, &cwd, |path| path.is_file().then(|| text::read(path).unwrap_or_default()));
    let from = match &which {
        Which::At((path, ..)) => path,
        Which::Id(_) => &cwd,
    };
    let advice = "give a PATH within one, run it within one, or name it with -- dbt lsp --project-dir DIR";
    let (root, route) = match locate::project(from, command, advice) {
        Ok(found) => found,
        Err(why) => return json!({"error": why, "node": null}),
    };
    let (dbt, dir) = locate::dbt_of(&root, route, command);
    let mut workspace = Workspace::default();
    (workspace.root, workspace.dbt) = (Some(root.clone()), config::Dbt::new(dbt.command, dir));
    indexed(&mut workspace, &root);
    let answer = workspace.node(&which);
    let (node, error) = workspace::reply(answer, workspace.failure.as_deref(), asked);
    workspace.stop();
    json!({"error": error, "node": node})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn the_first_word_alone_names_a_subcommand() {
        let ours: [(&[&str], &str); 4] = [
            (&["doctor"], "doctor"),
            (&["doctor", "x"], "doctor"),
            (&["select", "+m"], "select"),
            (&["node", "x"], "node"),
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
        let run =
            |operand: &str, command: &[&str]| Ok(Asked::Run { operand: operand.to_owned(), command: words(command) });
        let default = ["dbt", "lsp"];
        assert_eq!(asked(&SUBCOMMANDS[0], &[]), run(".", &default), "doctor's default");
        assert_eq!(asked(&SUBCOMMANDS[0], &words(&["x.sql"])), run("x.sql", &default));
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
        assert_eq!(asked(&SUBCOMMANDS[0], &given), run(".", &["uvx", "dbt", "lsp", "--target", "prod"]));
        let selected = words(&["+m+", "--", "dbt", "lsp"]);
        assert_eq!(asked(&SUBCOMMANDS[1], &selected), run("+m+", &default));
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
        let operands: Vec<(&str, Result<&str, &str>)> = SUBCOMMANDS.iter().map(|row| (row.name, row.operand)).collect();
        assert_eq!(operands, [("doctor", Ok(".")), ("select", Err("a SELECTOR")), ("node", Err("an ID or a PATH"))]);
        let none: Vec<Result<Asked, String>> = SUBCOMMANDS.iter().map(|row| asked(row, &[])).collect();
        let doctor = Asked::Run { operand: ".".into(), command: words(&["dbt", "lsp"]) };
        assert_eq!(none, [Ok(doctor), Err("select needs a SELECTOR".into()), Err("node needs an ID or a PATH".into())]);
    }

    #[test]
    fn a_node_s_operand_is_a_file_s_place_else_a_unique_id() {
        // Three lines, the third with a character of two UTF-16 units and three bytes before its column 3.
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
