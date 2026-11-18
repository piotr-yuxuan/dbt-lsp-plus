// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Where the project and its dbt are, without `--project-dir`: the walk up
//! from a document over scratch layouts, and the lookup of dbt in the
//! `.venv`s of a scratch repository, with stub dbts.

use crate::support::{fixture, scratch, script};
use dbt_lsp_plus::locate::{self, Found, Git};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// Writes each of FILES under DIR, with its text.
fn write(dir: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

#[test]
fn a_document_s_project_is_the_nearest_one_above_that_is_no_package() {
    let dir = scratch("locate-walk");
    let project = "name: p\n";
    write(
        &dir,
        &[
            ("nested/repo/dbt/dbt_project.yml", project),
            ("worktree/repo/.worktree/x/dbt/dbt_project.yml", project),
            ("both/repo/dbt_project.yml", project),
            ("both/repo/.worktree/x/dbt_project.yml", project),
            ("install/dbt_project.yml", project),
            ("install/vendor/p/dbt_project.yml", project),
            ("side/root/dbt_project.yml", project),
            ("side/root/packages.yml", "packages:\n  - local: ../beside\n"),
            ("side/beside/dbt_project.yml", project),
            // A tab after a key: dbt may read it, the proxy cannot.
            ("unread/dbt_project.yml", "name:\tp\n"),
            ("unread/dbt_packages/q/dbt_project.yml", project),
        ],
    );
    let corpus = fixture().with_file_name("conformance");
    let (default, vendor) = (&["dbt", "lsp"][..], &["dbt", "lsp", "--packages-install-path", "vendor"][..]);
    let cases: [(&str, PathBuf, &[&str], Option<PathBuf>); 11] = [
        ("nested", dir.join("nested/repo/dbt/models/a.sql"), default, Some(dir.join("nested/repo/dbt"))),
        (
            "in a worktree",
            dir.join("worktree/repo/.worktree/x/dbt/models/a.sql"),
            default,
            Some(dir.join("worktree/repo/.worktree/x/dbt")),
        ),
        (
            "a worktree in a checkout is no package of its project",
            dir.join("both/repo/.worktree/x/models/a.sql"),
            default,
            Some(dir.join("both/repo/.worktree/x")),
        ),
        (
            "an installed package",
            fixture().join("dbt_packages/shop_utils/macros/is_positive.sql"),
            default,
            Some(fixture()),
        ),
        (
            "a local package",
            corpus.join("local-package/shared/models/s_m.sql"),
            default,
            Some(corpus.join("local-package")),
        ),
        (
            "a package's directory",
            corpus.join("package-directory/dbt_packages/zz/models/hm.sql"),
            default,
            Some(corpus.join("package-directory")),
        ),
        ("under the server's install path", dir.join("install/vendor/p/m.sql"), vendor, Some(dir.join("install"))),
        ("outside the install path", dir.join("install/vendor/p/m.sql"), default, Some(dir.join("install/vendor/p"))),
        ("under no dbt_project.yml", dir.join("nowhere/notes/x.yml"), default, None),
        ("a local package beside it", dir.join("side/beside/models/m.sql"), default, Some(dir.join("side/beside"))),
        (
            "above it, a project that does not read",
            dir.join("unread/dbt_packages/q/m.sql"),
            default,
            Some(dir.join("unread/dbt_packages/q")),
        ),
    ];
    for (name, path, command, expected) in cases {
        assert_eq!(locate::project_of(&path, &words(command)), expected, "{name}");
    }
}

/// What a stub dbt does when asked `lsp --help`.
const HAS_LSP: &str = "exit 0";
const NO_LSP: &str = "echo \"error: unrecognized subcommand 'lsp'\" >&2; exit 2";
const DBT_CORE: &str = "echo \"Error: No such command 'lsp'.\" >&2; exit 2";

/// A stub dbt at PATH that appends its path to LOG, then does ANSWER.
fn stub(path: &Path, answer: &str, log: &Path) {
    script(path, &format!("echo \"$0\" >> '{}'\n{answer}", log.display()));
}

/// A scratch repository for test NAME: `repo`, whose project is in `dbt`,
/// with a linked worktree in `.worktree/x`, made by a git with a scratch
/// home, no global nor system configuration, no repository above, the
/// tests' identity.  Returns the repository, a git for the lookup there,
/// and the file the stubs log to.
fn repository(name: &str) -> (PathBuf, Git, PathBuf) {
    let dir = scratch(name);
    let (repo, home) = (dir.join("repo"), dir.join("home"));
    fs::create_dir_all(&home).unwrap();
    let identity = ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"].map(|name| (name.into(), "tests".into()));
    let email = ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"].map(|name| (name.into(), "tests@example.invalid".into()));
    let fixed: [(OsString, OsString); 5] = [
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("HOME".into(), home.into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CEILING_DIRECTORIES".into(), dir.clone().into()),
    ];
    let env: Vec<(OsString, OsString)> = fixed.into_iter().chain(identity).chain(email).collect();
    write(&repo, &[("dbt/dbt_project.yml", "name: p\n"), (".gitignore", ".venv/\n.worktree/\n")]);
    let steps: [&[&str]; 4] = [
        &["init", "-q", "-b", "main", "."],
        &["add", "."],
        &["commit", "-q", "-m", "A project"],
        &["worktree", "add", "-q", ".worktree/x"],
    ];
    for args in steps {
        let status = Command::new("git").args(args).current_dir(&repo).env_clear().envs(env.iter().cloned()).status();
        assert!(status.unwrap().success(), "git {args:?}");
    }
    (repo, Git { program: "git".into(), env }, dir.join("stubs.log"))
}

const COMMAND: [&str; 3] = ["dbt", "lsp", "--no-send-anonymous-usage-stats"];

/// The dbt that FOUND runs, its links resolved, where it was found, and
/// whether it has `lsp`.
fn taken(found: &Found) -> (PathBuf, &'static str, Option<bool>) {
    (fs::canonicalize(&found.command[0]).unwrap(), found.source, found.lsp)
}

fn resolved(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[test]
fn dbt_is_the_project_s_before_the_repository_s() {
    let (repo, git, log) = repository("locate-dbt-project");
    let (project, command) = (repo.join("dbt"), words(&COMMAND));
    let (own, top) = (project.join(".venv/bin/dbt"), repo.join(".venv/bin/dbt"));
    stub(&own, HAS_LSP, &log);
    stub(&top, NO_LSP, &log);
    let found = locate::dbt(&project, &command, &git);
    assert_eq!(taken(&found), (resolved(&own), "the project's .venv", Some(true)));
    assert_eq!((&found.command[1..], found.named.as_str()), (&command[1..], own.to_str().unwrap()));
    fs::remove_file(&own).unwrap();
    let found = locate::dbt(&project, &command, &git);
    assert_eq!(taken(&found), (resolved(&top), "the repository's .venv", Some(false)), "dbt-oss, with no lsp");
}

#[test]
fn from_a_linked_worktree_dbt_is_the_main_worktree_s_project_s_then_its_own() {
    let (repo, git, log) = repository("locate-dbt-worktree");
    let (linked, command) = (repo.join(".worktree/x/dbt"), words(&COMMAND));
    let (main_project, main) = (repo.join("dbt/.venv/bin/dbt"), repo.join(".venv/bin/dbt"));
    stub(&main_project, HAS_LSP, &log);
    stub(&main, HAS_LSP, &log);
    let found = locate::dbt(&linked, &command, &git);
    assert_eq!(taken(&found), (resolved(&main_project), "the main worktree's project .venv", Some(true)));
    fs::remove_file(&main_project).unwrap();
    let found = locate::dbt(&linked, &command, &git);
    assert_eq!(taken(&found), (resolved(&main), "the main worktree's .venv", Some(true)));
}

#[test]
fn a_git_that_names_the_common_directory_from_where_it_ran_finds_the_main_worktree() {
    let (repo, git, log) = repository("locate-dbt-old-git");
    let (linked, command) = (repo.join(".worktree/x/dbt"), words(&COMMAND));
    let main_project = repo.join("dbt/.venv/bin/dbt");
    stub(&main_project, HAS_LSP, &log);
    // As git 2.30 answers in a linked worktree's subdirectory: a flag it does
    // not know printed back, the common directory relative to where it ran.
    let old_git = repo.with_file_name("git");
    let top = repo.join(".worktree/x");
    let answers = format!(
        "for word; do case $word in rev-parse) ;; --show-toplevel) echo '{}' ;; \
         --git-common-dir) echo ../../../.git ;; *) echo \"$word\" ;; esac; done",
        top.display()
    );
    script(&old_git, &answers);
    let found = locate::dbt(&linked, &command, &Git { program: old_git.into(), env: git.env });
    assert_eq!(taken(&found), (resolved(&main_project), "the main worktree's project .venv", Some(true)));
}

#[test]
fn a_dbt_that_cannot_run_or_has_no_lsp_subcommand_is_skipped_and_named() {
    let (repo, git, log) = repository("locate-dbt-skipped");
    let (project, command) = (repo.join("dbt"), words(&COMMAND));
    let (own, top) = (project.join(".venv/bin/dbt"), repo.join(".venv/bin/dbt"));
    stub(&own, HAS_LSP, &log);
    fs::set_permissions(&own, fs::Permissions::from_mode(0o644)).unwrap();
    stub(&top, DBT_CORE, &log);
    let found = locate::dbt(&project, &command, &git);
    assert_eq!((found.source, found.lsp, &found.command), ("PATH", None, &command));
    let skipped: Vec<(&PathBuf, bool)> = found.skipped.iter().map(|(path, why)| (path, why.is_empty())).collect();
    assert_eq!(skipped, [(&own, false), (&top, false)], "{:?}", found.skipped);
}

#[test]
fn without_git_dbt_is_the_project_s_then_path() {
    let (repo, git, log) = repository("locate-dbt-no-git");
    let missing = Git { program: "/nonexistent/git".into(), env: git.env };
    let (project, command) = (repo.join("dbt"), words(&COMMAND));
    let (own, top) = (project.join(".venv/bin/dbt"), repo.join(".venv/bin/dbt"));
    stub(&own, HAS_LSP, &log);
    stub(&top, HAS_LSP, &log);
    assert_eq!(locate::dbt(&project, &command, &missing).source, "the project's .venv");
    fs::remove_file(&own).unwrap();
    let found = locate::dbt(&project, &command, &missing);
    assert_eq!((found.source, &found.command, found.skipped.len()), ("PATH", &command, 0), "no repository's");
}

#[test]
fn a_first_word_other_than_dbt_is_kept_and_nothing_probed() {
    let (repo, git, log) = repository("locate-dbt-other");
    let project = repo.join("dbt");
    stub(&project.join(".venv/bin/dbt"), HAS_LSP, &log);
    let launcher = words(&["uvx", "dbt", "lsp"]);
    let found = locate::dbt(&project, &launcher, &git);
    assert_eq!((&found.command, found.source, found.lsp), (&launcher, "the server command", None));
    assert_eq!(found.named, "uvx dbt");
    assert!(!log.exists(), "a stub ran");
}
