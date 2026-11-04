/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `GET /api/file-manager/zip?path=<dir>`: a directory handed to the page as one zip archive.
//!
//! A folder has no single file to fetch, so the archive is what a folder's Download button asks
//! for. The folder is packed to a temporary file first and the response starts once the whole
//! archive exists; the wait before the first byte is that packing, and the temporary is
//! unlinked as soon as it is open for streaming, so it outlives nothing. Symlinks are not
//! followed, FIFOs and devices are left out, and every file is read through the directory the
//! confinement walked, so no link planted before the walk or swapped in during it puts content
//! from outside the root into an archive the user asked for the tree's contents of. Off Linux
//! the directory is the resolved path the string confinement verified
//! ([`confine`'s module docs](super::confine)) and a leaf that is not a plain file is left out
//! the same way. The root is refused like every route that acts rather than lists,
//! view-only credentials are refused the archive like the listing and the bytes, and another
//! site's page is refused too: an image or a link there would spend the pack on the session's
//! cached login.
//!
//! The folder is bounded before anything is staged ([`ARCHIVE_MAX_SOURCE_BYTES`],
//! [`ARCHIVE_MAX_ENTRIES`]), because the pack runs to completion ahead of the first byte and
//! the pacing only throttles the network side; the pack holds to the bounds as it reads, so a
//! file that grows between the two cannot carry the archive past them. Packs run at most two
//! at a time, since each stages a full copy of the folder. The archive rides the download
//! pacing ([`transfer`](super::transfer)), streamed rather than served from the path, and the
//! route is GET alone: a `HEAD`, a `Range`, or a conditional request would want a file response
//! from a file that is gone, and each is refused before the folder is packed rather than after.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{Datelike, Timelike};
use zip::write::SimpleFileOptions;

#[cfg(target_os = "linux")]
use nix::fcntl::AtFlags;
#[cfg(target_os = "linux")]
use nix::fcntl::{OFlag, openat};
#[cfg(target_os = "linux")]
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;

use super::confine::{DirFd, FileRoot, local_time, mtime_pair};
use super::transfer::{Download, is_conditional, serve_paced_download};
use super::{FilesState, RequestAuth, json_error, query_param, text};

/// The most a folder archive's source can hold, in bytes.
pub const ARCHIVE_MAX_SOURCE_BYTES: u64 = 8 << 30;
/// The most a folder archive's source can hold, in entries.
pub const ARCHIVE_MAX_ENTRIES: usize = 100_000;
/// A file at least this large is packed as a zip64 entry, so one that grows past the 4 GiB a
/// plain entry can hold while it is read still packs; smaller ones stay plain entries, which
/// every unzipper reads.
pub const ARCHIVE_ZIP64_FROM: u64 = 1 << 30;

/// Why a folder could not be packed.
#[derive(Debug)]
pub enum ArchiveError {
    /// The folder holds more than one archive may carry; the entry that crossed the bound.
    TooLarge(String),
    Io(io::Error),
    Zip(zip::result::ZipError),
}

impl From<io::Error> for ArchiveError {
    fn from(e: io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

#[cfg(target_os = "linux")]
impl From<nix::Error> for ArchiveError {
    fn from(e: nix::Error) -> Self {
        ArchiveError::Io(e.into())
    }
}

impl From<zip::result::ZipError> for ArchiveError {
    fn from(e: zip::result::ZipError) -> Self {
        ArchiveError::Zip(e)
    }
}

/// An entry's `lstat` as the platform serves it to a walk: the stat struct on Linux, the
/// standard library's metadata off it, which carries the same answers through the helpers
/// below.
#[cfg(target_os = "linux")]
type EntryStat = FileStat;
#[cfg(not(target_os = "linux"))]
type EntryStat = std::fs::Metadata;

/// The kind of a status.
#[cfg(target_os = "linux")]
fn kind(st: &FileStat) -> SFlag {
    SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT
}

/// Whether a walked entry is a regular file, never a link, a FIFO, or a device.
#[cfg(target_os = "linux")]
fn entry_is_regular(st: &EntryStat) -> bool {
    kind(st) == SFlag::S_IFREG
}

/// Whether a walked entry is a regular file (the Linux body's docs).
#[cfg(not(target_os = "linux"))]
fn entry_is_regular(st: &EntryStat) -> bool {
    st.is_file()
}

/// A walked entry's size in bytes.
#[cfg(target_os = "linux")]
fn entry_size(st: &EntryStat) -> u64 {
    st.st_size as u64
}

/// A walked entry's size in bytes (the Linux body's docs).
#[cfg(not(target_os = "linux"))]
fn entry_size(st: &EntryStat) -> u64 {
    st.len()
}

/// A walked entry's modification time in whole seconds.
#[cfg(target_os = "linux")]
fn entry_mtime(st: &EntryStat) -> i64 {
    st.st_mtime
}

/// A walked entry's modification time in whole seconds (the Linux body's docs).
#[cfg(not(target_os = "linux"))]
fn entry_mtime(st: &EntryStat) -> i64 {
    mtime_pair(st).0
}

/// The permission bits a walked entry carries into the archive: its own on Linux, and the
/// archive's default off it, where a file holds no bits to carry.
#[cfg(target_os = "linux")]
fn entry_mode(st: &EntryStat) -> u32 {
    st.st_mode & 0o777
}

/// The permission bits a walked entry carries into the archive (the Linux body's docs).
#[cfg(not(target_os = "linux"))]
fn entry_mode(_st: &EntryStat) -> u32 {
    0o644
}

/// Walk the tree under `dir_fd` top-down, never descending a symlinked directory, calling
/// `visit` for every entry that is not a directory with its own directory's descriptor, its
/// name, its path relative to the top, and its `lstat`; `visit` returns `false` to stop.
#[cfg(target_os = "linux")]
fn walk(
    dir_fd: &DirFd,
    prefix: &Path,
    visit: &mut dyn FnMut(&DirFd, &OsStr, &Path, &EntryStat) -> bool,
) -> bool {
    let Ok(listing) = std::fs::read_dir(format!("/proc/self/fd/{}", dir_fd.as_raw_fd())) else {
        return true;
    };
    let mut subdirs = Vec::new();
    for entry in listing.flatten() {
        let name = entry.file_name();
        let Ok(st) = fstatat(
            Some(dir_fd.as_raw_fd()),
            name.as_os_str(),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) else {
            continue;
        };
        if kind(&st) == SFlag::S_IFDIR {
            subdirs.push(name);
            continue;
        }
        if !visit(dir_fd, &name, &prefix.join(&name), &st) {
            return false;
        }
    }
    for name in subdirs {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let Ok(raw) = openat(
            Some(dir_fd.as_raw_fd()),
            name.as_os_str(),
            flags,
            Mode::empty(),
        ) else {
            continue;
        };
        let sub = unsafe { OwnedFd::from_raw_fd(raw) };
        if !walk(&sub, &prefix.join(&name), visit) {
            return false;
        }
    }
    true
}

/// Walk the tree under `dir` top-down, never descending a symlinked directory, calling
/// `visit` for every entry that is not a directory with its directory's resolved path, its
/// name, its path relative to the top, and its `lstat`; `visit` returns `false` to stop. An
/// entry that vanishes mid-walk is skipped rather than failing the walk, and a directory whose
/// path no longer resolves is left unvisited, the resolved-string act of the whole off-Linux
/// side.
#[cfg(not(target_os = "linux"))]
fn walk(
    dir: &DirFd,
    prefix: &Path,
    visit: &mut dyn FnMut(&DirFd, &OsStr, &Path, &EntryStat) -> bool,
) -> bool {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return true;
    };
    let mut subdirs = Vec::new();
    for entry in listing.flatten() {
        let name = entry.file_name();
        let Ok(st) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if st.is_dir() {
            subdirs.push(name);
            continue;
        }
        if !visit(dir, &name, &prefix.join(&name), &st) {
            return false;
        }
    }
    for name in subdirs {
        if !walk(&dir.join(&name), &prefix.join(&name), visit) {
            return false;
        }
    }
    true
}

/// The total size and count of the regular files an archive of `dir_fd` would carry, by
/// `lstat` without reading contents, stopping as soon as either is past its bound; an entry
/// that vanishes mid-walk is skipped rather than failing the request.
pub fn archive_source_bounds(dir_fd: &DirFd, max_entries: usize) -> (u64, usize) {
    let (mut total, mut count) = (0u64, 0usize);
    walk(dir_fd, Path::new(""), &mut |_, _, _, st| {
        if !entry_is_regular(st) {
            return true;
        }
        total += entry_size(st);
        count += 1;
        total <= ARCHIVE_MAX_SOURCE_BYTES && count <= max_entries
    });
    (total, count)
}

/// A modification time as a zip entry's local date and time, clamped to the years 1980 to
/// 2107 that the entry can hold.
fn zip_date_time(mtime: i64) -> zip::DateTime {
    let t = local_time(mtime, 0);
    let stamp = (
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
    );
    let stamp = stamp.clamp((1980, 1, 1, 0, 0, 0), (2107, 12, 31, 23, 59, 59));
    zip::DateTime::from_date_and_time(
        stamp.0 as u16,
        stamp.1 as u8,
        stamp.2 as u8,
        stamp.3 as u8,
        stamp.4 as u8,
        stamp.5 as u8,
    )
    .unwrap_or_default()
}

/// A fresh temporary file for an archive, created for this user alone; its path for the
/// unlink once it is streaming.
fn staged_archive() -> io::Result<(std::fs::File, PathBuf)> {
    for _ in 0..16 {
        let path =
            std::env::temp_dir().join(format!("selkies-files-{:016x}.zip", rand::random::<u64>()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(target_os = "linux")]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free archive name",
    ))
}

/// Open the entry `name` in `dir` for reading without following a symlink, with the stat the
/// pack reads its size, time, and mode from, or `None` where it is not a regular file: a
/// link, a FIFO, or a device is left out of the archive rather than read.
#[cfg(target_os = "linux")]
fn open_regular(dir: &DirFd, name: &OsStr) -> Option<(std::fs::File, EntryStat)> {
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let raw = openat(Some(dir.as_raw_fd()), name, flags, Mode::empty()).ok()?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let st = fstat(fd.as_raw_fd()).ok()?;
    if kind(&st) != SFlag::S_IFREG {
        return None;
    }
    Some((std::fs::File::from(fd), st))
}

/// Open the entry `name` in `dir` for reading without following a symlink (the Linux body's
/// docs); the link check is the `lstat` the walk already holds.
#[cfg(not(target_os = "linux"))]
fn open_regular(dir: &DirFd, name: &OsStr) -> Option<(std::fs::File, EntryStat)> {
    let st = std::fs::symlink_metadata(dir.join(name)).ok()?;
    if !st.is_file() {
        return None;
    }
    let file = std::fs::File::open(dir.join(name)).ok()?;
    Some((file, st))
}

/// Write the regular files under `dir_fd` to a zip archive in the temporary directory and
/// return it open and already unlinked, so it lives exactly as long as the stream reading it.
///
/// A name that is not UTF-8 is carried with its stray bytes replaced; only the permission
/// bits ride along, no set-ID or sticky bit, into an archive another machine may extract; a
/// modification time an entry cannot hold is clamped rather than failing the pack.
pub fn build_directory_archive(
    dir_fd: &DirFd,
    max_entries: usize,
) -> Result<std::fs::File, ArchiveError> {
    let (staged, path) = staged_archive()?;
    let result = (|| -> Result<std::fs::File, ArchiveError> {
        let mut archive = zip::ZipWriter::new(staged);
        let mut carried = 0u64;
        let mut entries = 0usize;
        let mut failure: Option<ArchiveError> = None;
        walk(dir_fd, Path::new(""), &mut |dir, name, rel, _| {
            let Some((mut source, st)) = open_regular(dir, name) else {
                return true;
            };
            let entry_name = rel.to_string_lossy().into_owned();
            entries += 1;
            if entries > max_entries {
                failure = Some(ArchiveError::TooLarge(entry_name));
                return false;
            }
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .last_modified_time(zip_date_time(entry_mtime(&st)))
                .unix_permissions(entry_mode(&st))
                .large_file(entry_size(&st) >= ARCHIVE_ZIP64_FROM);
            if let Err(e) = archive.start_file(&entry_name, options) {
                failure = Some(e.into());
                return false;
            }
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = match source.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        failure = Some(e.into());
                        return false;
                    }
                };
                carried += n as u64;
                if carried > ARCHIVE_MAX_SOURCE_BYTES {
                    failure = Some(ArchiveError::TooLarge(entry_name));
                    return false;
                }
                if let Err(e) = archive.write_all(&buf[..n]) {
                    failure = Some(e.into());
                    return false;
                }
            }
            true
        });
        if let Some(e) = failure {
            return Err(e);
        }
        let mut file = archive.finish()?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))?;
        Ok(file)
    })();
    let _ = std::fs::remove_file(&path);
    result
}

/// Open the directory `parts` names under `root`, bound it, and pack it; the archive open and
/// unlinked, or `None` when the directory resolved outside the root. The bound comes first,
/// so a folder past the limits is refused before a byte is staged.
pub fn pack_directory(
    root: &FileRoot,
    parts: &[String],
    max_entries: usize,
) -> Result<Option<std::fs::File>, ArchiveError> {
    let Some((dir_fd, _)) = root.open_resolved_fd(parts)? else {
        return Ok(None);
    };
    let (total, count) = archive_source_bounds(&dir_fd, max_entries);
    if total > ARCHIVE_MAX_SOURCE_BYTES || count > max_entries {
        return Err(ArchiveError::TooLarge(parts.join("/")));
    }
    build_directory_archive(&dir_fd, max_entries).map(Some)
}

/// The handler (module docs).
pub async fn handle_zip(State(state): State<Arc<FilesState>>, request: Request) -> Response {
    let auth = RequestAuth::of(request.extensions());
    if auth.viewer_ceiling {
        return text(
            StatusCode::FORBIDDEN,
            "View-only credentials cannot download files",
        );
    }
    if !state.config.downloads {
        return text(StatusCode::FORBIDDEN, "Forbidden: downloads disabled");
    }
    if auth.fetched_cross_site(request.headers()) {
        log::warn!(
            target: "server",
            "Refused a folder archive to another site's page (Origin {:?})",
            request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("")
        );
        return text(StatusCode::FORBIDDEN, "Forbidden origin");
    }
    let rel = query_param(request.uri(), "path").unwrap_or_default();
    match state.root.resolve_file_path(&rel) {
        Some(target) if !state.root.at_file_root(&target) => {}
        _ => return json_error(StatusCode::NOT_FOUND, "not found"),
    }
    if request.method() != Method::GET
        || request.headers().contains_key(header::RANGE)
        || is_conditional(request.headers())
    {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "GET")]).into_response();
    }
    let parts = super::file_parts(&rel).unwrap_or_default();
    let Ok(_permit) = state.archive_pool.clone().acquire_owned().await else {
        return json_error(StatusCode::NOT_FOUND, "not found");
    };
    let root = state.root.clone();
    let limit = state.archive_entry_limit.load(Ordering::Relaxed);
    let packed = {
        let parts = parts.clone();
        tokio::task::spawn_blocking(move || pack_directory(&root, &parts, limit)).await
    };
    let file = match packed {
        Ok(Ok(Some(file))) => file,
        Ok(Err(ArchiveError::TooLarge(_))) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the folder holds too much to pack as one archive",
            );
        }
        Ok(Err(ArchiveError::Zip(e))) => {
            log::warn!(target: "server", "Packing {rel:?} failed: {e}");
            return json_error(StatusCode::NOT_FOUND, "not found");
        }
        _ => return json_error(StatusCode::NOT_FOUND, "not found"),
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let name = parts.last().cloned().unwrap_or_default();
    let download = Download {
        path: PathBuf::new(),
        file,
        size,
        audit_name: rel,
        download_name: format!("{name}.zip").into_bytes(),
        allow_fallback: false,
    };
    serve_paced_download(&state, &auth, request.method(), request.headers(), download).await
}
