// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Whether the user's dbt would install packages before it parses.  dbt
//! 2.0.9 and dbt-oss 2.0.11 install when a package is missing from the
//! install path, whatever the lock says: they write `package-lock.yml`,
//! rewrite the install path, deleting what it holds, untracked files
//! included, and reach the network for a package from it.  This guard
//! refuses such a parse.

use crate::yaml::{self, Tree};
use crate::{config, project, text};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The files of a project that name its packages.
const FILES: [&str; 2] = ["packages.yml", "dependencies.yml"];

fn items(doc: &Tree) -> &[Tree] {
    doc.get("packages").map_or(&[][..], Tree::items)
}

/// The directory of each `local:` package that DOCS name, as written,
/// from the root.
fn locals(docs: &[Tree]) -> impl Iterator<Item = &str> {
    docs.iter().flat_map(items).filter_map(|package| package.get("local")?.as_str())
}

/// The source of a package from the network, by which a lock names it too.
fn source(package: &Tree) -> Option<(&'static str, String)> {
    let keys = ["package", "git", "tarball", "private"];
    keys.into_iter().find_map(|key| Some((key, package.get(key)?.as_str()?.to_owned())))
}

/// The install path of the project at ROOT, as dbt takes it: SERVER's
/// `--packages-install-path`, else `DBT_PACKAGES_INSTALL_PATH`, set by
/// SERVER's words before `lsp` or in the environment the parse inherits,
/// else the project's `packages-install-path`, else `dbt_packages`; from the
/// root.  A `dbt_project.yml` that does not read is a refusal.
pub fn install_path(root: &Path, server: &[String]) -> Result<PathBuf, String> {
    let before = server.iter().take_while(|word| *word != "lsp");
    let assigned = before.filter_map(|word| word.strip_prefix("DBT_PACKAGES_INSTALL_PATH=")).last().map(str::to_owned);
    let variable = assigned.or_else(|| std::env::var("DBT_PACKAGES_INSTALL_PATH").ok()).filter(|path| !path.is_empty());
    let key = |config: Tree| Some(config.get("packages-install-path")?.as_str()?.to_owned());
    let configured = read(&root.join("dbt_project.yml"))?.and_then(key);
    let flagged = config::flag(server, "--packages-install-path").map(str::to_owned);
    Ok(root.join(flagged.or(variable).or(configured).unwrap_or_else(|| "dbt_packages".into())))
}

/// Whether a package named NAME is installed under INSTALL, as dbt sees it:
/// its project file there, not merely a directory.
fn installed(install: &Path, name: &str) -> bool {
    install.join(name).join("dbt_project.yml").is_file()
}

/// The YAML document at PATH, none for a file absent, empty or of comments
/// only; a file that does not read is a refusal, since dbt may read it.
fn read(path: &Path) -> Result<Option<Tree>, String> {
    let Some(text) = text::read(path) else { return Ok(None) };
    let empty = text.lines().map(str::trim).all(|line| line.is_empty() || line.starts_with('#'));
    match yaml::parse(&text) {
        Some(doc) => Ok(Some(doc)),
        None if empty => Ok(None),
        None => Err(format!(
            "dbt-lsp-plus cannot read {} as YAML, a tab after a key perhaps, which dbt may read",
            path.display()
        )),
    }
}

/// The documents that name the packages of the project at ROOT: its own,
/// then those of its local packages, in turn, each `local:` from the root,
/// as dbt resolves them.  One that repeats a key, or holds an alias or a
/// merge key, is a refusal: dbt reads the last value, and through them.
fn named(root: &Path) -> Result<Vec<Tree>, String> {
    let (mut docs, mut pending, mut seen) = (Vec::new(), vec![root.to_owned()], HashSet::new());
    while let Some(dir) = pending.pop() {
        let found: Vec<Tree> = FILES
            .iter()
            .map(|name| read(&dir.join(name)))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        if found.iter().any(|doc| doc.repeats() || doc.shares()) {
            return Err(format!(
                "the packages of {} repeat a key, or share one by an alias, as dbt-lsp-plus cannot follow",
                dir.display()
            ));
        }
        pending.extend(locals(&found).map(|local| root.join(local)).filter(|local| seen.insert(local.clone())));
        docs.extend(found);
    }
    Ok(docs)
}

/// The directory of each `local:` package of the project at ROOT, its own
/// and its local packages', in turn, each from the root as dbt reads it,
/// its `..` resolved by the file system: none when a file that names
/// packages does not read as dbt reads it, which only the guard refuses.
pub fn local_dirs(root: &Path) -> Vec<PathBuf> {
    let docs = named(root).unwrap_or_default();
    let resolved = |local: &str| {
        let dir = root.join(local);
        let up = Path::new(local).components().any(|part| part == Component::ParentDir);
        if up { fs::canonicalize(&dir).unwrap_or(dir) } else { dir }
    };
    locals(&docs).map(resolved).collect()
}

/// Why dbt would install packages before it parses the project at ROOT with
/// the server command SERVER, if it would, or the reason a parse is not
/// safe: a `local:` package, of the project or of one of its local packages,
/// with no project at `<install path>/<its project's name>`; a package from
/// the network, named by any of them, that a lock no older than the
/// project's files does not name; a package of the lock with no project at
/// `<install path>/<its name>`; a file that names packages, or the
/// project's, that does not read as dbt reads it.  A lock as recent as
/// those files but stale in content passes, and dbt then installs anew.
pub fn refused(root: &Path, server: &[String]) -> Option<String> {
    let lock = root.join("package-lock.yml");
    let read_all = || Ok::<_, String>((install_path(root, server)?, named(root)?, read(&lock)?));
    let (install, docs, locked) = match read_all() {
        Ok(found) => found,
        Err(refusal) => return Some(refusal),
    };
    let why = "dbt parse would install it, rewriting the install path; run dbt deps";
    // A package with no name dbt cannot install either.
    let missing =
        |dir: &&str| project::package_config(&root.join(dir)).is_none_or(|(project, _)| !installed(&install, &project));
    if let Some(dir) = locals(&docs).find(missing) {
        return Some(format!("the local package {dir} is not installed: {why}"));
    }
    let entries = locked.as_ref().map_or(&[][..], items);
    let modified = |path: &Path| fs::metadata(path).and_then(|meta| meta.modified()).ok();
    let files = FILES.map(|name| root.join(name));
    let fresh = modified(&lock).is_some_and(|lock| files.iter().all(|path| modified(path).is_none_or(|at| lock >= at)));
    let mut sources = docs.iter().flat_map(items).filter_map(source);
    if sources.any(|known| !fresh || !entries.iter().any(|entry| source(entry).as_ref() == Some(&known))) {
        return Some(format!("a package from the network is not in a lock as recent as the files naming it: {why}"));
    }
    let name = |entry: &Tree| entry.get("name").and_then(Tree::as_str).map(str::to_owned);
    if entries.iter().any(|entry| name(entry).is_none()) {
        return Some("package-lock.yml names no packages, as an older dbt wrote it: run dbt deps".into());
    }
    let absent = entries.iter().filter_map(name).find(|name| !installed(&install, name));
    absent.map(|name| format!("the locked package {name} is not installed: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::scratch_project;
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn packages_from_the_network_need_a_recent_lock_and_their_install() {
        let files = [
            ("packages.yml", "packages:\n  - package: dbt-labs/dbt_utils\n    version: 1.4.1\n"),
            ("dbt_packages/dbt_utils/dbt_project.yml", "name: dbt_utils\n"),
        ];
        let dir = scratch_project("settled", &files);
        let refused = |dir: &Path| super::refused(dir, &[]).unwrap_or_default();
        assert!(refused(&dir).starts_with("a package from the network is not in a lock"), "no lock");
        let lock = "packages:\n- package: dbt-labs/dbt_utils\n  name: dbt_utils\n  version: 1.4.1\nsha1_hash: x\n";
        fs::write(dir.join("package-lock.yml"), lock).unwrap();
        assert_eq!(super::refused(&dir, &[]), None, "locked and installed");
        let older = SystemTime::now() - Duration::from_secs(3600);
        File::options().write(true).open(dir.join("package-lock.yml")).unwrap().set_modified(older).unwrap();
        assert!(refused(&dir).starts_with("a package from the network"), "a lock older than packages.yml");
        fs::write(dir.join("package-lock.yml"), "packages: []\n").unwrap();
        assert!(refused(&dir).starts_with("a package from the network"), "a lock that does not name it");
        fs::write(dir.join("package-lock.yml"), "packages:\n- package: dbt-labs/dbt_utils\n  version: 1.4.1\n")
            .unwrap();
        assert!(refused(&dir).contains("as an older dbt wrote it"), "a lock with no names");
        fs::write(dir.join("package-lock.yml"), lock).unwrap();
        fs::remove_dir_all(dir.join("dbt_packages")).unwrap();
        assert!(refused(&dir).starts_with("the locked package dbt_utils is not installed"), "after dbt clean");
        fs::write(dir.join("packages.yml"), "# packages:\n#   - package: dbt-labs/dbt_utils\n").unwrap();
        fs::remove_file(dir.join("package-lock.yml")).unwrap();
        assert_eq!(super::refused(&dir, &[]), None, "comments only name no package");
        fs::write(dir.join("dependencies.yml"), "packages:\n  - git: https://example.invalid/x.git\n").unwrap();
        assert!(refused(&dir).starts_with("a package from the network"), "from git, in dependencies.yml");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Each layout that dbt 2.0.9 and dbt-oss 2.0.11 were seen to rewrite,
    /// deleting what the install path held (plan 011's critic, then 015's
    /// critic and review).
    #[test]
    fn a_local_package_not_installed_is_refused() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("packages.yml", "packages:\n  - local: dbt_packages/shop_utils\n"),
            ("dbt_packages/shop_utils/dbt_project.yml", "name: shop_utils\n"),
            ("dbt_packages/utils_dir/dbt_project.yml", "name: utils\n"),
            ("vendor/sibling/dbt_project.yml", "name: sibling\n"),
            ("helpers/dbt_project.yml", "name: helpers\n"),
        ];
        let dir = scratch_project("local-packages", &files);
        assert_eq!(super::refused(&dir, &[]), None, "installed where its name says");
        let names =
            |local: &str| fs::write(dir.join("packages.yml"), format!("packages:\n  - local: {local}\n")).unwrap();
        names("dbt_packages/shop_utils\n  - local: ./dbt_packages/utils_dir");
        let refused = super::refused(&dir, &[]).unwrap();
        assert!(refused.starts_with("the local package ./dbt_packages/utils_dir is not installed"), "{refused}");
        names("vendor/sibling");
        assert!(super::refused(&dir, &[]).is_some(), "a package outside the install path, not installed");
        // A directory with no project is not installed, to dbt.
        fs::create_dir_all(dir.join("dbt_packages/sibling/macros")).unwrap();
        assert!(super::refused(&dir, &[]).is_some(), "an install left without its project file");
        // As `dbt deps` installs it: a link under the install path.
        fs::remove_dir_all(dir.join("dbt_packages/sibling")).unwrap();
        std::os::unix::fs::symlink(dir.join("vendor/sibling"), dir.join("dbt_packages/sibling")).unwrap();
        assert_eq!(super::refused(&dir, &[]), None);
        // Its own packages, from the root: a local one not installed, then one from the network.
        fs::write(dir.join("vendor/sibling/packages.yml"), "packages:\n  - local: helpers\n").unwrap();
        assert!(super::refused(&dir, &[]).unwrap().starts_with("the local package helpers"), "a local package's own");
        fs::write(dir.join("vendor/sibling/packages.yml"), "packages:\n  - package: dbt-labs/dbt_utils\n").unwrap();
        assert!(super::refused(&dir, &[]).is_some(), "a package of a local package, with no lock");
        names("vendor/nothing");
        assert!(super::refused(&dir, &[]).is_some(), "a package with no project");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_install_path_is_dbt_s_and_a_file_that_does_not_read_refuses() {
        let files = [
            ("dbt_project.yml", "name: shop\npackages-install-path: deps\n"),
            ("packages.yml", "\u{feff}packages:\n  - local: dbt_packages/shop_utils\n"),
            ("dbt_packages/shop_utils/dbt_project.yml", "name: shop_utils\n"),
        ];
        let dir = scratch_project("install-path", &files);
        assert!(super::refused(&dir, &[]).is_some(), "not under the project's install path, behind a byte order mark");
        let flagged = words(&["dbt", "lsp", "--packages-install-path=dbt_packages"]);
        assert_eq!(super::refused(&dir, &flagged), None, "under the server's");
        let assigned = words(&["env", "DBT_PACKAGES_INSTALL_PATH=dbt_packages", "dbt", "lsp"]);
        assert_eq!(super::refused(&dir, &assigned), None, "set before the server's dbt");
        fs::write(dir.join("packages.yml"), "packages:\n  - package:\tdbt-labs/dbt_utils\n    version: 1.4.1\n")
            .unwrap();
        let refused = super::refused(&dir, &flagged).unwrap();
        assert!(refused.starts_with("dbt-lsp-plus cannot read"), "a tab dbt reads: {refused}");
        // dbt reads the last of two keys, and through an alias.
        fs::write(dir.join("packages.yml"), "packages: []\npackages:\n  - local: vendor/x\n").unwrap();
        assert!(super::refused(&dir, &flagged).unwrap().contains("repeat a key"));
        fs::write(dir.join("packages.yml"), "common: &all []\npackages: *all\n").unwrap();
        assert!(super::refused(&dir, &flagged).is_some(), "an alias");
        fs::write(dir.join("packages.yml"), "packages: []\n").unwrap();
        fs::write(dir.join("dbt_project.yml"), "name: shop\nmodels:\n  shop:\n    +materialized:\tview\n").unwrap();
        assert!(super::refused(&dir, &[]).is_some(), "a project file that does not read");
        fs::remove_dir_all(&dir).unwrap();
    }
}
