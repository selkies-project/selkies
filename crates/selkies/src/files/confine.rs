/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Confinement of every file-manager path to its root: the string check and the descriptor
//! act.
//!
//! [`FileRoot::resolve_file_path`] is the check a static path analyzer follows: a canonicalize-
//! then-prefix comparison that refuses traversal, a leaf symlink out of the tree, and a sibling
//! whose name merely extends the root's. The routes that act rather than list do not act on that
//! string: [`FileRoot::open_dir_fd`] walks the components through directory descriptors, opening
//! each relative to the one before and verifying where it landed from the descriptor itself
//! (`/proc/self/fd`, the kernel's own record of the object, which a name swapped after the open
//! cannot change), and the act is named through the held descriptor, so a symlink planted between
//! the check and the act cannot steer it outside the root, the one case the string check cannot
//! see. An in-tree symlink stays what it always was, a link the manager follows the way the
//! resolved path did.

use std::ffi::OsStr;
use std::fmt;
use std::fs::Metadata;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::{Mode, mkdirat};

/// The most entries the file manager's JSON listing returns for one folder: a folder past it
/// is refused rather than listed in part, and no request can make the server stat and
/// serialize an unbounded directory.
pub const FILE_LISTING_MAX_ENTRIES: usize = 50_000;

/// A component of a file-manager path resolved outside the root while the descriptor walk was
/// following it: the refusal the resolved string path gives statically, raised for the one case
/// that path cannot see, a symlink planted or swapped into the name space between the path's
/// validation and the act on it.
#[derive(Debug)]
pub enum WalkError {
    /// The named component led out of the root.
    Escape(String),
    /// The walk failed on the filesystem: a missing component, one that is not a directory.
    Io(io::Error),
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::Escape(part) => write!(f, "{part:?} resolves outside the file-manager root"),
            WalkError::Io(e) => e.fmt(f),
        }
    }
}

impl From<io::Error> for WalkError {
    fn from(e: io::Error) -> Self {
        WalkError::Io(e)
    }
}

impl From<nix::Error> for WalkError {
    fn from(e: nix::Error) -> Self {
        WalkError::Io(io::Error::from(e))
    }
}

/// Split a root-relative path into its name components, or `None` when it escapes the root.
///
/// Empty and `.` components are dropped, and a `..` surviving the normalization is the escape;
/// a path that normalizes to nothing is no components, which is the root itself. A NUL byte
/// names nothing on the filesystem and is refused with the escapes, rather than failing the
/// calls that would see it.
pub fn file_parts(rel: &str) -> Option<Vec<String>> {
    if rel.contains('\0') {
        return None;
    }
    let rel = rel.trim_matches(|c| c == '/' || c == '\\');
    let mut parts: Vec<&str> = Vec::new();
    for component in rel.split('/') {
        match component {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ => parts.push(".."),
            },
            name => parts.push(name),
        }
    }
    if !rel.is_empty() && parts.contains(&"..") {
        return None;
    }
    Some(parts.into_iter().map(str::to_owned).collect())
}

/// `path` with every symlink of its existing prefix resolved and the missing tail appended as
/// written, so a path that need not exist still resolves. A path that exists is
/// `std::fs::canonicalize`'s answer.
pub fn realpath(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    let mut tail: Vec<&OsStr> = Vec::new();
    let mut head = path;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(head) {
            let mut out = resolved;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                head = parent;
            }
            _ => return std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
        }
    }
}

/// Where a descriptor's open really landed, from the kernel's record of it.
pub fn fd_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// The file-manager root and the confinement of every path named under it.
#[derive(Clone, Debug)]
pub struct FileRoot {
    base: PathBuf,
}

impl FileRoot {
    /// The root at `base`, already canonical (`realpath`).
    pub fn new(base: PathBuf) -> Self {
        FileRoot { base }
    }

    /// The root directory.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Whether a resolved path sits at or under the root: the root itself, or one whose text
    /// starts with the root's followed by the separator, so a sibling whose name extends the
    /// root's is not read as inside it. A root of `/` is a prefix that already ends in one.
    pub fn within(&self, resolved: &Path) -> bool {
        let base = self.base.as_os_str().as_bytes();
        let candidate = resolved.as_os_str().as_bytes();
        if candidate == base {
            return true;
        }
        let mut prefix = base.to_vec();
        if prefix.last() != Some(&b'/') {
            prefix.push(b'/');
        }
        candidate.starts_with(&prefix)
    }

    /// Resolve a root-relative path to a filesystem path at or under the root, or `None` when
    /// it escapes. An empty path resolves to the root itself, which browse may name and mkdir
    /// refuses through [`FileRoot::at_file_root`].
    ///
    /// The confinement is the canonicalize-then-prefix comparison the static path checker
    /// recognizes: the parent of the leaf is canonicalized, and both it and the canonical leaf
    /// must sit at or under the root. The routes that act rather than list do not act on the
    /// returned string: they open the directory through [`FileRoot::open_dir_fd`] and
    /// [`FileRoot::open_resolved_fd`], whose descriptor holds the object this check validated.
    pub fn resolve_file_path(&self, rel: &str) -> Option<PathBuf> {
        let parts = file_parts(rel)?;
        let Some((leaf, dirs)) = parts.split_last() else {
            return Some(self.base.clone());
        };
        let mut parent = self.base.clone();
        for dir in dirs {
            parent.push(dir);
        }
        let parent = realpath(&parent);
        let full = parent.join(leaf);
        if !self.within(&parent) || !self.within(&realpath(&full)) {
            return None;
        }
        Some(full)
    }

    /// Open the directory `parts` names under the root as a descriptor, descending one
    /// component per `openat`, each named relative to the descriptor before it and verified
    /// from the descriptor it opened to rather than by the name it went in under.
    ///
    /// A symlinked component is followed the way the resolved string path followed it, and
    /// what makes that safe where the string was not is the verification: `/proc/self/fd`
    /// names where each open really landed. A component that resolves outside the root,
    /// planted before the walk or in the instant of it, ends the walk with
    /// [`WalkError::Escape`] instead of handing the caller an object to act outside with.
    /// With `create`, a missing component is made in place first; what the walk itself
    /// created is inside the descriptor it came from, so it needs no verification.
    pub fn open_dir_fd(&self, parts: &[String], create: bool) -> Result<OwnedFd, WalkError> {
        let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
        let mut fd = open(&self.base, dir_flags, Mode::empty())?;
        for part in parts {
            if matches!(part.as_str(), "" | "." | "..") || part.contains('/') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("not a name component: {part:?}"),
                )
                .into());
            }
            let opened = match openat(fd.as_fd(), part.as_str(), dir_flags, Mode::empty()) {
                Ok(opened) => opened,
                Err(nix::errno::Errno::ENOENT) if create => {
                    match mkdirat(fd.as_fd(), part.as_str(), Mode::from_bits_truncate(0o777)) {
                        Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                        Err(e) => return Err(e.into()),
                    }
                    openat(fd.as_fd(), part.as_str(), dir_flags, Mode::empty())?
                }
                Err(e) => return Err(e.into()),
            };
            let resolved = fd_path(&opened).unwrap_or_default();
            if !self.within(&resolved) {
                return Err(WalkError::Escape(part.clone()));
            }
            fd = opened;
        }
        Ok(fd)
    }

    /// Open the directory `parts` names under the root as a descriptor, with the canonical
    /// path it opened to, or `None` when that sits outside the root.
    ///
    /// The walk to the leaf verifies every intermediate component from the descriptor it
    /// opened ([`FileRoot::open_dir_fd`]); the leaf itself is opened following symlinks and
    /// verified the same way, so an in-tree link to another directory in the tree stays
    /// listable the way the resolved string path allowed it while one resolving outside is
    /// refused. `O_DIRECTORY` refuses a file or a FIFO before anything is read, so a FIFO
    /// cannot hold the open waiting for a writer; the error is `NotADirectory`.
    pub fn open_resolved_fd(&self, parts: &[String]) -> io::Result<Option<(OwnedFd, PathBuf)>> {
        let (leaf, dirs) = match parts.split_last() {
            Some(split) => split,
            None => (&String::new(), parts),
        };
        let fd = match self.open_dir_fd(dirs, false) {
            Ok(fd) => fd,
            Err(WalkError::Escape(_)) => return Ok(None),
            Err(WalkError::Io(e)) => return Err(e),
        };
        let fd = if leaf.is_empty() {
            fd
        } else {
            let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
            openat(fd.as_fd(), leaf.as_str(), flags, Mode::empty())?
        };
        let Ok(resolved) = fd_path(&fd) else {
            return Ok(None);
        };
        if !self.within(&resolved) {
            return Ok(None);
        }
        Ok(Some((fd, resolved)))
    }

    /// Whether `target` is the root itself rather than an entry within it.
    ///
    /// An empty request path, and the `.` and `a/..` spellings that normalize to one, all
    /// resolve to the root, so a route that acts rather than lists refuses it: a directory is
    /// only ever created inside another, and creating the one the manager is rooted at is not
    /// an operation a client can mean. Browse may still name the root.
    pub fn at_file_root(&self, target: &Path) -> bool {
        let normalize = |p: &Path| -> PathBuf {
            let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
            let mut out = PathBuf::new();
            for c in abs.components() {
                match c {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        out.pop();
                    }
                    other => out.push(other.as_os_str()),
                }
            }
            out
        };
        normalize(target) == normalize(&self.base)
    }
}

/// The status a file listing shows for the entry at `path`, and whether it is a directory.
///
/// A symbolic link is followed where it resolves at or under the root; one that leads out of
/// it is shown as the link itself, so no listing reports the kind, size, or date of a file
/// outside the root, which no route would serve either.
pub fn listing_status(path: &Path, root: &FileRoot) -> io::Result<(Metadata, bool)> {
    let lstat = std::fs::symlink_metadata(path)?;
    if lstat.file_type().is_symlink() && !root.within(&realpath(path)) {
        return Ok((lstat, false));
    }
    let stat = std::fs::metadata(path)?;
    let is_dir = stat.is_dir();
    Ok((stat, is_dir))
}

/// One row of the HTML file index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexItem {
    /// The entry's name as the filesystem spells it, `/` appended for a directory.
    pub name: Vec<u8>,
    /// `"12.3 KB"` for a file, `-` for a directory.
    pub size: String,
    /// `%Y-%m-%d %H:%M:%S` in local time, `-` for the parent row.
    pub mtime: String,
    pub is_dir: bool,
}

/// One directory's entries as the file index renders them, directories first then by
/// lowercase name, with the parent row first where `include_parent`.
pub fn scan_directory(
    path: &Path,
    include_parent: bool,
    root: &FileRoot,
) -> io::Result<Vec<IndexItem>> {
    let mut items = Vec::new();
    if include_parent {
        items.push(IndexItem {
            name: b"../".to_vec(),
            size: "-".into(),
            mtime: "-".into(),
            is_dir: true,
        });
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!(target: "server", "Skipping unreadable directory entry: {e}");
                continue;
            }
        };
        let (stat, is_dir) = match listing_status(&entry.path(), root) {
            Ok(status) => status,
            Err(e) => {
                log::warn!(target: "server", "Skipping unreadable directory entry {:?}: {e}", entry.file_name());
                continue;
            }
        };
        let mut name = entry.file_name().as_bytes().to_vec();
        if is_dir {
            name.push(b'/');
        }
        entries.push(IndexItem {
            name,
            size: if is_dir {
                "-".into()
            } else {
                format!("{:.1} KB", stat.len() as f64 / 1024.0)
            },
            mtime: local_time(stat.mtime(), stat.mtime_nsec())
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            is_dir,
        });
    }
    entries.sort_by(|a, b| {
        (!a.is_dir, a.name.to_ascii_lowercase()).cmp(&(!b.is_dir, b.name.to_ascii_lowercase()))
    });
    items.extend(entries);
    Ok(items)
}

/// A file time in the host's local zone.
pub fn local_time(secs: i64, nsec: i64) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(secs, nsec.clamp(0, 999_999_999) as u32)
        .single()
        .unwrap_or_else(|| {
            chrono::Local
                .timestamp_opt(0, 0)
                .single()
                .unwrap_or_default()
        })
}

/// One entry of the JSON listing.
#[derive(Clone, Debug, serde::Serialize)]
pub struct BrowseEntry {
    pub name: String,
    /// The root-relative path in normalized spelling.
    pub path: String,
    pub is_dir: bool,
    /// `None` for a directory.
    pub size: Option<u64>,
    /// Local time in ISO 8601 without a zone, microseconds only where they are not zero.
    pub mtime: String,
}

/// Why a JSON listing was refused.
#[derive(Debug)]
pub enum BrowseError {
    /// The entry is missing or is not a directory.
    NotADirectory,
    PermissionDenied,
    /// The folder holds more than [`FILE_LISTING_MAX_ENTRIES`].
    TooLarge,
    Other(io::Error),
}

impl From<io::Error> for BrowseError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => BrowseError::NotADirectory,
            io::ErrorKind::PermissionDenied => BrowseError::PermissionDenied,
            _ => BrowseError::Other(e),
        }
    }
}

/// Open the directory `parts` names and stat it into JSON entries, sorted directories first
/// then by lowercase name; `rel` is the normalized root-relative spelling for the `path`
/// fields. `None` when the entry resolved outside the root at open time, the race-only
/// spelling of the refusal [`FileRoot::resolve_file_path`] gives statically. The listing reads
/// the pinned directory through its `/proc/self/fd` path, the descriptor the confinement
/// opened, never a name that could have been swapped since the path was validated.
pub fn browse_dir(
    root: &FileRoot,
    parts: &[String],
    rel: &str,
    max_entries: usize,
) -> Result<Option<Vec<BrowseEntry>>, BrowseError> {
    let Some((dir_fd, _)) = root.open_resolved_fd(parts)? else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir_fd.as_raw_fd()))? {
        let Ok(entry) = entry else { continue };
        if entries.len() >= max_entries {
            return Err(BrowseError::TooLarge);
        }
        let Ok((stat, is_dir)) = listing_status(&entry.path(), root) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        entries.push(BrowseEntry {
            size: if is_dir { None } else { Some(stat.len()) },
            mtime: isoformat(stat.mtime(), stat.mtime_nsec()),
            name,
            path,
            is_dir,
        });
    }
    entries.sort_by(|a, b| {
        (!a.is_dir, a.name.to_lowercase()).cmp(&(!b.is_dir, b.name.to_lowercase()))
    });
    Ok(Some(entries))
}

/// A file time in ISO 8601 without a zone: local time, the microseconds present only where they are
/// not zero.
fn isoformat(secs: i64, nsec: i64) -> String {
    let t = local_time(secs, nsec);
    let micros = nsec.clamp(0, 999_999_999) / 1000;
    if micros == 0 {
        t.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        format!("{}.{micros:06}", t.format("%Y-%m-%dT%H:%M:%S"))
    }
}

#[cfg(test)]
mod tests {
    //! The confinement primitives and the descriptor walk.

    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    fn scratch(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn parts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn raises_escape(root: &FileRoot, p: &[&str]) -> bool {
        matches!(
            root.open_dir_fd(&parts(p), false),
            Err(WalkError::Escape(_))
        )
    }

    #[test]
    fn file_parts_normalizes_and_refuses_escapes() {
        assert_eq!(file_parts("../x"), None, "traversal is the escape");
        assert_eq!(
            file_parts("a/../../x"),
            None,
            "a buried round trip that still escapes"
        );
        assert_eq!(
            file_parts("./a/./b"),
            Some(parts(&["a", "b"])),
            "dot components drop out"
        );
        assert_eq!(file_parts("a/.."), Some(vec![]), "nothing left is the root");
        assert_eq!(
            file_parts("a\x00b"),
            None,
            "a NUL byte is refused rather than raised on"
        );
        assert_eq!(file_parts(""), Some(vec![]));
        assert_eq!(file_parts(".//real/"), Some(parts(&["real"])));
    }

    #[test]
    fn resolve_confines_to_the_root() {
        let base = scratch("selkies-confinement");
        let root = FileRoot::new(base.clone());
        std::fs::create_dir_all(base.join("sub dir")).unwrap();
        std::fs::write(base.join("sub dir/plain.bin"), b"0123456789").unwrap();
        assert_eq!(root.resolve_file_path("a\x00b"), None);
        assert_eq!(
            root.resolve_file_path(""),
            Some(base.clone()),
            "an empty path is the root itself"
        );
        assert_eq!(
            root.resolve_file_path("sub dir/plain.bin"),
            Some(base.join("sub dir/plain.bin"))
        );

        let sibling = base
            .parent()
            .unwrap()
            .join(format!("{}x", base.file_name().unwrap().to_str().unwrap()));
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("secret.txt"), "outside").unwrap();
        let sibling_rel = Path::new("..").join(sibling.file_name().unwrap());
        symlink(&sibling_rel, base.join("alias-dir")).unwrap();
        symlink(sibling_rel.join("secret.txt"), base.join("alias-file")).unwrap();
        assert_eq!(
            root.resolve_file_path("alias-dir/secret.txt"),
            None,
            "a sibling extending the root's name"
        );
        assert_eq!(root.resolve_file_path("alias-dir"), None);
        assert_eq!(
            root.resolve_file_path("alias-file"),
            None,
            "a leaf link to that sibling is refused"
        );
        assert!(
            raises_escape(&root, &["alias-dir"]),
            "the walk refuses the sibling link like the check"
        );
        std::fs::remove_dir_all(&sibling).unwrap();
        std::fs::remove_dir_all(&base).unwrap();

        let slash = FileRoot::new(PathBuf::from("/"));
        assert_eq!(
            slash.resolve_file_path("tmp"),
            Some(PathBuf::from("/tmp")),
            "a root of /"
        );
        let fd = slash.open_dir_fd(&parts(&["tmp"]), false).unwrap();
        assert_eq!(fd_path(&fd).unwrap(), PathBuf::from("/tmp"));
    }

    #[test]
    fn the_descriptor_walk_verifies_each_open() {
        let base = scratch("selkies-walk");
        let root = FileRoot::new(base.clone());
        std::fs::create_dir_all(base.join("real")).unwrap();
        std::fs::write(base.join("real/plain.txt"), b"payload").unwrap();
        symlink("real", base.join("docs")).unwrap();
        symlink(base.parent().unwrap(), base.join("away")).unwrap();

        let fd = root.open_dir_fd(&[], false).unwrap();
        assert!(fd_path(&fd).unwrap().is_dir(), "an empty walk is the root");
        assert!(matches!(
            root.open_dir_fd(&parts(&["missing"]), false),
            Err(WalkError::Io(_))
        ));
        let fd = root
            .open_dir_fd(&parts(&["missing", "deep"]), true)
            .unwrap();
        drop(fd);
        assert!(
            base.join("missing/deep").is_dir(),
            "create makes the missing intermediates in place"
        );
        let fd = root.open_dir_fd(&parts(&["docs"]), false).unwrap();
        assert_eq!(
            fd_path(&fd).unwrap().file_name().unwrap(),
            "real",
            "an in-tree link is followed"
        );
        assert!(
            raises_escape(&root, &["away"]),
            "a link out of the tree ends it"
        );

        let opened = root
            .open_resolved_fd(&parts(&["docs"]))
            .unwrap()
            .expect("in-tree leaf link opens");
        assert_eq!(opened.1, std::fs::canonicalize(base.join("real")).unwrap());
        assert!(
            root.open_resolved_fd(&parts(&["away"])).unwrap().is_none(),
            "a leaf link out is refused"
        );
        let opened = root
            .open_resolved_fd(&parts(&["real"]))
            .unwrap()
            .expect("a plain directory opens");
        assert_eq!(opened.1, base.join("real"));
        assert!(
            matches!(root.open_resolved_fd(&parts(&["absent"])), Err(e) if e.kind() == io::ErrorKind::NotFound)
        );
        std::fs::write(base.join("plain.txt"), b"payload").unwrap();
        assert!(
            matches!(root.open_resolved_fd(&parts(&["plain.txt"])), Err(e) if e.kind() == io::ErrorKind::NotADirectory),
            "a file is not a directory"
        );
        nix::unistd::mkfifo(&base.join("pipe"), Mode::from_bits_truncate(0o600)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let fifo_root = root.clone();
        std::thread::spawn(move || {
            tx.send(matches!(fifo_root.open_resolved_fd(&parts(&["pipe"])), Err(e) if e.kind() == io::ErrorKind::NotADirectory))
                .ok();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "a FIFO is refused at once, not waited on for a writer"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
