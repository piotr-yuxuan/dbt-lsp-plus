// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Where a session's project and its dbt are when the server command names
//! no `--project-dir`: the project of a document, from its path; the dbt of
//! a project, from its `.venv`, its repository's, its main worktree's, then
//! `PATH`; and the probe that tells a dbt 2 with `lsp`, one without, and
//! anything else apart.

use crate::{config, packages, project, run};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// The project of the document at PATH, as spelt: the nearest directory
/// above it holding `dbt_project.yml` that lies in no package of a
/// directory above it holding one; none outside every project.  A
/// directory lies in a package of one above when it is under that one's
/// install path, as dbt takes it with the server command COMMAND, or in
/// one of its local packages, as spelt or links resolved, as a package's
/// own `integration_tests` project does; the walk then goes on from the
/// next one above.  A project whose `dbt_project.yml` does not read has no
/// packages.  No walk stops at git's top level.
pub fn project_of(path: &Path, command: &[String]) -> Option<PathBuf> {
    let projects: Vec<&Path> = path.ancestors().filter(|dir| dir.join("dbt_project.yml").is_file()).collect();
    let mut found = *projects.first()?;
    for (at, outer) in projects.iter().enumerate().skip(1) {
        if !projects[at..].iter().any(|above| package_of(found, above, command)) {
            break;
        }
        found = outer;
    }
    Some(found.to_owned())
}

/// Whether INNER lies under DIR, as spelt or links resolved.
fn within(inner: &Path, dir: &Path) -> bool {
    let resolved = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    inner.starts_with(dir) || resolved(inner).starts_with(resolved(dir))
}

/// Whether INNER lies in a package of the project at OUTER: under its
/// install path, as dbt takes it with the server command COMMAND, or in
/// one of its local packages; never when OUTER's `dbt_project.yml` does
/// not read.
fn package_of(inner: &Path, outer: &Path, command: &[String]) -> bool {
    let Ok(install) = packages::install_path(outer, command) else { return false };
    within(inner, &install) || packages::local_dirs(outer).iter().any(|local| within(inner, local))
}

/// The root of the project that the document at PATH lies in, when it is
/// not the session's project at ROOT: none when PATH lies under ROOT or
/// in one of its packages (`package_of`), in no project, or while ROOT's
/// `dbt_project.yml` does not read as a project, whose own reason stands.
pub fn elsewhere(path: &Path, root: &Path, command: &[String]) -> Option<PathBuf> {
    if within(path, root) || project::package_config(root).is_none() || package_of(path, root, command) {
        return None;
    }
    project_of(path, command)
}

/// How git runs for the lookup: its program, and its whole environment,
/// from which `GIT_DIR` and `GIT_WORK_TREE` are left out, since they would
/// name another repository than the project's.
pub struct Git {
    pub program: OsString,
    pub env: Vec<(OsString, OsString)>,
}

/// Where the project at PROJECT lies in git, by one run of GIT there: its
/// work tree's top, and its main worktree's, the parent of its common
/// directory when that is named `.git`; none outside a repository, or when
/// git cannot run, fails or answers past the probe's deadline.  The common
/// directory may come relative to where git ran, as git before 2.31 gives
/// it, which knows no `--path-format`: it is read from PROJECT with its
/// links resolved, as git resolved them, then as written, each `..`
/// dropping the component before it, so that a `.git` reached through a
/// link keeps its name.
fn repository(project: &Path, git: &Git) -> Option<(PathBuf, Option<PathBuf>)> {
    let mut command = Command::new(&git.program);
    command.args(["rev-parse", "--show-toplevel", "--git-common-dir"]);
    command.current_dir(project).env_clear().envs(git.env.iter().map(|(name, value)| (name, value)));
    command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let stdout = child.stdout.take()?;
    // Two short lines, which the pipe holds until git has exited.
    let status = run::finish(child, config::PROBE_DEADLINE, &AtomicBool::new(false))?;
    let mut text = String::new();
    stdout.take(1 << 16).read_to_string(&mut text).ok()?;
    let lines: Vec<PathBuf> = text.lines().map(PathBuf::from).collect();
    let [top, common] = &lines[..] else { return None };
    if !status.success() || !top.is_absolute() {
        return None;
    }
    let ran = fs::canonicalize(project).unwrap_or_else(|_| project.to_owned());
    let common = ran.join(common).components().fold(PathBuf::new(), |mut path, part| {
        match part {
            Component::ParentDir => drop(path.pop()),
            Component::CurDir => {}
            other => path.push(other),
        }
        path
    });
    let main = common.parent().filter(|_| common.file_name() == Some(".git".as_ref()));
    Some((top.clone(), main.map(Path::to_owned)))
}

/// The places of the dbt of the project at PROJECT, in order and without
/// repeats, each with where it is: the project's `.venv`; then, from its
/// repository REPOSITORY, its top and its main worktree's, the top's
/// `.venv`, the main worktree's project's and the main worktree's own.
fn candidates(project: &Path, repository: Option<(PathBuf, Option<PathBuf>)>) -> Vec<(PathBuf, &'static str)> {
    let venv = |dir: &Path| dir.join(".venv/bin/dbt");
    let resolved = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let mut places = vec![(venv(project), "the project's .venv")];
    if let Some((top, main)) = repository {
        places.push((venv(&top), "the repository's .venv"));
        // git names its directories with their links resolved.
        let relative = resolved(project).strip_prefix(&top).map(Path::to_owned).ok();
        let main = main.iter().flat_map(|main| {
            let project =
                relative.as_ref().map(|relative| (venv(&main.join(relative)), "the main worktree's project .venv"));
            project.into_iter().chain([(venv(main), "the main worktree's .venv")])
        });
        places.extend(main);
    }
    let mut seen = HashSet::new();
    places.retain(|(path, _)| seen.insert(resolved(path)));
    places
}

/// The dbt that the server command of a project runs, as `dbt` found it.
#[derive(Debug)]
pub struct Located {
    /// The server command, with the dbt found in place of its first word.
    pub command: Vec<String>,
    /// The dbt as a line names it: its path, or the words before `lsp`.
    pub named: String,
    /// Where it was found.
    pub source: &'static str,
    /// Whether it has `lsp`, by its probe; none when it was not probed.
    pub lsp: Option<bool>,
    /// Each place skipped, and why.
    pub skipped: Vec<(PathBuf, String)>,
}

impl Located {
    /// The dbt of the server command COMMAND as written, unprobed: named by
    /// its words before `lsp`.
    pub fn written(command: &[String]) -> Located {
        let before = command.iter().take_while(|word| *word != "lsp").cloned().collect::<Vec<_>>().join(" ");
        Located {
            command: command.to_vec(),
            named: before,
            source: "the server command",
            lsp: None,
            skipped: Vec::new(),
        }
    }
}

/// The dbt of the project at PROJECT for the server command COMMAND.  When
/// COMMAND's first word is `dbt`, the first of the `candidates` that is a
/// file and answers its `probe`, in the project, as a dbt 2 does, with
/// `lsp` or without; one that answers otherwise is skipped; else `dbt`
/// stays, for `PATH`, unprobed, named by the first file of that name
/// there.  Any other first word is the user's choice, kept and unprobed.
/// GIT runs once, to find the project's repository.
pub fn dbt(project: &Path, command: &[String], git: &Git) -> Located {
    let mut found = Located::written(command);
    if command.first().map(String::as_str) != Some("dbt") {
        return found;
    }
    for (path, source) in candidates(project, repository(project, git)).into_iter().filter(|(path, _)| path.is_file()) {
        let mut words = command.to_vec();
        words[0] = path.to_string_lossy().into_owned();
        match probe(&words, Some(project)) {
            Probe::Other(why) => found.skipped.push((path, why)),
            answer => {
                let named = path.display().to_string();
                return Located { command: words, named, source, lsp: Some(answer == Probe::Lsp), ..found };
            }
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path).map(|dir| dir.join("dbt")).find(|dbt| dbt.is_file());
    Located { named: on_path.map_or_else(|| "dbt".into(), |dbt| dbt.display().to_string()), source: "PATH", ..found }
}

/// How a session found its project: the server command's `--project-dir`,
/// the client's folder, or a document in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Given,
    Folder,
    Document,
}

/// Why a session on ROUTE answers alone, NEVER saying that the backend is
/// `never`; none when dbt lsp starts behind the proxy.  NO_LSP, whether the
/// dbt has no `lsp`, is asked only where it decides: neither under `never`
/// nor on a document's route, where no dbt lsp starts.
pub fn alone(never: bool, route: Route, no_lsp: impl FnOnce() -> bool) -> Option<&'static str> {
    match (never, route) {
        (true, _) => Some("the backend is never"),
        (_, Route::Document) => Some("dbt lsp is not started for a project found from a file"),
        _ => no_lsp().then_some("it has no lsp"),
    }
}

/// What a dbt answers to `lsp --help`.
#[derive(Debug, PartialEq)]
pub enum Probe {
    /// Status 0, as dbt 2.0.9 prints its help.
    Lsp,
    /// Status 2 with "unrecognized subcommand 'lsp'" on the standard error,
    /// as dbt-oss 2.0.11 refuses it.
    NoLsp,
    /// Anything else, and how it went.
    Other(String),
}

/// What the dbt of COMMAND, a server command, answers: the words before
/// its `lsp` word, then `lsp --help`, with `NO_COLOR`, in DIR when given,
/// killed past `PROBE_DEADLINE`.  A launcher's own failure (`uvx dbt`
/// offline exits 2 too), dbt-core 1.x's "No such command 'lsp'", a
/// program that cannot run, a command with no word before `lsp`: other.
pub fn probe(command: &[String], dir: Option<&Path>) -> Probe {
    let Some(at) = command.iter().position(|word| word == "lsp").filter(|at| *at > 0) else {
        return Probe::Other("the command has no word before lsp".into());
    };
    let mut probe = Command::new(&command[0]);
    probe.args(&command[1..at]).args(["lsp", "--help"]).env("NO_COLOR", "1").env_remove("CLICOLOR_FORCE");
    if let Some(dir) = dir {
        probe.current_dir(dir);
    }
    let mut child = match probe.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(error) => return Probe::Other(format!("cannot run: {error}")),
    };
    let stderr = child.stderr.take().expect("piped stderr");
    let (sender, read) = mpsc::channel();
    // Read apart, so that neither a full pipe nor a launcher's child holding
    // it open holds the proxy.
    thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.take(1 << 16).read_to_end(&mut text);
        let _ = sender.send(text);
    });
    let status = run::finish(child, config::PROBE_DEADLINE, &AtomicBool::new(false));
    let stderr = read.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let refused = String::from_utf8_lossy(&stderr).contains("unrecognized subcommand 'lsp'");
    match (status, status.and_then(|status| status.code())) {
        (_, Some(0)) => Probe::Lsp,
        (_, Some(2)) if refused => Probe::NoLsp,
        (Some(status), _) => Probe::Other(format!("lsp --help gives {status}")),
        (None, _) => Probe::Other(format!("no answer to lsp --help within {} s", config::PROBE_DEADLINE.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn only_dbt_s_own_answers_tell_whether_it_has_lsp() {
        let refused = "echo \"error: unrecognized subcommand 'lsp'\" >&2";
        // dbt-oss 2.0.11 colours the word under CLICOLOR_FORCE, unless NO_COLOR is set.
        let coloured = format!(
            "[ -n \"$NO_COLOR\" ] || {{ printf \"unrecognized subcommand '\\033[33mlsp\\033[0m'\" >&2; exit 2; }}; \
             {refused}; exit 2"
        );
        // The probe takes only the words before `lsp`, and adds `--help`.
        let words = format!("[ \"$0 $1\" = 'lsp --help' ] && [ $# -eq 1 ] && {{ {refused}; exit 2; }}");
        // `sh -c SCRIPT lsp --help` runs SCRIPT, as a dbt answering its probe.
        let cases = [
            ("dbt-oss 2.0.11", format!("{refused}; exit 2"), "no lsp"),
            ("2.0.9 prints its help", "echo 'Start the dbt language server'; exit 0".into(), "lsp"),
            ("a launcher offline", "echo 'error: Failed to fetch the index' >&2; exit 2".into(), "other"),
            ("another status", format!("{refused}; exit 1"), "other"),
            ("dbt-core 1.x", "echo \"Error: No such command 'lsp'.\" >&2; exit 2".into(), "other"),
            ("only the words before lsp", words, "no lsp"),
            ("in colour unless asked not to", coloured, "no lsp"),
        ];
        let kind = |probed: Probe| match probed {
            Probe::Lsp => "lsp",
            Probe::NoLsp => "no lsp",
            Probe::Other(_) => "other",
        };
        for (name, script, expected) in cases {
            let command = args(&["sh", "-c", &script, "lsp", "--project-dir", "/p"]);
            assert_eq!(kind(probe(&command, None)), expected, "{name}");
        }
        let commands: [(&str, &[&str]); 3] = [
            ("a dbt that cannot start", &["/nonexistent/dbt", "lsp"]),
            ("a command with no lsp word", &["sh", "-c", "exit 2"]),
            ("no word before lsp", &["lsp", "--project-dir", "/p"]),
        ];
        for (name, command) in commands {
            assert_eq!(kind(probe(&args(command), None)), "other", "{name}");
        }
        let in_root = args(&["sh", "-c", "[ \"$(pwd)\" = / ] && exit 0; exit 1", "lsp"]);
        assert_eq!(kind(probe(&in_root, Some(Path::new("/")))), "lsp", "in the directory given");
    }

    #[test]
    fn a_session_is_alone_under_never_on_a_document_or_with_no_lsp_probed_only_then() {
        let never = "the backend is never";
        let document = "dbt lsp is not started for a project found from a file";
        let cases = [
            (true, Route::Given, false, Some(never), 0),
            (true, Route::Folder, true, Some(never), 0),
            (true, Route::Document, true, Some(never), 0),
            (false, Route::Document, false, Some(document), 0),
            (false, Route::Given, true, Some("it has no lsp"), 1),
            (false, Route::Folder, true, Some("it has no lsp"), 1),
            (false, Route::Given, false, None, 1),
            (false, Route::Folder, false, None, 1),
        ];
        for (never_set, route, no_lsp, expected, probes) in cases {
            let asked = std::cell::Cell::new(0);
            let probe = || {
                asked.set(asked.get() + 1);
                no_lsp
            };
            assert_eq!(alone(never_set, route, probe), expected, "{never_set} {route:?}");
            assert_eq!(asked.get(), probes, "probes of {never_set} {route:?}");
        }
    }

    #[test]
    fn the_places_of_a_dbt_follow_the_project_its_repository_and_the_main_worktree() {
        let places = |project: &str, repository: Option<(&str, Option<&str>)>| {
            let repository = repository.map(|(top, main)| (PathBuf::from(top), main.map(PathBuf::from)));
            let found = candidates(Path::new(project), repository);
            found.into_iter().map(|(path, source)| (path.to_string_lossy().into_owned(), source)).collect::<Vec<_>>()
        };
        let (project, repository) = ("the project's .venv", "the repository's .venv");
        let (main_project, main) = ("the main worktree's project .venv", "the main worktree's .venv");
        assert_eq!(places("/x/p", None), [("/x/p/.venv/bin/dbt".into(), project)], "outside git");
        // The main worktree: its project's place and its own are the first two.
        assert_eq!(
            places("/r/dbt", Some(("/r", Some("/r")))),
            [("/r/dbt/.venv/bin/dbt".into(), project), ("/r/.venv/bin/dbt".into(), repository)]
        );
        assert_eq!(places("/r", Some(("/r", Some("/r")))), [("/r/.venv/bin/dbt".into(), project)], "at the top");
        let linked = places("/r/.worktree/x/dbt", Some(("/r/.worktree/x", Some("/r"))));
        let expected = [
            ("/r/.worktree/x/dbt/.venv/bin/dbt".into(), project),
            ("/r/.worktree/x/.venv/bin/dbt".into(), repository),
            ("/r/dbt/.venv/bin/dbt".into(), main_project),
            ("/r/.venv/bin/dbt".into(), main),
        ];
        assert_eq!(linked, expected, "a linked worktree");
        // A common directory not named .git, as a bare repository's: no main worktree.
        let bare = places("/w/dbt", Some(("/w", None)));
        assert_eq!(bare, [("/w/dbt/.venv/bin/dbt".into(), project), ("/w/.venv/bin/dbt".into(), repository)]);
    }
}
