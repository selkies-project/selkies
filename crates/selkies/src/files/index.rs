/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `GET /api/files/...`: the file-manager tree served for download.
//!
//! Files are served with an attachment disposition, since inline types (text, images, PDF)
//! would otherwise render inside the dashboard's file-browser iframe instead of downloading;
//! directories render a styled HTML listing, redirected to their slash-terminated URL so the
//! relative links resolve one level down. The index exists solely to download files, so the
//! listing itself is gated behind the same `download` permission as the bytes, and view-only
//! credentials are refused both. Path validation rejects any traversal before touching the
//! filesystem and re-checks the resolved path against the root.
//!
//! The listing carries a parent row below the root and never at it: the dashboard's iframe
//! navigates by the listing's own anchors, and at the root a parent row is an escape hatch out
//! of the tree (on the nginx `/files/` mount its `../` resolves to the desktop page, whose load
//! registers a fresh primary client and kills the running session). The `<h1>` is always
//! `/api/files/...` whatever the subfolder; the footer script rewrites it from the mount, and
//! `window.__SELKIES_INJECTED_PATH_PREFIX__` tells it the disk root.

use std::sync::Arc;

use axum::extract::{OriginalUri, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_encode;

use super::confine::{IndexItem, realpath, scan_directory};
use super::index_html::{FILE_INDEX_FOOTER, FILE_INDEX_HEADER};
use super::transfer::{Download, QUOTE, serve_paced_download};
use super::{FilesState, RequestAuth, text};

/// `html.escape` with quotes: `&`, `<`, `>`, `"`, and `'`.
pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

/// The listing page for `rel_path` with `items`, the root's path injected for the footer.
pub fn render_index(rel_path: &str, items: &[IndexItem], root: &std::path::Path) -> String {
    let mut rows = String::new();
    for item in items {
        let href = percent_encode(&item.name, QUOTE).to_string();
        let label = html_escape(&String::from_utf8_lossy(&item.name));
        rows.push_str(&format!(
            "            <tr>\n                <td><a href=\"{href}\">{label}</a></td>\n                <td>{}</td>\n                <td>{}</td>\n            </tr>\n",
            html_escape(&item.mtime),
            html_escape(&item.size)
        ));
    }
    let injected = serde_json::to_string(&root.to_string_lossy()).unwrap_or_else(|_| "\"\"".into());
    format!(
        "{FILE_INDEX_HEADER}        /api/files/{}</h1>\n        <script>window.__SELKIES_INJECTED_PATH_PREFIX__ = {injected};</script>\n        <table id=\"list\">\n            <thead>\n                <tr>\n                    <th>File Name</th>\n                    <th>Date</th>\n                    <th>Size</th>\n                </tr>\n            </thead>\n            <tbody>\n{rows}            </tbody>\n        </table>\n{FILE_INDEX_FOOTER}",
        html_escape(rel_path)
    )
}

/// `GET /api/files/`: the root listing.
pub async fn handle_index_root(
    State(state): State<Arc<FilesState>>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Response {
    serve_index(state, uri, String::new(), request).await
}

/// `GET /api/files/{path}`: a file or a directory under the root.
pub async fn handle_index(
    State(state): State<Arc<FilesState>>,
    OriginalUri(uri): OriginalUri,
    Path(path): Path<String>,
    request: Request,
) -> Response {
    serve_index(state, uri, path, request).await
}

/// The handler shared by the root and the subpath routes (module docs).
async fn serve_index(
    state: Arc<FilesState>,
    uri: axum::http::Uri,
    path: String,
    request: Request,
) -> Response {
    let auth = RequestAuth::of(request.extensions());
    if auth.viewer_ceiling {
        return text(
            StatusCode::FORBIDDEN,
            "View-only credentials cannot list or download files",
        );
    }
    if !state.config.downloads {
        return text(StatusCode::FORBIDDEN, "Forbidden: downloads disabled");
    }
    let rel_path = path.trim_start_matches('/').to_string();
    let Some(parts) = super::file_parts(&rel_path) else {
        return text(
            StatusCode::FORBIDDEN,
            "Forbidden: Directory Traversal detected",
        );
    };
    let mut joined = state.root.base().to_path_buf();
    for part in &parts {
        joined.push(part);
    }
    let resolved = realpath(&joined);
    if !state.root.within(&resolved) {
        return text(
            StatusCode::FORBIDDEN,
            "Forbidden: Directory Traversal detected",
        );
    }
    let Ok(meta) = tokio::fs::metadata(&resolved).await else {
        return text(StatusCode::NOT_FOUND, "Not Found");
    };
    if meta.is_file() {
        let file = match tokio::fs::File::open(&resolved).await {
            Ok(file) => file.into_std().await,
            Err(e) => {
                return text(
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::NOT_FOUND
                    },
                    "Not Found",
                );
            }
        };
        let download = Download {
            path: resolved.clone(),
            file,
            size: meta.len(),
            audit_name: parts.join("/"),
            download_name: resolved
                .file_name()
                .map(|n| std::os::unix::ffi::OsStrExt::as_bytes(n).to_vec())
                .unwrap_or_default(),
            allow_fallback: true,
        };
        return serve_paced_download(&state, &auth, request.method(), request.headers(), download)
            .await;
    }
    if !uri.path().ends_with('/') {
        let mut location = format!("/{}/", uri.path().trim_start_matches('/'));
        if let Some(query) = uri.query() {
            location.push('?');
            location.push_str(query);
        }
        return match HeaderValue::from_str(&location) {
            Ok(value) => {
                (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, value)]).into_response()
            }
            Err(_) => text(StatusCode::NOT_FOUND, "Not Found"),
        };
    }
    let root = state.root.clone();
    let include_parent = resolved != *state.root.base();
    let scanned = {
        let resolved = resolved.clone();
        tokio::task::spawn_blocking(move || scan_directory(&resolved, include_parent, &root)).await
    };
    let items = match scanned {
        Ok(Ok(items)) => items,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return text(StatusCode::FORBIDDEN, "Permission Denied");
        }
        Ok(Err(e)) => return text(StatusCode::INTERNAL_SERVER_ERROR, &super::strerror(&e)),
        Err(_) => return text(StatusCode::INTERNAL_SERVER_ERROR, "listing failed"),
    };
    let html = render_index(&rel_path, &items, state.root.base());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}
