//! Background work for the file manager: copies, moves and deletions on one
//! thread, image thumbnails on another, both reporting through a channel and
//! a pipe the main loop polls so the desktop never blocks on the disk.

use std::collections::HashSet;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use tiny_skia::Pixmap;

/// Largest image file decoded for a thumbnail, in bytes.
const THUMBNAIL_MAX_BYTES: u64 = 64 << 20;

/// What the main thread asks for.
pub enum Job {
    /// Copy files into a directory.
    Copy {
        id: u64,
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// Move files into a directory.
    Move {
        id: u64,
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// Delete files and directories permanently.
    Delete { id: u64, paths: Vec<PathBuf> },
    /// Decode an image at a size.
    Thumbnail { key: ThumbKey },
}

/// Identifies a thumbnail: the file, its modification time and the pixel size.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThumbKey {
    /// The image file.
    pub path: PathBuf,
    /// Modification time in seconds, so an edited file is decoded again.
    pub modified: i64,
    /// Longer side in physical pixels.
    pub size: u32,
}

/// What the workers report back.
pub enum Message {
    /// An operation advanced.
    Progress {
        id: u64,
        done: usize,
        total: usize,
        current: String,
    },
    /// An operation finished, with what went wrong.
    Done { id: u64, errors: Vec<String> },
    /// A thumbnail was decoded, or could not be.
    Thumb { key: ThumbKey, image: Option<Pixmap> },
}

/// The channel to the workers and the pipe they wake the main loop with.
pub struct Worker {
    ops: Sender<Job>,
    thumbs: Sender<Job>,
    results: Receiver<Message>,
    wake_read: OwnedFd,
    pending: HashSet<ThumbKey>,
    next_id: u64,
}

impl Worker {
    /// Starts both worker threads.
    pub fn new() -> Worker {
        let mut fds = [0 as RawFd; 2];
        let ok = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } == 0;
        let (wake_read, wake_write) = if ok {
            unsafe {
                (
                    OwnedFd::from_raw_fd(fds[0]),
                    Some(Arc::new(OwnedFd::from_raw_fd(fds[1]))),
                )
            }
        } else {
            let null = std::fs::File::open("/dev/null").map(OwnedFd::from);
            (null.expect("/dev/null"), None)
        };
        let (result_tx, results) = channel::<Message>();
        let (ops, ops_rx) = channel::<Job>();
        let (thumbs, thumbs_rx) = channel::<Job>();
        for rx in [ops_rx, thumbs_rx] {
            let tx = result_tx.clone();
            let wake = wake_write.clone();
            std::thread::spawn(move || {
                while let Ok(job) = rx.recv() {
                    run(job, &tx, wake.as_deref());
                }
            });
        }
        Worker {
            ops,
            thumbs,
            results,
            wake_read,
            pending: HashSet::new(),
            next_id: 1,
        }
    }

    /// The descriptor that becomes readable when a message waits.
    pub fn fd(&self) -> RawFd {
        self.wake_read.as_raw_fd()
    }

    /// Queues a file operation.
    ///
    /// # Arguments
    /// * `make` - Builds the job from the number assigned to it.
    ///
    /// # Returns
    /// The operation's number, which its messages carry.
    pub fn submit(&mut self, make: impl FnOnce(u64) -> Job) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.ops.send(make(id));
        id
    }

    /// Queues a thumbnail unless the same one is already being made.
    ///
    /// # Arguments
    /// * `key` - The thumbnail wanted.
    pub fn request_thumbnail(&mut self, key: ThumbKey) {
        if self.pending.insert(key.clone()) {
            let _ = self.thumbs.send(Job::Thumbnail { key });
        }
    }

    /// Takes every message that arrived, clearing the wake pipe.
    pub fn drain(&mut self) -> Vec<Message> {
        let mut buf = [0u8; 64];
        while unsafe {
            libc::read(
                self.wake_read.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        } > 0
        {}
        let mut messages = Vec::new();
        while let Ok(message) = self.results.try_recv() {
            if let Message::Thumb { key, .. } = &message {
                self.pending.remove(key);
            }
            messages.push(message);
        }
        messages
    }
}

/// Runs one job on a worker thread.
fn run(job: Job, tx: &Sender<Message>, wake: Option<&OwnedFd>) {
    let send = |message: Message| {
        let _ = tx.send(message);
        if let Some(fd) = wake {
            let byte = [1u8];
            unsafe {
                libc::write(fd.as_raw_fd(), byte.as_ptr() as *const libc::c_void, 1);
            }
        }
    };
    match job {
        Job::Thumbnail { key } => {
            let image = decode_thumbnail(&key);
            send(Message::Thumb { key, image });
        }
        Job::Copy { id, sources, dest } => {
            let errors = transfer(id, &sources, &dest, false, &send);
            send(Message::Done { id, errors });
        }
        Job::Move { id, sources, dest } => {
            let errors = transfer(id, &sources, &dest, true, &send);
            send(Message::Done { id, errors });
        }
        Job::Delete { id, paths } => {
            let mut errors = Vec::new();
            let total = paths.len();
            for (done, path) in paths.iter().enumerate() {
                send(Message::Progress {
                    id,
                    done,
                    total,
                    current: file_name(path),
                });
                let result = if path.is_dir() && !path.is_symlink() {
                    std::fs::remove_dir_all(path)
                } else {
                    std::fs::remove_file(path)
                };
                if let Err(error) = result {
                    errors.push(format!("{}: {error}", file_name(path)));
                }
            }
            send(Message::Done { id, errors });
        }
    }
}

/// Decodes an image file scaled to a thumbnail.
fn decode_thumbnail(key: &ThumbKey) -> Option<Pixmap> {
    let metadata = std::fs::metadata(&key.path).ok()?;
    if metadata.len() > THUMBNAIL_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(&key.path).ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?.into_rgba8();
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let size = key.size.max(1);
    let (nw, nh) = if w >= h {
        (size, (h as u64 * size as u64 / w as u64).max(1) as u32)
    } else {
        ((w as u64 * size as u64 / h as u64).max(1) as u32, size)
    };
    let scaled = if (nw, nh) == (w, h) {
        decoded
    } else {
        image::imageops::thumbnail(&decoded, nw, nh)
    };
    crate::render::pixmap_from_rgba(nw, nh, scaled.as_raw())
}

/// Copies or moves the sources into a directory, naming a copy placed next
/// to its original and never overwriting what is already there.
fn transfer(id: u64, sources: &[PathBuf], dest: &Path, moving: bool, send: &dyn Fn(Message)) -> Vec<String> {
    let mut errors = Vec::new();
    let total: usize = sources.iter().map(|s| count_files(s)).sum::<usize>().max(1);
    let mut done = 0usize;
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        let same_dir = source.parent() == Some(dest);
        if moving && same_dir {
            continue;
        }
        if dest.starts_with(source) {
            errors.push(crate::i18n::trf(
                "{name}: cannot be placed inside itself",
                &[("name", &file_name(source))],
            ));
            continue;
        }
        let target = unique_name(dest, name.to_string_lossy().as_ref(), same_dir);
        send(Message::Progress {
            id,
            done,
            total,
            current: file_name(source),
        });
        if moving && std::fs::rename(source, &target).is_ok() {
            done += count_files(&target);
            continue;
        }
        if let Err(error) = copy_tree(source, &target, id, total, &mut done, send) {
            errors.push(format!("{}: {error}", file_name(source)));
            continue;
        }
        if moving {
            let result = if source.is_dir() && !source.is_symlink() {
                std::fs::remove_dir_all(source)
            } else {
                std::fs::remove_file(source)
            };
            if let Err(error) = result {
                errors.push(crate::i18n::trf(
                    "{name}: copied but not removed: {error}",
                    &[("name", &file_name(source)), ("error", &error.to_string())],
                ));
            }
        }
    }
    errors
}

/// Copies a file, symlink or directory tree, reporting each file.
fn copy_tree(
    source: &Path,
    target: &Path,
    id: u64,
    total: usize,
    done: &mut usize,
    send: &dyn Fn(Message),
) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        let link = std::fs::read_link(source)?;
        std::os::unix::fs::symlink(link, target)?;
        *done += 1;
        return Ok(());
    }
    if metadata.is_dir() {
        std::fs::create_dir(target)?;
        let mut entries: Vec<PathBuf> = std::fs::read_dir(source)?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for entry in entries {
            let Some(name) = entry.file_name() else {
                continue;
            };
            send(Message::Progress {
                id,
                done: *done,
                total,
                current: name.to_string_lossy().to_string(),
            });
            copy_tree(&entry, &target.join(name), id, total, done, send)?;
        }
        let _ = std::fs::set_permissions(target, metadata.permissions());
        return Ok(());
    }
    std::fs::copy(source, target)?;
    *done += 1;
    Ok(())
}

/// Counts the files under a path, directories themselves not counted.
fn count_files(path: &Path) -> usize {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return 1;
    }
    std::fs::read_dir(path)
        .map(|read| read.flatten().map(|e| count_files(&e.path())).sum())
        .unwrap_or(0)
}

/// A path in a directory that nothing occupies yet: the name itself, or the
/// name marked as a copy or numbered.
///
/// # Arguments
/// * `dir` - The directory.
/// * `name` - The wanted file name.
/// * `copy` - Whether the file is a copy beside its original, which gets
///   "(copy)" rather than a number.
pub fn unique_name(dir: &Path, name: &str, copy: bool) -> PathBuf {
    let direct = dir.join(name);
    if !direct.exists() && !direct.is_symlink() {
        return direct;
    }
    let (stem, ext) = split_extension(name);
    if copy {
        let candidate = dir.join(format!("{stem} (copy){ext}"));
        if !candidate.exists() && !candidate.is_symlink() {
            return candidate;
        }
    }
    for n in 2.. {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() && !candidate.is_symlink() {
            return candidate;
        }
    }
    direct
}

/// Splits a file name into its stem and its extension with the dot, keeping
/// a leading dot with the stem and treating a double archive extension as one.
///
/// # Arguments
/// * `name` - The file name.
pub fn split_extension(name: &str) -> (&str, &str) {
    let body = name.strip_prefix('.').unwrap_or(name);
    let offset = name.len() - body.len();
    for double in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if let Some(stem) = body.strip_suffix(double) {
            return (&name[..offset + stem.len()], &name[offset + stem.len()..]);
        }
    }
    match body.rfind('.') {
        Some(dot) if dot > 0 => (&name[..offset + dot], &name[offset + dot..]),
        _ => (name, ""),
    }
}

/// The last component of a path, for messages.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_extensions() {
        assert_eq!(split_extension("photo.jpg"), ("photo", ".jpg"));
        assert_eq!(split_extension("archive.tar.gz"), ("archive", ".tar.gz"));
        assert_eq!(split_extension(".bashrc"), (".bashrc", ""));
        assert_eq!(split_extension(".config.bak"), (".config", ".bak"));
        assert_eq!(split_extension("README"), ("README", ""));
    }

    #[test]
    fn unique_names_number_and_mark_copies() {
        let dir = std::env::temp_dir().join(format!("selkies-files-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"").unwrap();
        assert_eq!(unique_name(&dir, "b.txt", false), dir.join("b.txt"));
        assert_eq!(unique_name(&dir, "a.txt", true), dir.join("a (copy).txt"));
        assert_eq!(unique_name(&dir, "a.txt", false), dir.join("a (2).txt"));
        std::fs::write(dir.join("a (2).txt"), b"").unwrap();
        assert_eq!(unique_name(&dir, "a.txt", false), dir.join("a (3).txt"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn copies_and_moves_trees() {
        let dir = std::env::temp_dir().join(format!("selkies-files-ops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/inner")).unwrap();
        std::fs::write(dir.join("src/inner/f.txt"), b"hello").unwrap();
        std::fs::create_dir_all(dir.join("dest")).unwrap();
        let noop = |_: Message| {};
        let errors = transfer(1, &[dir.join("src")], &dir.join("dest"), false, &noop);
        assert!(errors.is_empty());
        assert_eq!(std::fs::read(dir.join("dest/src/inner/f.txt")).unwrap(), b"hello");
        let errors = transfer(2, &[dir.join("src")], &dir.join("dest"), true, &noop);
        assert!(errors.is_empty());
        assert!(dir.join("dest/src (2)/inner/f.txt").is_file());
        assert!(!dir.join("src").exists());
        let errors = transfer(3, &[dir.join("dest")], &dir.join("dest/src"), false, &noop);
        assert_eq!(errors.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
