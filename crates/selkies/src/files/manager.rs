/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The file manager's JSON routes: `GET /api/file-manager/browse?path=` lists one directory
//! and `POST /api/file-manager/mkdir` creates one.
//!
//! The listing is read-only, but it carries both gates of the HTML index: the download
//! permission, and the viewer ceiling, since a viewer watches the desktop and never reaches the
//! files on it. It reads the descriptor the confinement opened
//! ([`FileRoot::open_resolved_fd`](super::FileRoot::open_resolved_fd)), never a path re-walked
//! after the check, so a symlink swapped in after the path was validated cannot turn the
//! listing into one of a directory outside the root. A new folder is made inside a descriptor
//! of its parent that the confinement walked, so a symlink planted over an intermediate name
//! cannot move the creation outside the root; a name already taken is the `mkdir`'s own
//! `EEXIST`, not a check ahead of it that a concurrent request could invalidate. Uploads double
//! as the writable toggle: a deployment that disables uploads serves the directory read-only.
//! Another site's page is refused a new folder too: a form posting `text/plain` needs no
//! preflight, and the browser attaches a cached Basic login to it.

use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use nix::sys::stat::{Mode, mkdirat};
use serde_json::json;

use super::confine::{BrowseError, WalkError, browse_dir};
use super::{FilesState, RequestAuth, body_bytes, json_error, query_param, strerror};

/// `GET /api/file-manager/browse?path=<rel>` (module docs).
pub async fn handle_browse(State(state): State<Arc<FilesState>>, request: Request) -> Response {
    let auth = RequestAuth::of(request.extensions());
    if auth.viewer_ceiling {
        return json_error(
            StatusCode::FORBIDDEN,
            "View-only credentials cannot list files",
        );
    }
    if !state.config.downloads {
        return json_error(StatusCode::FORBIDDEN, "downloads disabled");
    }
    let rel_query = query_param(request.uri(), "path").unwrap_or_default();
    let parts = match super::file_parts(&rel_query) {
        Some(parts) if state.root.resolve_file_path(&rel_query).is_some() => parts,
        _ => return json_error(StatusCode::BAD_REQUEST, "invalid path"),
    };
    let rel = parts.join("/");
    let root = state.root.clone();
    let limit = state.listing_limit.load(Ordering::Relaxed);
    let listing = {
        let rel = rel.clone();
        tokio::task::spawn_blocking(move || browse_dir(&root, &parts, &rel, limit)).await
    };
    match listing {
        Ok(Ok(Some(entries))) => {
            axum::Json(json!({"status": "ok", "path": rel, "entries": entries})).into_response()
        }
        Ok(Ok(None)) => json_error(StatusCode::BAD_REQUEST, "invalid path"),
        Ok(Err(BrowseError::NotADirectory)) => json_error(StatusCode::NOT_FOUND, "not a directory"),
        Ok(Err(BrowseError::PermissionDenied)) => {
            json_error(StatusCode::FORBIDDEN, "permission denied")
        }
        Ok(Err(BrowseError::TooLarge)) => json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the folder holds too many entries to list",
        ),
        Ok(Err(BrowseError::Other(e))) => {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, &strerror(&e))
        }
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "listing failed"),
    }
}

/// The message for a refused file-modification request, or `None` when the session may
/// modify files: view-only credentials never do, and a deployment without uploads is
/// read-only.
fn viewer_or_disabled_files(state: &FilesState, auth: &RequestAuth) -> Option<&'static str> {
    if auth.viewer_ceiling {
        return Some("View-only credentials cannot modify files");
    }
    if !state.config.uploads || state.root.base().as_os_str().is_empty() {
        return Some("File modifications disabled");
    }
    None
}

/// `POST /api/file-manager/mkdir` with body `{"path": "<rel>"}` (module docs).
pub async fn handle_mkdir(State(state): State<Arc<FilesState>>, request: Request) -> Response {
    let auth = RequestAuth::of(request.extensions());
    if let Some(refused) = viewer_or_disabled_files(&state, &auth) {
        return json_error(StatusCode::FORBIDDEN, refused);
    }
    if auth.fetched_cross_site(request.headers()) {
        log::warn!(
            target: "server",
            "Refused a new folder to another site's page (Origin {:?})",
            request.headers().get("origin").and_then(|v| v.to_str().ok()).unwrap_or("")
        );
        return json_error(StatusCode::FORBIDDEN, "Forbidden origin");
    }
    let body = body_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let data: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let rel = match data.get("path") {
        Some(serde_json::Value::String(s))
            if !s.trim_matches(|c| c == '/' || c == '\\').is_empty() =>
        {
            s.clone()
        }
        _ => return json_error(StatusCode::BAD_REQUEST, "invalid path"),
    };
    let target = match state.root.resolve_file_path(&rel) {
        Some(target) if !state.root.at_file_root(&target) => target,
        _ => return json_error(StatusCode::BAD_REQUEST, "invalid path"),
    };
    drop(target);
    let parts = super::file_parts(&rel).unwrap_or_default();
    let root = state.root.clone();
    let made = tokio::task::spawn_blocking(move || -> Result<(), WalkError> {
        let Some((leaf, dirs)) = parts.split_last() else {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput).into());
        };
        let parent = root.open_dir_fd(dirs, true)?;
        mkdirat(
            parent.as_fd(),
            leaf.as_str(),
            Mode::from_bits_truncate(0o777),
        )?;
        Ok(())
    })
    .await;
    match made {
        Ok(Ok(())) => {}
        Ok(Err(WalkError::Escape(_))) => {
            return json_error(StatusCode::BAD_REQUEST, "invalid path");
        }
        Ok(Err(WalkError::Io(e))) => {
            return match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    json_error(StatusCode::CONFLICT, "already exists")
                }
                std::io::ErrorKind::NotADirectory => {
                    json_error(StatusCode::CONFLICT, "a file is in the way")
                }
                _ => {
                    let message = strerror(&e);
                    json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        if message.is_empty() {
                            "failed"
                        } else {
                            &message
                        },
                    )
                }
            };
        }
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
    state.audit("file.mkdir", json!({"filename": rel}));
    axum::Json(json!({"status": "ok"})).into_response()
}
