// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! The scan of a project kept between requests: each file's text and what
//! it gives, reused while the file's metadata, read before its content, is
//! the same; the project built again only when a file or a package changed,
//! else shared.

use crate::project::{self, File, Inventory, Project, Scanned};
use std::collections::HashMap;
use std::fs::Metadata;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// What the cache knows a file by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A file on disk, by its metadata: a same-sized edit, a file replaced
    /// or a link turned to another file changes one of them.
    Disk { modified: (i64, i64), changed: (i64, i64), size: u64, inode: u64, device: u64 },
    /// An open document, by the proxy's count of its edits: a client's
    /// version starts again at each open.
    Open(u64),
}

impl Key {
    /// The key of a file on disk whose metadata is METADATA.
    pub fn of(metadata: &Metadata) -> Key {
        Key::Disk {
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            size: metadata.size(),
            inode: metadata.ino(),
            device: metadata.dev(),
        }
    }
}

/// What a project is built from, as a scan finds it: each package's name
/// and directory, which its `dbt_project.yml` gives, and each file in the
/// order of the scan, with its package, its layout and its key; no key for
/// a file whose metadata or content could not be read.
#[derive(Debug, Default, PartialEq)]
struct Seen {
    packages: Vec<(String, PathBuf)>,
    files: Vec<(PathBuf, usize, Option<usize>, Option<Key>)>,
}

/// Whether the project of NOW is the one of LAST, so that it need not be
/// built again: the same packages, the same files in the same order and
/// places, each with the same key, and every key known.
fn unchanged(last: &Seen, now: &Seen) -> bool {
    last == now && now.files.iter().all(|(.., key)| key.is_some())
}

/// The documents the editor has open: their text, which wins over their
/// file, and the proxy's count of each one's edits.
pub struct Open<'a> {
    pub texts: &'a HashMap<PathBuf, String>,
    pub edits: &'a HashMap<PathBuf, u64>,
}

impl Open<'_> {
    /// The key and text of the document at PATH, if it is open.
    fn get(&self, path: &Path) -> Option<(Key, &str)> {
        Some((Key::Open(*self.edits.get(path)?), self.texts.get(path)?.as_str()))
    }
}

/// The scan of a project, kept between requests.
#[derive(Default)]
pub struct Cache {
    /// Each file read, by path, with the key it was read under.
    files: HashMap<PathBuf, (Key, Arc<Scanned>)>,
    /// The root of the last project, and what it was built from.
    root: Option<PathBuf>,
    seen: Seen,
    project: Option<Rc<Project>>,
    /// How many times the project was built: what is derived from it, such
    /// as the graph, holds while this stays.
    pub generation: u64,
    /// The epoch of the last refresh: one a request, as the proxy counts.
    epoch: Option<u64>,
}

impl Cache {
    /// The project at ROOT, OPEN's documents winning over their files, as
    /// of EPOCH: looked at once an epoch, built again only when a file or a
    /// package changed, each file read again only when its key changed.  A
    /// failure while building leaves the cache as it was; a project that is
    /// no longer found keeps its files for when it is again.
    pub fn project(&mut self, root: &Path, epoch: u64, open: &Open) -> Option<Rc<Project>> {
        if self.epoch == Some(epoch) && self.root.as_deref() == Some(root) {
            return self.project.clone();
        }
        let Some(inventory) = project::find(root) else {
            self.project = None;
            return None;
        };
        let key = |found: &File| open.get(&found.path).map(|(key, _)| key).or(found.metadata.as_ref().map(Key::of));
        let place = |found: &File| (found.path.clone(), found.package, found.layout, key(found));
        let mut seen =
            Seen { packages: inventory.packages.clone(), files: inventory.files.iter().map(place).collect() };
        if self.project.is_some() && self.root.as_deref() == Some(root) && unchanged(&self.seen, &seen) {
            self.epoch = Some(epoch);
            return self.project.clone();
        }
        let (project, files) = self.build(&inventory, &seen, open);
        // A file that could not be read is tried again at the next request.
        for (path, .., key) in &mut seen.files {
            *key = key.filter(|_| files.contains_key(path.as_path()));
        }
        let project = Rc::new(project);
        (self.files, self.seen, self.root) = (files, seen, Some(root.to_owned()));
        (self.project, self.generation, self.epoch) = (Some(project.clone()), self.generation + 1, Some(epoch));
        Some(project)
    }

    /// The project INVENTORY finds, its files as SEEN: each kept file whose
    /// key is the same reused, any other read; and the files to keep, those
    /// read under a known key.
    fn build(
        &self,
        inventory: &Inventory,
        seen: &Seen,
        open: &Open,
    ) -> (Project, HashMap<PathBuf, (Key, Arc<Scanned>)>) {
        let mut files = HashMap::new();
        let keyed: HashMap<&Path, Key> =
            seen.files.iter().filter_map(|(path, .., key)| Some((path.as_path(), (*key)?))).collect();
        let mut read = |found: &File| {
            let key = keyed.get(found.path.as_path()).copied();
            let kept = self.files.get(&found.path).filter(|(was, _)| key == Some(*was)).map(|(_, kept)| kept.clone());
            let text = open.get(&found.path).map(|(_, text)| text);
            let scanned = kept.or_else(|| Scanned::read(&found.path, text).map(Arc::new));
            if let (Some(key), Some(scanned)) = (key, &scanned) {
                files.insert(found.path.clone(), (key, scanned.clone()));
            }
            scanned.unwrap_or_default()
        };
        let project = project::assemble(inventory, &mut read);
        (project, files)
    }

    /// Takes the files that FILLED read, as `fill` gives them, where none is
    /// kept yet: the first scan then reads only what changed since.
    pub fn adopt(&mut self, filled: Vec<(PathBuf, Key, Arc<Scanned>)>) {
        for (path, key, scanned) in filled {
            self.files.entry(path).or_insert((key, scanned));
        }
    }
}

/// Every file of the project at ROOT that reads from disk, with the key it
/// was read under: what a thread does before the first request needs it.
pub fn fill(root: &Path) -> Vec<(PathBuf, Key, Arc<Scanned>)> {
    let files = project::find(root).map_or_else(Vec::new, |inventory| inventory.files);
    let read = |found: File| {
        let key = Key::of(found.metadata.as_ref()?);
        Some((Arc::new(Scanned::read(&found.path, None)?), key, found.path))
    };
    files.into_iter().filter_map(read).map(|(scanned, key, path)| (path, key, scanned)).collect()
}

/// A stamp of the project at ROOT as it lies on disk: each package's name
/// and directory, and each file that may hold a resource
/// (`project::resource_file`) by its path and the key of its metadata.
/// The walk is `project::find`'s, which reads each package's
/// `dbt_project.yml` and each `packages.yml` and `dependencies.yml` that
/// names local packages, and no other file; open texts play no part, as
/// dbt parses the disk.  0 where ROOT holds no project.  It changes when
/// such a file is written, added, removed or replaced, and when a package
/// comes or goes, a `local:` one named included.
pub fn disk(root: &Path) -> u64 {
    let Some(inventory) = project::find(root) else { return 0 };
    let mut hasher = DefaultHasher::new();
    inventory.packages.hash(&mut hasher);
    for found in inventory.files.iter().filter(|found| project::resource_file(&found.path)) {
        (&found.path, found.metadata.as_ref().map(Key::of)).hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::scratch_project;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn disk(size: u64, inode: u64) -> Option<Key> {
        Some(Key::Disk { modified: (1, 0), changed: (1, 0), size, inode, device: 1 })
    }

    /// What a scan of the package NAME finds: FILES, each a path, its layout
    /// and its key.
    fn seen(name: &str, files: &[(&str, usize, Option<Key>)]) -> Seen {
        let place = |(path, layout, key): &(&str, usize, Option<Key>)| (PathBuf::from(path), 0, Some(*layout), *key);
        Seen { packages: vec![(name.to_owned(), PathBuf::from("/shop"))], files: files.iter().map(place).collect() }
    }

    #[test]
    fn a_project_is_built_again_only_when_a_file_or_a_package_changed() {
        let last = seen("shop", &[("a.sql", 0, disk(10, 1)), ("b.yml", 0, disk(20, 2))]);
        assert!(unchanged(&last, &seen("shop", &[("a.sql", 0, disk(10, 1)), ("b.yml", 0, disk(20, 2))])));
        let now = |files: &[(&str, usize, Option<Key>)]| seen("shop", files);
        assert!(!unchanged(&last, &now(&[("a.sql", 0, disk(11, 1)), ("b.yml", 0, disk(20, 2))])), "a file's size");
        assert!(!unchanged(&last, &now(&[("a.sql", 0, disk(10, 3)), ("b.yml", 0, disk(20, 2))])), "a file replaced");
        assert!(!unchanged(&last, &now(&[("a.sql", 0, disk(10, 1))])), "a file gone");
        assert!(!unchanged(&last, &now(&[("b.yml", 0, disk(20, 2)), ("a.sql", 0, disk(10, 1))])), "another order");
        assert!(!unchanged(&last, &now(&[("a.sql", 0, Some(Key::Open(1))), ("b.yml", 0, disk(20, 2))])), "opened");
        assert!(!unchanged(&last, &now(&[("a.sql", 2, disk(10, 1)), ("b.yml", 0, disk(20, 2))])), "another layout");
        let renamed = seen("store", &[("a.sql", 0, disk(10, 1)), ("b.yml", 0, disk(20, 2))]);
        assert!(!unchanged(&last, &renamed), "a package renamed, its files the same");
        let unknown = now(&[("a.sql", 0, None)]);
        assert!(!unchanged(&unknown, &unknown), "a file whose metadata or content is unknown is read every time");
    }

    #[test]
    fn keeps_what_is_unchanged_and_reads_what_changed() {
        let files = [
            ("dbt_project.yml", "name: shop\n"),
            ("models/a.sql", "select * from {{ ref('b') }}"),
            ("models/b.sql", "select 1"),
        ];
        let dir = scratch_project("cache", &files);
        let (texts, mut edits) = (HashMap::new(), HashMap::new());
        let mut cache = Cache::default();
        let first = cache.project(&dir, 1, &Open { texts: &texts, edits: &edits }).unwrap();
        assert!(Rc::ptr_eq(&first, &cache.project(&dir, 1, &Open { texts: &texts, edits: &edits }).unwrap()));
        let again = cache.project(&dir, 2, &Open { texts: &texts, edits: &edits }).unwrap();
        assert!(Rc::ptr_eq(&first, &again), "nothing changed: the same project");
        // Of another size: an edit of the same size may keep every key.
        fs::write(dir.join("models/b.sql"), "select 22").unwrap();
        let changed = cache.project(&dir, 3, &Open { texts: &texts, edits: &edits }).unwrap();
        assert_eq!(changed.text(&dir.join("models/b.sql")), Some("select 22"));
        let a = dir.join("models/a.sql");
        assert!(Arc::ptr_eq(&first.files[&a], &changed.files[&a]), "an unchanged file is not read again");
        // An open document wins, under its count of edits.
        let texts = HashMap::from([(a.clone(), "select 3".to_owned())]);
        edits.insert(a.clone(), 1);
        let opened = cache.project(&dir, 4, &Open { texts: &texts, edits: &edits }).unwrap();
        assert_eq!(opened.text(&a), Some("select 3"));
        fs::remove_file(dir.join("models/b.sql")).unwrap();
        let gone = cache.project(&dir, 5, &Open { texts: &texts, edits: &edits }).unwrap();
        assert!(gone.text(&dir.join("models/b.sql")).is_none(), "a deleted file dropped");
        assert_eq!(cache.generation, 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fill_gives_what_the_first_scan_then_reuses_but_a_file_changed_since() {
        let files = [("dbt_project.yml", "name: shop\n"), ("models/a.sql", "select 1"), ("models/b.sql", "select 1")];
        let dir = scratch_project("cache-fill", &files);
        let filled = fill(&dir);
        let a = dir.join("models/a.sql");
        let read = filled.iter().find(|(path, ..)| *path == a).map(|(_, _, scanned)| scanned.clone()).unwrap();
        fs::write(dir.join("models/b.sql"), "select 22").unwrap();
        let mut cache = Cache::default();
        cache.adopt(filled);
        let (texts, edits) = (HashMap::new(), HashMap::new());
        let project = cache.project(&dir, 1, &Open { texts: &texts, edits: &edits }).unwrap();
        assert!(Arc::ptr_eq(&project.files[&a], &read));
        assert_eq!(project.text(&dir.join("models/b.sql")), Some("select 22"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_cannot_be_read_is_tried_again_but_one_not_text_is_kept() {
        let dir =
            scratch_project("cache-unreadable", &[("dbt_project.yml", "name: shop\n"), ("models/a.sql", "select 1")]);
        // Not UTF-8, as a `Thumbs.db` that Windows leaves in a directory.
        fs::write(dir.join("models/Thumbs.db"), b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1\xff").unwrap();
        let (texts, edits) = (HashMap::new(), HashMap::new());
        let mut cache = Cache::default();
        let first = cache.project(&dir, 1, &Open { texts: &texts, edits: &edits }).unwrap();
        assert_eq!(first.text(&dir.join("models/Thumbs.db")), Some(""));
        cache.project(&dir, 2, &Open { texts: &texts, edits: &edits }).unwrap();
        assert_eq!(cache.generation, 1, "nothing changed: not built again");
        let unreadable = dir.join("models/b.sql");
        fs::write(&unreadable, "select 2").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(fs::read(&unreadable).is_err(), "run as a user whom permissions bind");
        cache.project(&dir, 3, &Open { texts: &texts, edits: &edits }).unwrap();
        cache.project(&dir, 4, &Open { texts: &texts, edits: &edits }).unwrap();
        assert_eq!(cache.generation, 3, "built again, the file read again");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_disk_stamp_changes_with_the_files_that_may_hold_a_resource() {
        let dir = scratch_project("cache-stamp", &[("dbt_project.yml", "name: shop\n"), ("models/a.sql", "select 1")]);
        let write = |path: &str, text: &str| {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        let first = super::disk(&dir);
        assert_eq!(super::disk(&dir), first, "nothing changed");
        // Outside the resource paths, hidden, an editor's auto-save and its backup.
        write("target/x.sql", "select 3");
        write(".written", "");
        write("models/#a.sql#", "select 4");
        write("models/a.sql~", "select 5");
        assert_eq!(super::disk(&dir), first, "files that hold no resource");
        write("models/a.sql", "select 22");
        let rewritten = super::disk(&dir);
        assert_ne!(rewritten, first, "a file rewritten at another size");
        write("models/b.sql", "select 2");
        assert_ne!(super::disk(&dir), rewritten, "a file added");
        fs::remove_file(dir.join("models/b.sql")).unwrap();
        assert_eq!(super::disk(&dir), rewritten, "the file added, then removed");
        write("packages.yml", "packages:\n  - local: vendor/shared\n");
        write("vendor/shared/dbt_project.yml", "name: shared\n");
        assert_ne!(super::disk(&dir), rewritten, "a local package named");
        write("dbt_project.yml", "name: [");
        assert_eq!(super::disk(&dir), 0, "a project that does not read");
        let none = scratch_project("cache-stamp-none", &[("models/a.sql", "select 1")]);
        assert_eq!(super::disk(&none), 0, "no project");
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&none).unwrap();
    }

    #[test]
    fn an_open_seed_is_not_read() {
        let files = [("dbt_project.yml", "name: shop\n"), ("models/a.sql", "select 1"), ("seeds/s.csv", "id\n1\n")];
        let dir = scratch_project("cache-seed", &files);
        let seed = dir.join("seeds/s.csv");
        let texts = HashMap::from([(seed.clone(), "{{ ref('a') }}".to_owned())]);
        let edits = HashMap::from([(seed.clone(), 1)]);
        let project = Cache::default().project(&dir, 1, &Open { texts: &texts, edits: &edits }).unwrap();
        assert_eq!(project.text(&seed), Some(""));
        assert!(project.files[&seed].calls.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
