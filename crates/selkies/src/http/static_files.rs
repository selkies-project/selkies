/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The web client: where it is served from and how.
//!
//! A configured `web_root` wins when it holds an `index.html`; otherwise the bundled client
//! is served from [`bundle_dir`]. Nothing in `index.html` is rewritten or injected: the
//! client derives its route prefix from its own URL. The entry page and every other file of
//! the client's code whose name does not change with its content (`WEB_CODE` outside
//! `assets/`) are sent with `Cache-Control: no-cache`, so a browser revalidates them on every
//! load and never runs an old core under a new page after an upgrade; what the build names by
//! content hash under `assets/`, and the icons and manifest, keep heuristic caching. The
//! entry page is also where a browser that arrived on the IPv6 loopback is moved to the IPv4
//! one ([`super::listen::ipv6_loopback_redirect`]).

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use super::listen::{ConnInfo, ipv6_loopback_redirect};
use super::{AppState, WEB_CODE};

/// The directory the bundled web client is read from while the Python and Rust servers share
/// one tree: `SELKIES_WEB_BUNDLE` when set, else the Python package's data directory
/// `src/selkies/selkies_web` relative to this crate's manifest at build time. None when
/// neither holds an `index.html`.
pub fn bundle_dir() -> Option<PathBuf> {
    let configured = std::env::var("SELKIES_WEB_BUNDLE")
        .ok()
        .filter(|p| !p.trim().is_empty());
    let dir = match configured {
        Some(path) => PathBuf::from(path.trim()),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src/selkies/selkies_web"),
    };
    dir.join("index.html").is_file().then_some(dir)
}

/// Resolve the directory the web client is served from: `web_root` when it holds an
/// `index.html`, else the bundle; none, with a warning, when neither does.
pub fn resolve_static_root(web_root: &str) -> Option<PathBuf> {
    if !web_root.is_empty() {
        let path = expand_user(web_root);
        if path.is_dir() && path.join("index.html").is_file() {
            log::info!(target: "server", "Using custom web_root directory: {}", path.display());
            return Some(path);
        }
        log::warn!(target: "server", "web_root directory {} not found or missing index.html", path.display());
    }
    log::debug!(target: "server", "Defaulting to packaged web files.");
    let bundle = bundle_dir();
    if bundle.is_none() {
        log::warn!(target: "server", "Unable to find web content, skipping web routers handlers");
    }
    bundle
}

/// `~` and `~/...` as the home directory, like Python's `expanduser`.
fn expand_user(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest.trim_start_matches('/'));
    }
    PathBuf::from(path)
}

/// Whether a file of the client, by its root-relative name, is code a browser must
/// revalidate on every load.
pub fn revalidates(name: &str) -> bool {
    !name.starts_with("assets/") && WEB_CODE.iter().any(|ext| name.ends_with(ext))
}

fn no_cache(response: &mut Response) {
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-cache"));
}

/// The route for everything no API route took: `{prefix}/` is the entry page, after the
/// IPv6-loopback check, and `{prefix}/<file>` a file of the client; anything outside the
/// prefix, or with no web content found, is 404.
pub async fn fallback(
    State(state): State<AppState>,
    OriginalUri(original): OriginalUri,
    request: Request,
) -> Response {
    let prefix = state.config.subfolder.as_str();
    let Some(root) = state.static_root.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = original.path();
    let Some(rel) = path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (parts, _body) = request.into_parts();
    if rel.is_empty() {
        return index(&state, root, &parts).await;
    }
    let decoded = percent_encoding::percent_decode_str(rel)
        .decode_utf8_lossy()
        .into_owned();
    let (check_root, check_name) = (root.clone(), decoded.clone());
    let inside = tokio::task::spawn_blocking(move || resolves_inside(&check_root, &check_name));
    if !inside.await.unwrap_or(false) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut response = serve(
        ServeDir::new(root).append_index_html_on_directories(false),
        &parts,
        rel,
    )
    .await;
    if revalidates(&decoded) {
        no_cache(&mut response);
    }
    response
}

/// Whether `name` under `root` resolves to a file inside it: a symlink leading out of the
/// web root is not served, whatever it names.
fn resolves_inside(root: &Path, name: &str) -> bool {
    let (Ok(root), Ok(target)) = (root.canonicalize(), root.join(name).canonicalize()) else {
        return false;
    };
    target.starts_with(root)
}

/// The entry page: a browser on the IPv6 loopback asking for this host is moved to the IPv4
/// literal, every other request gets `index.html`.
async fn index(state: &AppState, root: &Path, parts: &Parts) -> Response {
    let conn = parts.extensions.get::<ConnInfo>();
    let path = format!("{}/", state.config.subfolder);
    if let Some(conn) = conn
        && let Some(target) = ipv6_loopback_redirect(conn, &parts.headers, &path, parts.uri.query())
    {
        return (StatusCode::FOUND, [(header::LOCATION, target)]).into_response();
    }
    let mut response = serve(ServeFile::new(root.join("index.html")), parts, "index.html").await;
    no_cache(&mut response);
    response
}

/// Run a tower-http file service on a copy of the request whose path is `rel`, keeping the
/// method and the conditional and range headers.
async fn serve<S>(service: S, parts: &Parts, rel: &str) -> Response
where
    S: tower::Service<
            Request,
            Response = Response<tower_http::services::fs::ServeFileSystemResponseBody>,
        >,
    S::Error: std::fmt::Debug,
{
    let uri: Uri = match format!("/{rel}").parse() {
        Ok(uri) => uri,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut copy = Request::builder().method(parts.method.clone()).uri(uri);
    for (name, value) in &parts.headers {
        copy = copy.header(name, value);
    }
    let Ok(copy) = copy.body(Body::empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match service.oneshot(copy).await {
        Ok(response) => response.map(Body::new),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revalidation_rule() {
        assert!(revalidates("index.html"));
        assert!(revalidates("selkies-core.js"));
        assert!(revalidates("codecs/x.wasm"));
        assert!(!revalidates("assets/app-abc123.js"));
        assert!(!revalidates("favicon.ico"));
        assert!(!revalidates("manifest.json"));
    }

    #[test]
    fn bundle_is_found_in_the_tree() {
        let dir = bundle_dir().expect("the web bundle beside the crate");
        assert!(dir.join("index.html").is_file());
    }
}
