/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The web client: where it is served from and how.
//!
//! One source serves it, settled once at start ([`resolve`]): a configured `web_root` when it
//! holds an `index.html`; else the directory `SELKIES_WEB_BUNDLE` names, a client under
//! development served as it is rebuilt; else the client compiled into the executable
//! ([`super::embedded`]); else, in a build that embedded none, `dist/web` in the tree it was
//! built from, where `scripts/ci/build-web.sh` writes it. A directory is served by tower-http's
//! `ServeDir`, and a symbolic link leading out of it is not followed. Nothing in `index.html`
//! is rewritten or injected: the client derives its route prefix from its own URL. The entry
//! page and every other file of the client's code whose name does not change with its content
//! (`WEB_CODE` outside `assets/`) are sent with `Cache-Control: no-cache`, so a browser
//! revalidates them on every load and never runs an old core under a new page after an
//! upgrade; what the build names by content hash under `assets/`, and the icons and manifest,
//! keep heuristic caching. The entry page is also where a browser that arrived on the IPv6
//! loopback is moved to the IPv4 one ([`super::listen::ipv6_loopback_redirect`]).

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use super::embedded::{self, EMBEDDED, EmbeddedClient};
use super::listen::{ConnInfo, ipv6_loopback_redirect};
use super::{AppState, WEB_CODE};

/// Where the web client is served from.
#[derive(Clone, Debug)]
pub enum WebClient {
    /// A directory holding an `index.html`.
    Directory(PathBuf),
    /// A table compiled into the executable.
    Embedded(&'static EmbeddedClient),
}

impl WebClient {
    /// The client a server with no `web_root` serves: `SELKIES_WEB_BUNDLE`'s directory, the
    /// embedded client, or the tree's `dist/web` in a build that embedded none.
    pub fn bundled() -> Option<WebClient> {
        let configured = std::env::var("SELKIES_WEB_BUNDLE").ok();
        choose("", configured.as_deref(), &EMBEDDED, &tree_bundle()).map(|(client, _)| client)
    }
}

/// `dist/web` in the tree this crate was built from.
fn tree_bundle() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/web")
}

fn holds_index(dir: &Path) -> bool {
    dir.is_dir() && dir.join("index.html").is_file()
}

/// The client by precedence, and what to call where it came from: `web_root`, then the
/// `bundle` directory (`SELKIES_WEB_BUNDLE`), each when it holds an `index.html`, then
/// `embedded` unless empty, then `tree`. A configured directory without an `index.html` is
/// passed over with a warning.
fn choose(
    web_root: &str,
    bundle: Option<&str>,
    embedded: &'static EmbeddedClient,
    tree: &Path,
) -> Option<(WebClient, String)> {
    if !web_root.is_empty() {
        let path = crate::core::expand_user(web_root);
        if holds_index(&path) {
            let origin = format!("web_root {}", path.display());
            return Some((WebClient::Directory(path), origin));
        }
        log::warn!(target: "server", "web_root directory {} not found or missing index.html", path.display());
    }
    if let Some(dir) = bundle.map(str::trim).filter(|dir| !dir.is_empty()) {
        let path = PathBuf::from(dir);
        if holds_index(&path) {
            let origin = format!("SELKIES_WEB_BUNDLE {}", path.display());
            return Some((WebClient::Directory(path), origin));
        }
        log::warn!(target: "server", "SELKIES_WEB_BUNDLE directory {} not found or missing index.html", path.display());
    }
    if !embedded.is_empty() {
        let count = embedded.len();
        let origin = format!(
            "this executable, which embeds {count} file{}",
            if count == 1 { "" } else { "s" }
        );
        return Some((WebClient::Embedded(embedded), origin));
    }
    holds_index(tree).then(|| {
        let origin = format!("{}, this build embedding none", tree.display());
        (WebClient::Directory(tree.to_path_buf()), origin)
    })
}

/// Settle the client the server serves, saying which at start; none, with a warning, where
/// no source has one.
pub fn resolve(web_root: &str) -> Option<WebClient> {
    let configured = std::env::var("SELKIES_WEB_BUNDLE").ok();
    match choose(web_root, configured.as_deref(), &EMBEDDED, &tree_bundle()) {
        Some((client, origin)) => {
            log::info!(target: "server", "Serving the web client from {origin}");
            Some(client)
        }
        None => {
            log::warn!(target: "server", "No web client: this build embedded none and no web_root holds one; serving the API alone");
            None
        }
    }
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
/// prefix, or with no web client, is 404. Under the prefix, a method other than `GET` and
/// `HEAD` is 405, as the client's catch-all route answered it in 2.2.1.
pub async fn fallback(
    State(state): State<AppState>,
    OriginalUri(original): OriginalUri,
    request: Request,
) -> Response {
    let prefix = state.config.subfolder.as_str();
    let Some(client) = state.web_client.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = original.path();
    let Some(rel) = path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (parts, _body) = request.into_parts();
    if parts.method != Method::GET && parts.method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
        )
            .into_response();
    }
    if rel.is_empty() {
        return index(&state, client, &parts).await;
    }
    let decoded = percent_encoding::percent_decode_str(rel)
        .decode_utf8_lossy()
        .into_owned();
    let mut response = match client {
        WebClient::Embedded(table) => embedded::serve(table, &parts, &decoded).await,
        WebClient::Directory(root) => {
            let (check_root, check_name) = (root.clone(), decoded.clone());
            let inside =
                tokio::task::spawn_blocking(move || resolves_inside(&check_root, &check_name));
            if !inside.await.unwrap_or(false) {
                return StatusCode::NOT_FOUND.into_response();
            }
            serve(
                ServeDir::new(root).append_index_html_on_directories(false),
                &parts,
                rel,
            )
            .await
        }
    };
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
async fn index(state: &AppState, client: &WebClient, parts: &Parts) -> Response {
    let conn = parts.extensions.get::<ConnInfo>();
    let path = format!("{}/", state.config.subfolder);
    if let Some(conn) = conn
        && let Some(target) = ipv6_loopback_redirect(conn, &parts.headers, &path, parts.uri.query())
    {
        return (StatusCode::FOUND, [(header::LOCATION, target)]).into_response();
    }
    let mut response = match client {
        WebClient::Embedded(table) => embedded::serve(table, parts, "index.html").await,
        WebClient::Directory(root) => {
            serve(ServeFile::new(root.join("index.html")), parts, "index.html").await
        }
    };
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

    fn origin_of(chosen: Option<(WebClient, String)>) -> String {
        chosen.map(|(_, origin)| origin).unwrap_or_default()
    }

    #[test]
    fn web_root_wins_over_the_bundle_directory_which_wins_over_the_embedded_client() {
        let root = crate::http::testing::tempfile_dir("selkies-web-choose-");
        let (web_root, bundle, tree, empty) = (
            root.join("web_root"),
            root.join("bundle"),
            root.join("tree"),
            root.join("empty"),
        );
        for dir in [&web_root, &bundle, &tree] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("index.html"), "<html>").unwrap();
        }
        std::fs::create_dir_all(&empty).unwrap();
        let table = embedded::leak(&[("index.html", b"<html>")], 0);
        let none = embedded::leak(&[], 0);
        let (web_root_s, bundle_s, empty_s) = (
            web_root.to_string_lossy(),
            bundle.to_string_lossy(),
            empty.to_string_lossy(),
        );

        let chosen = choose(&web_root_s, Some(&bundle_s), table, &tree);
        assert!(matches!(&chosen, Some((WebClient::Directory(d), _)) if *d == web_root));
        assert!(origin_of(chosen).starts_with("web_root "));

        let chosen = choose(&empty_s, Some(&bundle_s), table, &tree);
        assert!(
            matches!(&chosen, Some((WebClient::Directory(d), _)) if *d == bundle),
            "a web_root without an index.html is passed over"
        );
        assert!(origin_of(chosen).starts_with("SELKIES_WEB_BUNDLE "));

        let chosen = choose("", Some(&empty_s), table, &tree);
        assert!(
            matches!(&chosen, Some((WebClient::Embedded(t), _)) if std::ptr::eq(*t, table)),
            "so is a bundle directory without one"
        );
        assert_eq!(
            origin_of(chosen),
            "this executable, which embeds 1 file",
            "the embedded client wins over the tree"
        );

        let chosen = choose("", None, none, &tree);
        assert!(
            matches!(&chosen, Some((WebClient::Directory(d), _)) if *d == tree),
            "a build that embedded none serves the tree's"
        );
        assert!(choose("", None, none, &empty).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
