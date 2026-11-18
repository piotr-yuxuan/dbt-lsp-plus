// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The subcommands, `doctor` and `select`: what an agent in a shell asks
//! without an LSP client, each answered on one line of compact JSON.  The
//! project and its dbt are found as the server finds them.

use crate::locate::{self, Probe, Route};
use crate::modes::{self, Arguments, Backend};
use crate::{commands, config, packages, proxy};
use serde_json::{Value, json};
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

/// A subcommand, named by the first word of the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subcommand {
    Doctor,
    Select,
}

/// The subcommand that WORDS, the command line after the program's name,
/// name by their first word; none when it names none, and the words are the
/// server's.
pub fn named(words: &[String]) -> Option<Subcommand> {
    match words.first().map(String::as_str) {
        Some("doctor") => Some(Subcommand::Doctor),
        Some("select") => Some(Subcommand::Select),
        _ => None,
    }
}

/// What a subcommand's words ask.
#[derive(Debug, PartialEq)]
enum Asked {
    Help,
    /// Its OPERAND, doctor's PATH or select's SELECTOR, and the server's
    /// backend and command.
    Run {
        operand: Option<String>,
        backend: Backend,
        command: Vec<String>,
    },
}

/// What WORDS, after SUBCOMMAND's name, ask: its usage, by `-h` or
/// `--help` first, whatever follows; else its operand, optional for doctor and needed by
/// select, then, after `--`, the server's arguments, as `modes::arguments`
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
    if subcommand == Subcommand::Select && operand.is_none() {
        return Err("select needs a SELECTOR".into());
    }
    match modes::arguments(server)? {
        Arguments::Run { backend, command, .. } => Ok(Asked::Run { operand, backend, command }),
        _ => Err("-h and -V are not the server's arguments here".into()),
    }
}

/// Runs SUBCOMMAND with WORDS, those after its name: its answer on the
/// standard output, one line of compact JSON, or its usage; a usage error on
/// the standard error, with status 2.
pub fn main(subcommand: Subcommand, words: &[String]) -> ExitCode {
    let usage = match subcommand {
        Subcommand::Doctor => DOCTOR_USAGE,
        Subcommand::Select => SELECT_USAGE,
    };
    let (operand, backend, command) = match asked(subcommand, words) {
        Ok(Asked::Run { operand, backend, command }) => (operand, backend, command),
        Ok(Asked::Help) => {
            println!("{usage}");
            return ExitCode::SUCCESS;
        }
        Err(why) => {
            eprintln!("dbt-lsp-plus: {why}\n\n{usage}");
            return ExitCode::from(2);
        }
    };
    let (answer, ok) = match (subcommand, operand) {
        (Subcommand::Doctor, path) => doctor(Path::new(path.as_deref().unwrap_or(".")), backend, &command),
        (Subcommand::Select, selector) => {
            let answer = select(&selector.unwrap_or_default(), &command);
            let ok = answer["error"].is_null();
            (answer, ok)
        }
    };
    // A reader that closed the pipe, as `| head`, wants no more and no panic.
    let _ = writeln!(std::io::stdout(), "{answer}");
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
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
fn dbt_of(project: &Path, route: Route, command: &[String]) -> (locate::Found, Option<PathBuf>) {
    if route == Route::Given {
        return (locate::Found::written(command), None);
    }
    let git = locate::Git { program: "git".into(), env: std::env::vars_os().collect() };
    let anchored = std::env::current_dir().map_or_else(|_| command.to_vec(), |cwd| config::anchored(command, &cwd));
    (locate::dbt(project, &anchored, &git), Some(project.to_owned()))
}

/// What the server would find and do from PATH with BACKEND and the server
/// command COMMAND, and whether it would work: its answer, and whether a
/// project was found whose dbt answers as a dbt 2 does.
fn doctor(path: &Path, backend: Backend, command: &[String]) -> (Value, bool) {
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
    let alone = locate::alone(backend == Backend::Never, route, || probed == Ok(false));
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
    if let commands::Via::Refused(failure) = commands::route(commands::NODES, &|_| false, config::can_cut()) {
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
    // What a select or a proxy killed by a signal left, swept after a day, as `Proxy::cache` does.
    config::remove_stale(&cache);
    let (job, run) = (commands::nodes_job(selector), cache.join("run"));
    let Some(line) = config::dbt_command(&dbt.command, &project, &run, true, &job.words) else {
        return commands::failure(commands::NODES, commands::NO_LSP_WORD);
    };
    let (sender, answer) = mpsc::channel();
    let report = Box::new(move |result| drop(sender.send(result)));
    let running = commands::start(job, line, dir, run, config::PARSE_DEADLINE, report);
    let ended = || commands::failure(commands::NODES, "the run ended without an answer");
    let result = answer.recv().unwrap_or_else(|_| ended());
    running.stop();
    let _ = std::fs::remove_dir_all(&cache);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn the_first_word_alone_names_a_subcommand() {
        let ours: [(&[&str], Subcommand); 3] = [
            (&["doctor"], Subcommand::Doctor),
            (&["doctor", "x"], Subcommand::Doctor),
            (&["select", "+m"], Subcommand::Select),
        ];
        for (line, subcommand) in ours {
            assert_eq!(named(&words(line)), Some(subcommand), "{line:?}");
        }
        let servers: [(&[&str], &[&str]); 6] = [
            (&["--", "doctor"], &["doctor"]),
            (&["--shadow", "select", "x"], &["select", "x"]),
            (&["node", "x"], &["node", "x"]),
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
        let run = |operand: Option<&str>, backend: Backend, command: &[&str]| {
            Ok(Asked::Run { operand: operand.map(str::to_owned), backend, command: words(command) })
        };
        let default = ["dbt", "lsp"];
        assert_eq!(asked(Subcommand::Doctor, &[]), run(None, Backend::Auto, &default));
        assert_eq!(asked(Subcommand::Doctor, &words(&["x.sql"])), run(Some("x.sql"), Backend::Auto, &default));
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
            run(None, Backend::Never, &["uvx", "dbt", "lsp", "--target", "prod"])
        );
        let selected = words(&["+m+", "--", "--shadow", "dbt", "lsp"]);
        assert_eq!(asked(Subcommand::Select, &selected), run(Some("+m+"), Backend::Auto, &default));
        let refused: [(Subcommand, &[&str]); 7] = [
            (Subcommand::Doctor, &["--bogus"]),
            (Subcommand::Doctor, &["x.sql", "-h"]),
            (Subcommand::Doctor, &["a", "b"]),
            (Subcommand::Select, &[]),
            (Subcommand::Select, &["--", "dbt", "lsp"]),
            (Subcommand::Doctor, &["--", "--mode", "x"]),
            (Subcommand::Doctor, &["--", "-V"]),
        ];
        for (subcommand, line) in refused {
            assert!(asked(subcommand, &words(line)).is_err(), "{subcommand:?} {line:?}");
        }
    }
}
