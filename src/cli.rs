// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The subcommands, `doctor`, `select` and `node`: what an agent in a shell
//! asks without an LSP client, each answered on one line of compact JSON.
//! The project and its dbt are found as the server finds them.

use crate::locate::{self, Probe, Route};
use crate::modes::{self, Arguments, Launch};
use crate::text::{self, Encoding, LineStarts};
use crate::workspace::{self, Which, Workspace};
use crate::{cache, commands, config, packages, proxy};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;

const DOCTOR_USAGE: &str = "\
usage: dbt-lsp-plus doctor [PATH] [-- ARGUMENTS...]

Says on one line of JSON what the server would find from PATH, by default
the current directory: the project and by which route; the dbt, where from,
what was skipped, and whether it has `lsp`; whether the server would answer
alone, and why; whether this system can cut dbt off the network; why a parse
would be refused, if it would.  It runs only what the server runs as it
starts: git, `lsp --help` of the dbt, `unshare` on `true`.  ARGUMENTS are
the server's own, as an editor gives them: `--backend never`, `-- dbt lsp
--project-dir DIR`.  Exit status 0 when a project is found whose dbt answers
as a dbt 2 does, 1 otherwise, 2 for a usage error.

  -h, --help   this text";

const SELECT_USAGE: &str = "\
usage: dbt-lsp-plus select SELECTOR [-- ARGUMENTS...]

Lists on one line of JSON the nodes that the dbt selector SELECTOR selects in
the project of the current directory, as `dbt list` gives them, cut off the
network: {\"nodes\": [...], \"error\": null}, or `error` set and no node.
ARGUMENTS are the server's own, as for doctor.  Exit status 0 when nodes are
listed, 1 otherwise, \"No nodes found\" included, 2 for a usage error.

  -h, --help   this text";

const NODE_USAGE: &str = "\
usage: dbt-lsp-plus node ID|PATH[:LINE[:COLUMN]] [-- ARGUMENTS...]

Gives on one line of JSON the node that the unique id ID names in the
project of the current directory, or the one named at line LINE, column
COLUMN (from 1, COLUMN in characters) of the file PATH, in the project of
that file, as the command nodeInfo gives it, with its `summary` and its
`children`: {\"error\": null, \"node\": {...}}, or `error` set.  PATH alone
names the one node its file defines.  The operand is a PATH when such a
file exists, else an ID.  The nodes are those of the manifest kept from
a `dbt parse` of the project as its files lie now, else of a `dbt parse`
run then, whose manifest is kept when it does not fail.  ARGUMENTS are
the server's own, as for doctor.  Exit status 0 when `error` is null, 1
otherwise, 2 for a usage error.

  -h, --help   this text";

/// A subcommand, named by the first word of the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subcommand {
    Doctor,
    Select,
    Node,
}

/// A subcommand's run: from its operand, the server's launch and the
/// server command, its answer and whether it worked.
type Run = fn(Option<String>, Launch, &[String]) -> (Value, bool);

/// A subcommand's row: its name, its usage, the operand it needs, if any,
/// and its run.
struct Row {
    subcommand: Subcommand,
    name: &'static str,
    usage: &'static str,
    needs: Option<&'static str>,
    run: Run,
}

/// The subcommands, in `Subcommand`'s order, by which `Subcommand::row`
/// reads them; a unit test holds each row at its subcommand's place.
const SUBCOMMANDS: [Row; 3] = [
    Row {
        subcommand: Subcommand::Doctor,
        name: "doctor",
        usage: DOCTOR_USAGE,
        needs: None,
        run: |path, launch, command| doctor(Path::new(path.as_deref().unwrap_or(".")), launch, command),
    },
    Row {
        subcommand: Subcommand::Select,
        name: "select",
        usage: SELECT_USAGE,
        needs: Some("a SELECTOR"),
        run: |selector, _, command| by_error(select(&selector.unwrap_or_default(), command)),
    },
    Row {
        subcommand: Subcommand::Node,
        name: "node",
        usage: NODE_USAGE,
        needs: Some("an ID or a PATH"),
        run: |operand, _, command| by_error(node(&operand.unwrap_or_default(), command)),
    },
];

impl Subcommand {
    /// Its row of `SUBCOMMANDS`.
    fn row(self) -> &'static Row {
        &SUBCOMMANDS[self as usize]
    }
}

/// The subcommand that WORDS, the command line after the program's name,
/// name by their first word; none when it names none, and the words are the
/// server's.
pub fn named(words: &[String]) -> Option<Subcommand> {
    let first = words.first()?;
    SUBCOMMANDS.iter().find(|row| row.name == first).map(|row| row.subcommand)
}

/// What a subcommand's words ask.
#[derive(Debug, PartialEq)]
enum Asked {
    Help,
    /// Its OPERAND, doctor's PATH, select's SELECTOR or node's ID or PATH,
    /// and the server's launch and command.
    Run {
        operand: Option<String>,
        launch: Launch,
        command: Vec<String>,
    },
}

/// What WORDS, after SUBCOMMAND's name, ask: its usage, by `-h` or
/// `--help` first, whatever follows; else its operand, optional for doctor and needed by
/// select and node, then, after `--`, the server's arguments, as `modes::arguments`
/// reads them.  Or why they cannot be read.
fn asked(subcommand: Subcommand, words: &[String]) -> Result<Asked, String> {
    if let Some(first) = words.first()
        && (first == "-h" || first == "--help")
    {
        return Ok(Asked::Help);
    }
    let split = words.iter().position(|word| word == "--").unwrap_or(words.len());
    let server = words.get(split + 1..).unwrap_or_default();
    let operand = match &words[..split] {
        [] => None,
        [word] if !word.starts_with('-') => Some(word.clone()),
        [word] => return Err(format!("{word} is no option of the subcommand")),
        [_, extra, ..] => return Err(format!("{extra}: the server's arguments follow --")),
    };
    let row = subcommand.row();
    if let Some(needed) = row.needs
        && operand.is_none()
    {
        return Err(format!("{} needs {needed}", row.name));
    }
    match modes::arguments(server)? {
        Arguments::Run { launch, command, .. } => Ok(Asked::Run { operand, launch, command }),
        _ => Err("-h and -V are not the server's arguments here".into()),
    }
}

/// Runs SUBCOMMAND with WORDS, those after its name: its answer on the
/// standard output, one line of compact JSON, or its usage; a usage error on
/// the standard error, with status 2.
pub fn main(subcommand: Subcommand, words: &[String]) -> ExitCode {
    let row = subcommand.row();
    let (operand, launch, command) = match asked(subcommand, words) {
        Ok(Asked::Run { operand, launch, command }) => (operand, launch, command),
        Ok(Asked::Help) => {
            println!("{}", row.usage);
            return ExitCode::SUCCESS;
        }
        Err(why) => {
            eprintln!("dbt-lsp-plus: {why}\n\n{}", row.usage);
            return ExitCode::from(2);
        }
    };
    let (answer, ok) = (row.run)(operand, launch, &command);
    // A reader that closed the pipe, as `| head`, wants no more and no panic.
    let _ = writeln!(std::io::stdout(), "{answer}");
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// ANSWER, and whether it worked: whether its `error` is null.
fn by_error(answer: Value) -> (Value, bool) {
    let ok = answer["error"].is_null();
    (answer, ok)
}

/// Why no project was found.
enum Missing {
    /// The server command's `--project-dir` holds no `dbt_project.yml`; why,
    /// in words that need no advice.
    Given(String),
    /// No `dbt_project.yml` at or above the path, which the caller words.
    Above,
}

/// The project as the server finds it from PATH, absolute, with the server
/// command COMMAND, and by which route: given, its `--project-dir`; else
/// PATH, when it holds `dbt_project.yml`, as a client's folder; else the
/// project of PATH, as a document's.  Nothing runs.
fn project(path: &Path, command: &[String]) -> Result<(PathBuf, Route), Missing> {
    if let Some(project) = proxy::project_dir(command) {
        let why = || {
            Missing::Given(format!("no dbt_project.yml in {}, the server command's --project-dir", project.display()))
        };
        return project.join("dbt_project.yml").is_file().then(|| (project.clone(), Route::Given)).ok_or_else(why);
    }
    match path.join("dbt_project.yml").is_file() {
        true => Ok((path.to_owned(), Route::Folder)),
        false => locate::project_of(path, command).map(|project| (project, Route::Document)).ok_or(Missing::Above),
    }
}

/// The dbt of PROJECT, found on ROUTE, as the server finds it from the
/// server command COMMAND, and where its processes run: given, the command
/// as written, in the current directory; else `locate::dbt`'s from COMMAND
/// anchored in the current directory, in the project.
fn dbt_of(project: &Path, route: Route, command: &[String]) -> (locate::Located, Option<PathBuf>) {
    if route == Route::Given {
        return (locate::Located::written(command), None);
    }
    let git = locate::Git { program: "git".into(), env: std::env::vars_os().collect() };
    let anchored = std::env::current_dir().map_or_else(|_| command.to_vec(), |cwd| config::anchored(command, &cwd));
    (locate::dbt(project, &anchored, &git), Some(project.to_owned()))
}

/// What the server would find and do from PATH with LAUNCH and the server
/// command COMMAND, and whether it would work: its answer, and whether a
/// project was found whose dbt answers as a dbt 2 does.
fn doctor(path: &Path, launch: Launch, command: &[String]) -> (Value, bool) {
    let version = env!("CARGO_PKG_VERSION");
    // `..` resolved as written, as a client's URI has none: the walk up to a
    // project then never passes through a directory the file lies outside.
    let path = proxy::lexical(&std::path::absolute(path).unwrap_or_else(|_| path.to_owned()));
    let shown = path.display().to_string();
    let (project, route) = match project(&path, command) {
        Ok(found) => found,
        Err(missing) => {
            let error = match missing {
                Missing::Given(why) => why,
                Missing::Above => format!("no dbt_project.yml at or above {shown}; give a path within the project"),
            };
            return (json!({"version": version, "path": shown, "project": null, "error": error}), false);
        }
    };
    let (dbt, dir) = dbt_of(&project, route, command);
    // What `locate::dbt` left unprobed is probed as the server probes it.
    let probed = match dbt.lsp {
        Some(lsp) => Ok(lsp),
        None => match locate::probe(&dbt.command, dir.as_deref()) {
            Probe::Lsp => Ok(true),
            Probe::NoLsp => Ok(false),
            Probe::Other(why) => Err(why),
        },
    };
    let alone = locate::alone(launch == Launch::Never, route, || probed == Ok(false));
    let cut = config::can_cut();
    // With the server command as given, as select asks it: finding the dbt
    // changes only a bare `dbt`, and the guard reads none of it.
    let packages = packages::refused(&project, command, cut, alone.is_some());
    let skipped = dbt.skipped.iter().map(|(path, why)| json!({"path": path.display().to_string(), "why": why}));
    let found = json!({"command": dbt.command, "named": dbt.named, "source": dbt.source,
                       "skipped": skipped.collect::<Vec<_>>(), "lsp": probed.as_ref().ok(), "why": probed.as_ref().err()});
    // The route's name as the enum spells it, in lower case.
    let route = format!("{route:?}").to_lowercase();
    let answer = json!({"version": version, "path": shown, "project": project.display().to_string(), "route": route,
                        "dbt": found, "alone": alone, "cut": cut, "packages": packages});
    (answer, probed.is_ok())
}

/// The nodes that SELECTOR selects in the project of the current directory
/// with the server command COMMAND, as `dbt-lsp-plus.nodes` answers them: by
/// `dbt list` under the cut, in a run directory removed afterwards; or why
/// it did not run.  Refused before anything runs where the system cannot
/// cut, without a project, or by the guard.
fn select(selector: &str, command: &[String]) -> Value {
    if let Err(failure) = commands::route(commands::NODES, &|_| false, config::can_cut()) {
        return failure;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let (project, route) = match project(&cwd, command) {
        Ok(found) => found,
        Err(Missing::Given(why)) => return commands::failure(commands::NODES, &why),
        Err(Missing::Above) => {
            let advice = "run it within the project, or name it with -- dbt lsp --project-dir DIR";
            let why = format!("no dbt_project.yml at or above {}; {advice}", cwd.display());
            return commands::failure(commands::NODES, &why);
        }
    };
    // Under the cut, whether the session is alone is moot; the guard reads
    // the server command's install path, which finding its dbt leaves as is.
    if let Some(why) = packages::refused(&project, command, true, false) {
        return commands::failure(commands::NODES, &why);
    }
    let (dbt, dir) = dbt_of(&project, route, command);
    let cache = config::cache_dir(&config::cache_base().unwrap_or_else(std::env::temp_dir), &project, &dbt.command);
    // What a select or a proxy killed by a signal left, swept after a day, as `Workspace::cache_dir` does.
    config::remove_stale(&cache);
    let job = commands::nodes_job(selector);
    let run = config::Dbt::new(dbt.command, dir).run(&job.words, &project, cache.join("run"), config::RUN_LOG, true);
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

/// What node's operand names: a unique id, or a place in a file, its line
/// and column from 0, the column in characters.
#[derive(Debug, PartialEq)]
enum Operand {
    Id(String),
    Place { path: PathBuf, line: u64, column: u64 },
}

/// What OPERAND names from the directory DIR: a place when a file exists,
/// as IS_FILE says, at its PATH, tried as `PATH:LINE:COLUMN`, `PATH:LINE`,
/// then `PATH`, LINE and COLUMN positive integers from 1, PATH made
/// absolute from DIR, each `..` dropping the component before it, as
/// written; else a unique id, as written.
fn operand(operand: &str, dir: &Path, is_file: impl Fn(&Path) -> bool) -> Operand {
    let (mut path, mut numbers) = (operand, Vec::new());
    let mut candidates = vec![(path, numbers.clone())];
    while let Some((rest, number)) = path.rsplit_once(':').filter(|_| numbers.len() < 2)
        && let Ok(number @ 1..) = number.parse::<u64>()
    {
        (path, numbers) = (rest, [vec![number - 1], numbers].concat());
        candidates.push((path, numbers.clone()));
    }
    for (path, numbers) in candidates.into_iter().rev() {
        let path = proxy::lexical(&dir.join(path));
        if is_file(&path) {
            let (line, column) = (numbers.first().copied().unwrap_or(0), numbers.get(1).copied().unwrap_or(0));
            return Operand::Place { path, line, column };
        }
    }
    Operand::Id(operand.to_owned())
}

/// The CLI's index of the project at ROOT, in WORKSPACE: the manifest kept
/// for the project as it lies on disk, else that of a parse, guarded as
/// alone, waited for, and kept when it did not fail; a parse that ended
/// well with no manifest that reads fails here, where no server's wait
/// could explain the missing index.
fn indexed(workspace: &mut Workspace, root: &Path) {
    let stamp = cache::disk(root);
    let cache = workspace.cache_dir(root);
    let kept = fs::read(config::kept(&cache, stamp)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if let Some(manifest) = kept {
        workspace.parsed(Some(config::ParseIndex::of(&manifest)), None);
        return;
    }
    let (sender, outcomes) = mpsc::channel();
    workspace.parse(true, Some(Box::new(move |outcome| drop(sender.send(outcome)))));
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
    let place = operand(asked, &cwd, Path::is_file);
    let from = match &place {
        Operand::Place { path, .. } => path.clone(),
        Operand::Id(_) => cwd,
    };
    let (root, route) = match project(&from, command) {
        Ok(found) => found,
        Err(Missing::Given(why)) => return json!({"error": why, "node": null}),
        Err(Missing::Above) => {
            let advice = "give a PATH within one, run it within one, or name it with -- dbt lsp --project-dir DIR";
            let why = format!("no dbt_project.yml at or above {}; {advice}", from.display());
            return json!({"error": why, "node": null});
        }
    };
    let (dbt, dir) = dbt_of(&root, route, command);
    let mut workspace = Workspace::default();
    (workspace.root, workspace.dbt, workspace.cut) =
        (Some(root.clone()), config::Dbt::new(dbt.command, dir), config::can_cut());
    indexed(&mut workspace, &root);
    let which = match place {
        Operand::Id(id) => Which::Id(id),
        Operand::Place { path, line, column } => {
            let text = text::read(&path).unwrap_or_default();
            let offset = LineStarts::new(&text).offset(line, column, Encoding::Utf32);
            Which::At((path, text, offset))
        }
    };
    let (texts, edits) = (HashMap::new(), HashMap::new());
    let answer = workspace.node(&cache::Open { texts: &texts, edits: &edits }, &which);
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
        let ours: [(&[&str], Subcommand); 4] = [
            (&["doctor"], Subcommand::Doctor),
            (&["doctor", "x"], Subcommand::Doctor),
            (&["select", "+m"], Subcommand::Select),
            (&["node", "x"], Subcommand::Node),
        ];
        for (line, subcommand) in ours {
            assert_eq!(named(&words(line)), Some(subcommand), "{line:?}");
        }
        let placed = SUBCOMMANDS.iter().enumerate().all(|(at, row)| row.subcommand as usize == at);
        assert!(placed, "each row of SUBCOMMANDS at its subcommand's place");
        let servers: [(&[&str], &[&str]); 5] = [
            (&["--", "doctor"], &["doctor"]),
            (&["--shadow", "select", "x"], &["select", "x"]),
            (&["dbt", "lsp"], &["dbt", "lsp"]),
            (
                &["--backend", "never", "--", "/p/dbt", "lsp", "--project-dir", "/p"],
                &["/p/dbt", "lsp", "--project-dir", "/p"],
            ),
            (&[], &["dbt", "lsp"]),
        ];
        for (line, server) in servers {
            assert_eq!(named(&words(line)), None, "{line:?}");
            let Ok(Arguments::Run { command, .. }) = modes::arguments(&words(line)) else { panic!("{line:?}") };
            assert_eq!(command, words(server), "{line:?}");
        }
    }

    #[test]
    fn a_subcommand_reads_its_operand_then_the_server_s_arguments_after_dashes() {
        let run = |operand: Option<&str>, launch: Launch, command: &[&str]| {
            Ok(Asked::Run { operand: operand.map(str::to_owned), launch, command: words(command) })
        };
        let default = ["dbt", "lsp"];
        assert_eq!(asked(Subcommand::Doctor, &[]), run(None, Launch::Auto, &default));
        assert_eq!(asked(Subcommand::Doctor, &words(&["x.sql"])), run(Some("x.sql"), Launch::Auto, &default));
        let helped: [(Subcommand, &[&str]); 4] = [
            (Subcommand::Doctor, &["-h"]),
            (Subcommand::Select, &["--help"]),
            (Subcommand::Doctor, &["--help", "--", "--backend", "never"]),
            (Subcommand::Select, &["-h", "x"]),
        ];
        for (subcommand, line) in helped {
            assert_eq!(asked(subcommand, &words(line)), Ok(Asked::Help), "{subcommand:?} {line:?}");
        }
        let given = words(&["--", "--backend", "never", "--", "uvx", "dbt", "lsp", "--target", "prod"]);
        assert_eq!(
            asked(Subcommand::Doctor, &given),
            run(None, Launch::Never, &["uvx", "dbt", "lsp", "--target", "prod"])
        );
        let selected = words(&["+m+", "--", "--shadow", "dbt", "lsp"]);
        assert_eq!(asked(Subcommand::Select, &selected), run(Some("+m+"), Launch::Auto, &default));
        let refused: [(Subcommand, &[&str]); 8] = [
            (Subcommand::Doctor, &["--bogus"]),
            (Subcommand::Doctor, &["x.sql", "-h"]),
            (Subcommand::Doctor, &["a", "b"]),
            (Subcommand::Select, &[]),
            (Subcommand::Node, &[]),
            (Subcommand::Select, &["--", "dbt", "lsp"]),
            (Subcommand::Doctor, &["--", "--mode", "x"]),
            (Subcommand::Doctor, &["--", "-V"]),
        ];
        for (subcommand, line) in refused {
            assert!(asked(subcommand, &words(line)).is_err(), "{subcommand:?} {line:?}");
        }
    }

    #[test]
    fn a_node_s_operand_is_a_file_s_place_else_a_unique_id() {
        let is_file = |path: &Path| path == Path::new("/w/shop/models/a.sql") || path == Path::new("/w/shop/x:1");
        let place = |path: &str, line: u64, column: u64| Operand::Place { path: path.into(), line, column };
        let a = "/w/shop/models/a.sql";
        let places = [
            ("models/a.sql", place(a, 0, 0)),
            ("models/a.sql:3", place(a, 2, 0)),
            ("models/a.sql:3:7", place(a, 2, 6)),
            ("./models/../models/a.sql:1:1", place(a, 0, 0)),
            ("/w/shop/models/a.sql", place(a, 0, 0)),
            ("x:1", place("/w/shop/x:1", 0, 0)),
        ];
        let ids = ["model.shop.a", "models/a.sql:0", "models/a.sql:3:x", "models/a.sql:1:2:3", "models/b.sql"];
        let expected: Vec<(&str, Operand)> =
            places.into_iter().chain(ids.map(|id| (id, Operand::Id(id.to_owned())))).collect();
        // Each operand asked beside what it reads as, so that a failure names it.
        let read: Vec<(&str, Operand)> =
            expected.iter().map(|(asked, _)| (*asked, operand(asked, Path::new("/w/shop"), is_file))).collect();
        assert_eq!(read, expected);
    }
}
