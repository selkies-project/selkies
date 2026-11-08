/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `POST /api/upload`: a client file streamed to the file-manager directory over HTTP.
//!
//! Available in every streaming mode and not bounded by the data-channel or WebSocket
//! per-message size, so it saturates the link where the per-chunk SCTP path cannot. The
//! destination, relative to the file-manager root, arrives URL-encoded in `X-Upload-Path`.
//! Two request shapes share the route:
//!
//! - Plain: one POST carrying the whole file, no chunk headers, staged in a hidden sibling of
//!   the destination and renamed onto it when the body is complete.
//! - Chunked (the client slices files above its 64 MiB threshold so no single request body
//!   exceeds a fronting proxy's per-request cap): sequential POSTs for the same
//!   `X-Upload-Path`, each also carrying `X-Upload-Id` (an opaque client-chosen transfer id),
//!   `X-Upload-Offset` (the absolute byte offset of the slice), `X-Upload-Total` (the final
//!   size), and `X-Upload-Final: 1` on the last slice. Slices accumulate in a staging sibling
//!   that offset 0 creates and the transfer's tracking entry names; offset 0 also removes the
//!   one an abandoned transfer to the same path left, and a non-zero offset must exactly
//!   continue the tracked transfer (same id, offset equal to the bytes banked, matching staged
//!   size) or the transfer is discarded with 409. The final slice validates the accumulated
//!   size against `X-Upload-Total` and renames the staged file onto the destination
//!   atomically. Transfers idle past [`UPLOAD_PART_TTL_SECONDS`] are expired on the next
//!   chunked request.
//!
//! The write lands in a directory descriptor the confinement walked and verified
//! ([`FileRoot::open_dir_fd`](super::FileRoot::open_dir_fd)): every component down to it was
//! opened relative to the one before and verified from the descriptor it opened to, so a
//! symlink planted over an intermediate name after the request's path was validated cannot
//! redirect the body or the final rename outside the root, and `O_NOFOLLOW` on the staging
//! leaf does the same for the file itself. Both shapes carry the mode of a file of this user's
//! they replace onto the replacement ([`carry_destination_mode`]) and are refused for
//! view-only credentials. A chunked transfer is tracked by its destination, so a slice for a
//! path whose staging file another slice is still writing is refused rather than interleaved.

use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use nix::fcntl::AtFlags;
use nix::fcntl::{OFlag, openat, renameat};
use nix::sys::stat::{FchmodatFlags, Mode, SFlag, fchmodat, fstatat};
use nix::unistd::{UnlinkatFlags, unlinkat};
use serde_json::json;
use tokio::io::AsyncWriteExt;

use super::confine::WalkError;
use super::pacing::{TRANSFER_MIN_GAUGED_BYTES, monotonic};
use super::{ChunkState, FilesState, RequestAuth, json_error, strerror};

/// Idle chunked transfers older than this are reaped; generous relative to one 64 MiB slice,
/// so only a transfer whose client is truly gone expires.
pub const UPLOAD_PART_TTL_SECONDS: f64 = 3600.0;
/// Hidden staging sibling an upload is renamed from, so a destination is only ever replaced by
/// a complete file.
pub const UPLOAD_STAGING_PREFIX: &str = ".selkies-upload-";

/// A staging file name for an upload: fixed length and random rather than appended to the
/// destination basename, so a filename that is legal but sits close to the filesystem's
/// `NAME_MAX` still uploads, and another account sharing the directory cannot put a file or a
/// FIFO where the upload will open it.
pub fn staging_leaf_name() -> String {
    format!("{UPLOAD_STAGING_PREFIX}{:016x}.part", rand::random::<u64>())
}

/// Give the staged upload `staging` the permission bits of `dest`, the file it is about to
/// replace, both named relative to `dir_fd`, so re-uploading over an existing file keeps its
/// mode: an executable script stays executable and a private file stays private. A new
/// destination, one that is not a regular file, or one another account (not `euid`) owns keeps
/// the staging file's creation mode, and the set-ID and sticky bits are never carried: a file
/// another account planted under the name must not leave the upload writable by it, or a
/// set-ID program of this user's that it can run.
pub fn carry_destination_mode(dir_fd: &OwnedFd, staging: &str, dest: &str, euid: u32) {
    let Ok(st) = fstatat(dir_fd.as_fd(), dest, AtFlags::AT_SYMLINK_NOFOLLOW) else {
        return;
    };
    if SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG || st.st_uid != euid
    {
        return;
    }
    let mode = Mode::from_bits_truncate(st.st_mode & 0o777);
    if let Err(e) = fchmodat(dir_fd.as_fd(), staging, mode, FchmodatFlags::FollowSymlink) {
        log::debug!(target: "server", "Could not carry the mode of {dest} onto the staged upload: {e}");
    }
}

/// Rename the complete staged upload `staging` onto `dest`, both relative to `dir_fd`, the
/// held descriptor of the directory they live in, carrying its mode over: the rename moves the
/// object the confinement opened rather than re-walking a path that could have been swapped.
pub fn finalize_upload(dir_fd: &OwnedFd, staging: &str, dest: &str) -> std::io::Result<()> {
    carry_destination_mode(dir_fd, staging, dest, nix::unistd::geteuid().as_raw());
    renameat(dir_fd.as_fd(), staging, dir_fd.as_fd(), dest)?;
    Ok(())
}

/// Remove `leaf` under `dir_fd`, ignoring a file that is already gone or cannot go.
fn remove_quietly(dir_fd: &OwnedFd, leaf: &str) {
    let _ = unlinkat(dir_fd.as_fd(), leaf, UnlinkatFlags::NoRemoveDir);
}

/// The leaf of a staging path.
fn leaf_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Refuse the transfer, recording the attempt and why it was refused.
fn failed(state: &FilesState, rel: &str, message: &str, status: StatusCode) -> Response {
    state.audit(
        "file.upload.error",
        json!({"filename": rel, "error": message}),
    );
    json_error(status, message)
}

/// `409` for a slice that reaches a path whose staging file another slice is writing.
fn in_flight() -> Response {
    json_error(
        StatusCode::CONFLICT,
        "another chunk for this path is in flight",
    )
}

/// The handler (module docs).
pub async fn handle_upload(State(state): State<Arc<FilesState>>, request: Request) -> Response {
    let auth = RequestAuth::of(request.extensions());
    let headers = request.headers().clone();
    let rel = headers
        .get("x-upload-path")
        .map(|v| {
            percent_encoding::percent_decode(v.as_bytes())
                .decode_utf8_lossy()
                .into_owned()
        })
        .unwrap_or_default();
    if auth.viewer_ceiling {
        return failed(
            &state,
            &rel,
            "View-only credentials cannot upload files",
            StatusCode::FORBIDDEN,
        );
    }
    if !state.config.uploads || state.config.file_manager_path.is_empty() {
        return failed(&state, &rel, "uploads disabled", StatusCode::FORBIDDEN);
    }
    let parts = match super::file_parts(&rel) {
        Some(parts) if !parts.is_empty() => parts,
        _ => return failed(&state, &rel, "invalid upload path", StatusCode::BAD_REQUEST),
    };
    let mut dest = state.root.base().to_path_buf();
    for part in &parts {
        dest.push(part);
    }
    if !state.root.within(&dest) || state.root.at_file_root(&dest) {
        return failed(&state, &rel, "invalid upload path", StatusCode::BAD_REQUEST);
    }
    let leaf = parts.last().cloned().unwrap_or_default();
    let name = parts.join("/");

    let upload_id = headers
        .get("x-upload-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let offset_header = headers.get("x-upload-offset").and_then(|v| v.to_str().ok());
    if upload_id.is_some() != offset_header.is_some() {
        return failed(
            &state,
            &rel,
            "X-Upload-Id and X-Upload-Offset must be sent together",
            StatusCode::BAD_REQUEST,
        );
    }
    let mut offset = 0u64;
    let mut total: Option<u64> = None;
    if upload_id.is_some() {
        let total_header = headers
            .get("x-upload-total")
            .map(|v| v.to_str().unwrap_or("x"));
        match (
            offset_header.and_then(|o| o.trim().parse::<u64>().ok()),
            total_header,
        ) {
            (Some(o), None) => offset = o,
            (Some(o), Some(t)) => match t.trim().parse::<u64>() {
                Ok(t) => {
                    offset = o;
                    total = Some(t);
                }
                Err(_) => {
                    return failed(
                        &state,
                        &rel,
                        "malformed chunk headers",
                        StatusCode::BAD_REQUEST,
                    );
                }
            },
            (None, _) => {
                return failed(
                    &state,
                    &rel,
                    "malformed chunk headers",
                    StatusCode::BAD_REQUEST,
                );
            }
        }
    }

    let root = state.root.clone();
    let dirs: Vec<String> = parts[..parts.len() - 1].to_vec();
    let parent_fd = match tokio::task::spawn_blocking(move || root.open_dir_fd(&dirs, true)).await {
        Ok(Ok(fd)) => Arc::new(fd),
        Ok(Err(WalkError::Escape(_))) => {
            return failed(
                &state,
                &rel,
                "path escape rejected",
                StatusCode::BAD_REQUEST,
            );
        }
        Ok(Err(WalkError::Io(e))) => {
            return failed(
                &state,
                &rel,
                &format!("mkdir failed: {}", strerror(&e)),
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
        Err(_) => {
            return failed(
                &state,
                &rel,
                "mkdir failed",
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let body = request.into_body();

    let Some(upload_id) = upload_id else {
        let staging = staging_leaf_name();
        let written =
            match stream_upload_body(&state, &auth, declared, body, &staging, &parent_fd, false)
                .await
            {
                Ok(written) => written,
                Err(e) => {
                    let (fd, leaf) = (parent_fd.clone(), staging.clone());
                    let _ = tokio::task::spawn_blocking(move || remove_quietly(&fd, &leaf)).await;
                    return failed(&state, &rel, &e, StatusCode::BAD_REQUEST);
                }
            };
        let (fd, staging_leaf, dest_leaf) = (parent_fd.clone(), staging.clone(), leaf.clone());
        if let Ok(Err(e)) =
            tokio::task::spawn_blocking(move || finalize_upload(&fd, &staging_leaf, &dest_leaf))
                .await
        {
            let (fd, leaf) = (parent_fd.clone(), staging.clone());
            let _ = tokio::task::spawn_blocking(move || remove_quietly(&fd, &leaf)).await;
            return failed(
                &state,
                &rel,
                &format!("finalize failed: {}", strerror(&e)),
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
        log::info!(target: "server", "HTTP upload finished: {} ({written} bytes)", dest.display());
        state.audit(
            "file.upload.end",
            json!({"filename": name, "size_bytes": written}),
        );
        return axum::Json(json!({"status": "success", "bytes": written})).into_response();
    };

    expire_stale_chunked_uploads(&state);
    let final_slice = headers
        .get("x-upload-final")
        .map(|v| v.as_bytes() == b"1")
        .unwrap_or(false);
    let part_path = {
        let mut table = state.chunked.lock();
        if offset == 0 {
            if let Some(existing) = table.get(&dest) {
                if existing.busy {
                    return in_flight();
                }
                let part = existing.part.clone();
                table.remove(&dest);
                let _ = std::fs::remove_file(&part);
            }
            let part = dest.with_file_name(staging_leaf_name());
            table.insert(
                dest.clone(),
                ChunkState {
                    id: upload_id.clone(),
                    offset: 0,
                    ts: monotonic(),
                    part: part.clone(),
                    busy: true,
                },
            );
            part
        } else {
            let existing = table.get(&dest);
            if existing.is_some_and(|s| s.busy) {
                return in_flight();
            }
            let staged = existing.map(|s| std::fs::metadata(&s.part).map(|m| m.len()).ok());
            let continues = matches!(existing, Some(s) if s.id == upload_id && s.offset == offset)
                && staged.flatten() == Some(offset);
            if !continues {
                if let Some(s) = table.remove(&dest) {
                    let _ = std::fs::remove_file(&s.part);
                }
                drop(table);
                return failed(
                    &state,
                    &rel,
                    &format!("chunk sequence mismatch at offset {offset}; transfer discarded"),
                    StatusCode::CONFLICT,
                );
            }
            let entry = table.get_mut(&dest).expect("checked above");
            entry.busy = true;
            entry.part.clone()
        }
    };
    let part_leaf = leaf_of(&part_path);

    let written = match stream_upload_body(
        &state,
        &auth,
        declared,
        body,
        &part_leaf,
        &parent_fd,
        offset > 0,
    )
    .await
    {
        Ok(written) => written,
        Err(e) => {
            discard_chunked_upload(&state, &dest, &part_path);
            return failed(&state, &rel, &e, StatusCode::BAD_REQUEST);
        }
    };
    let received = {
        let mut table = state.chunked.lock();
        let Some(entry) = table.get_mut(&dest) else {
            return failed(&state, &rel, "transfer discarded", StatusCode::CONFLICT);
        };
        entry.offset = offset + written;
        entry.ts = monotonic();
        if !final_slice {
            entry.busy = false;
        }
        entry.offset
    };
    if !final_slice {
        return axum::Json(json!({"status": "success", "bytes": received, "complete": false}))
            .into_response();
    }
    if total.is_some_and(|t| t != received) {
        discard_chunked_upload(&state, &dest, &part_path);
        return failed(
            &state,
            &rel,
            &format!(
                "size mismatch: received {received}, expected {}",
                total.unwrap_or(0)
            ),
            StatusCode::BAD_REQUEST,
        );
    }
    let (fd, staging_leaf, dest_leaf) = (parent_fd.clone(), part_leaf.clone(), leaf.clone());
    if let Ok(Err(e)) =
        tokio::task::spawn_blocking(move || finalize_upload(&fd, &staging_leaf, &dest_leaf)).await
    {
        discard_chunked_upload(&state, &dest, &part_path);
        return failed(
            &state,
            &rel,
            &format!("finalize failed: {}", strerror(&e)),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    state.chunked.lock().remove(&dest);
    log::info!(target: "server", "HTTP chunked upload finished: {} ({received} bytes)", dest.display());
    state.audit(
        "file.upload.end",
        json!({"filename": name, "size_bytes": received}),
    );
    axum::Json(json!({"status": "success", "bytes": received, "complete": true})).into_response()
}

/// Drop a chunked transfer's tracking entry and its on-disk `.part` file together.
fn discard_chunked_upload(state: &FilesState, dest: &Path, part: &Path) {
    state.chunked.lock().remove(dest);
    let _ = std::fs::remove_file(part);
}

/// Reap transfers idle past [`UPLOAD_PART_TTL_SECONDS`] (entry and `.part` file).
fn expire_stale_chunked_uploads(state: &FilesState) {
    let now = monotonic();
    let mut table = state.chunked.lock();
    let stale: Vec<PathBuf> = table
        .iter()
        .filter(|(_, s)| now - s.ts > UPLOAD_PART_TTL_SECONDS && !s.busy)
        .map(|(k, _)| k.clone())
        .collect();
    for key in stale {
        if let Some(s) = table.remove(&key) {
            let _ = std::fs::remove_file(&s.part);
        }
        log::debug!(target: "server", "Expired stale chunked upload: {}", key.display());
    }
}

/// Stream a request body into the file `leaf` names in the open directory `dir_fd`.
///
/// Creates the file when `append` is false, refusing one that already exists, and appends when
/// true. `O_NOFOLLOW` blocks a symlink planted over the leaf's name, and no component above it
/// is re-walked by name. Enforces the declared `Content-Length`. Reads pace against two
/// allowances: a paused read fills the server's flow-control buffer, the TCP window closes,
/// and the client's uplink is freed for the input and feedback traffic the stream depends on.
/// The operator's static cap is the shared `transfer_cap` bucket; congestion control is
/// `upload_pacer` fed one gauge verdict per chunk, so the transfer takes whatever the uplink
/// has spare and backs off the moment the session's own delay inflates. A transfer below
/// `TRANSFER_MIN_GAUGED_BYTES` skips the gauge, as does one with no gauge-able session socket;
/// a gauge whose sockets all vanish mid-transfer stops pacing, since the session it protected
/// is gone. The chunks arrive as the HTTP stack delivers them; a gauged transfer's pacing is
/// applied per chunk at the size the stack read.
///
/// Returns the byte count written, or the failure's message; the caller owns removal of the
/// target file.
async fn stream_upload_body(
    state: &Arc<FilesState>,
    auth: &RequestAuth,
    declared: Option<u64>,
    body: Body,
    leaf: &str,
    dir_fd: &Arc<OwnedFd>,
    append: bool,
) -> Result<u64, String> {
    let cap = state
        .transfer_cap
        .active()
        .then(|| state.transfer_cap.clone());
    let mut gauge = if declared.is_some_and(|d| d >= TRANSFER_MIN_GAUGED_BYTES) {
        state.session_gauge(auth)
    } else {
        None
    };
    let mut flags = OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    flags |= if append {
        OFlag::O_APPEND
    } else {
        OFlag::O_EXCL
    };
    let (fd, leaf_name) = (dir_fd.clone(), leaf.to_string());
    let raw = tokio::task::spawn_blocking(move || {
        openat(
            fd.as_fd(),
            leaf_name.as_str(),
            flags,
            Mode::from_bits_truncate(0o644),
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let std_file = std::fs::File::from(raw);
    let mut file = tokio::fs::File::from_std(std_file);
    let mut written = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            continue;
        }
        if declared.is_some_and(|d| written + chunk.len() as u64 > d) {
            return Err("body exceeds declared Content-Length".into());
        }
        if let Some(cap) = &cap {
            cap.pace(chunk.len()).await;
        }
        if let Some(g) = &mut gauge {
            let verdict = g.sample();
            if g.alive() {
                let inflation = g.inflation_us();
                state
                    .upload_pacer
                    .pace_verdict(chunk.len(), verdict, Some(inflation))
                    .await;
            } else {
                gauge = None;
            }
        }
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        written += chunk.len() as u64;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    //! An upload carries the mode of a file it replaces only from this user's own file, never its
    //! set-ID or sticky bits.

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn carried(dest_mode: u32, foreign: bool) -> u32 {
        let root = super::super::testing::scratch("selkies-upload-mode");
        let dest = root.join("dest");
        let staging = root.join("staging");
        std::fs::write(&dest, b"").unwrap();
        std::fs::write(&staging, b"").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(dest_mode)).unwrap();
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o644)).unwrap();
        let dir_fd = std::fs::File::open(&root).unwrap();
        let dir_fd = OwnedFd::from(dir_fd);
        let euid = nix::unistd::geteuid().as_raw() + u32::from(foreign);
        carry_destination_mode(&dir_fd, "staging", "dest", euid);
        let mode = std::fs::metadata(&staging).unwrap().permissions().mode() & 0o7777;
        std::fs::remove_dir_all(&root).unwrap();
        mode
    }

    #[test]
    fn modes_carry_from_this_users_own_file_alone() {
        assert_eq!(
            carried(0o700, false),
            0o700,
            "this user's own file passes its permission bits on"
        );
        assert_eq!(carried(0o4755, false), 0o755, "set-UID is not carried");
        assert_eq!(carried(0o2775, false), 0o775, "set-GID is not carried");
        assert_eq!(
            carried(0o777, true),
            0o644,
            "another account's world-writable file passes nothing on"
        );
        assert_eq!(
            carried(0o4755, true),
            0o644,
            "another account's set-UID file passes nothing on"
        );
    }
}
