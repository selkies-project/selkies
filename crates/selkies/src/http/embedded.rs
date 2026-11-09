/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The web client compiled into the executable, and how it is served.
//!
//! `build.rs` packs the bundle into [`EMBEDDED`]: a table of [`EmbeddedFile`]s sorted by name
//! over one blob of their stored bytes, each file gzip-compressed where that pays
//! (`web_bundle.rs`). A request names a key of that table, never a path, so nothing
//! outside it is reachable, and every body is a slice of the blob (`Bytes::from_static`). A
//! stored gzip goes out as it is, with `Content-Encoding: gzip`, to a client whose
//! `Accept-Encoding` takes gzip, and is inflated on a blocking thread for one that does not;
//! the two carry `Vary: Accept-Encoding` and an entity tag each. Every response carries the
//! table's `Last-Modified`: with no `Cache-Control`, a browser caches the content-hashed assets,
//! the icons, and the manifest by that date's heuristic, as it does a directory's files. A
//! request's `If-None-Match`, or its `If-Modified-Since` where it has none, answers 304. A range
//! request gets the whole file: no client of these files asks for part of one.

use std::io::Read;

use axum::body::Bytes;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// A web client as a table over one blob.
pub struct EmbeddedClient {
    /// The files, sorted by name.
    pub files: &'static [EmbeddedFile],
    /// Every file's stored bytes, end to end.
    pub blob: &'static [u8],
    /// The `Last-Modified` of every file, in seconds since the epoch.
    pub last_modified: u64,
}

/// One file of an [`EmbeddedClient`].
#[derive(Debug)]
pub struct EmbeddedFile {
    /// Its path under the client, `/`-separated.
    pub name: &'static str,
    pub content_type: &'static str,
    /// Where its stored bytes start in the blob.
    pub start: usize,
    /// Where they end.
    pub end: usize,
    /// The quoted entity tag of the file as it is.
    pub etag: &'static str,
    /// The quoted entity tag of its gzip encoding, when that is what the blob stores.
    pub gzip_etag: Option<&'static str>,
}

/// The web client this executable was built with; empty when it was built without one.
pub static EMBEDDED: EmbeddedClient = include!(concat!(env!("OUT_DIR"), "/web_client.rs"));

impl std::fmt::Debug for EmbeddedClient {
    /// The table's size, not its megabyte of bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedClient")
            .field("files", &self.files.len())
            .field("blob", &self.blob.len())
            .field("last_modified", &self.last_modified)
            .finish()
    }
}

impl EmbeddedClient {
    /// The file a request names, if the table holds one by that name.
    pub fn get(&self, name: &str) -> Option<&EmbeddedFile> {
        self.files
            .binary_search_by(|file| file.name.cmp(name))
            .ok()
            .map(|index| &self.files[index])
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// What the blob stores for `file`, without a copy.
    fn stored(&'static self, file: &EmbeddedFile) -> Bytes {
        Bytes::from_static(&self.blob[file.start..file.end])
    }

    /// The content of the file `name`, inflated where the table stores it compressed.
    pub fn read(&'static self, name: &str) -> Option<Bytes> {
        let file = self.get(name)?;
        let stored = self.stored(file);
        match file.gzip_etag {
            Some(_) => gunzip(&stored).ok(),
            None => Some(stored),
        }
    }
}

/// What `selkies --version` says of the embedded client: `web client: embedded (N files)`,
/// or `web client: none embedded` for a build that found no bundle.
pub fn summary() -> String {
    if EMBEDDED.is_empty() {
        "web client: none embedded".to_string()
    } else {
        format!("web client: embedded ({} files)", EMBEDDED.len())
    }
}

/// The response to `parts` for the file `name` of `client`: 404 where the table holds none
/// by that name, 304 where the request's validator matches, else the file. A `HEAD` request
/// gets the same headers, the router dropping the body.
pub async fn serve(client: &'static EmbeddedClient, parts: &Parts, name: &str) -> Response {
    let Some(file) = client.get(name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let send_gzip = file.gzip_etag.is_some() && accepts_gzip(&parts.headers);
    let etag = match file.gzip_etag {
        Some(tag) if send_gzip => tag,
        _ => file.etag,
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::ETAG, HeaderValue::from_static(etag));
    if let Ok(date) = HeaderValue::from_str(&http_date(client.last_modified)) {
        headers.insert(header::LAST_MODIFIED, date);
    }
    if file.gzip_etag.is_some() {
        headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    if not_modified(&parts.headers, etag, client.last_modified) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(file.content_type),
    );
    let stored = client.stored(file);
    let body = if send_gzip {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        stored
    } else if file.gzip_etag.is_some() {
        match tokio::task::spawn_blocking(move || gunzip(&stored)).await {
            Ok(Ok(inflated)) => inflated,
            _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    } else {
        stored
    };
    (StatusCode::OK, headers, body).into_response()
}

/// Whether a request's `Accept-Encoding` takes gzip: `gzip` or `x-gzip` with a nonzero
/// weight, else `*` with one. A request naming no encoding gets the file as it is, which
/// every client reads.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    let mut gzip = None;
    let mut any = None;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for item in value.split(',') {
            let mut params = item.split(';');
            let coding = params.next().unwrap_or("").trim().to_ascii_lowercase();
            let weight = params
                .filter_map(|p| p.trim().strip_prefix("q=").or(p.trim().strip_prefix("Q=")))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            match coding.as_str() {
                "gzip" | "x-gzip" => gzip = Some(weight),
                "*" => any = Some(weight),
                _ => {}
            }
        }
    }
    gzip.or(any).is_some_and(|weight| weight > 0.0)
}

/// Whether the request's validators match a representation tagged `etag` and last modified
/// at `last_modified`: `If-None-Match`, by weak comparison, where the request carries one,
/// else `If-Modified-Since`.
fn not_modified(headers: &HeaderMap, etag: &str, last_modified: u64) -> bool {
    if headers.contains_key(header::IF_NONE_MATCH) {
        return headers
            .get_all(header::IF_NONE_MATCH)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag);
    }
    headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
        .is_some_and(|since| since.timestamp() >= last_modified as i64)
}

/// `secs` since the epoch as an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_default()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// `stored`, a gzip stream, inflated.
fn gunzip(stored: &[u8]) -> std::io::Result<Bytes> {
    let mut inflated = Vec::new();
    flate2::read::GzDecoder::new(stored).read_to_end(&mut inflated)?;
    Ok(Bytes::from(inflated))
}

/// A client of `files`, `(name, content)`, packed as `build.rs` packs a bundle and leaked to
/// live as long as the table it stands in for.
#[cfg(test)]
pub(crate) fn leak(files: &[(&str, &[u8])], last_modified: u64) -> &'static EmbeddedClient {
    let mut packed: Vec<super::web_bundle::Packed> = files
        .iter()
        .map(|(name, content)| super::web_bundle::pack(name, content))
        .collect();
    packed.sort_by(|a, b| a.name.cmp(&b.name));
    let mut blob = Vec::new();
    let mut table = Vec::new();
    for file in &packed {
        let start = blob.len();
        blob.extend_from_slice(&file.bytes);
        table.push(EmbeddedFile {
            name: Box::leak(file.name.clone().into_boxed_str()),
            content_type: file.content_type,
            start,
            end: blob.len(),
            etag: Box::leak(file.etag.clone().into_boxed_str()),
            gzip_etag: file
                .gzip_etag
                .clone()
                .map(|tag| &*Box::leak(tag.into_boxed_str())),
        });
    }
    Box::leak(Box::new(EmbeddedClient {
        files: Box::leak(table.into_boxed_slice()),
        blob: Box::leak(blob.into_boxed_slice()),
        last_modified,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::web_bundle;
    use super::*;

    fn with(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    /// Bytes no deflate stream shortens: one xxh3 hash after another.
    fn incompressible(len: usize) -> Vec<u8> {
        (0..len.div_ceil(16) as u64)
            .flat_map(|i| xxhash_rust::xxh3::xxh3_128(&i.to_le_bytes()).to_le_bytes())
            .take(len)
            .collect()
    }

    #[test]
    fn a_file_gzip_shortens_by_a_tenth_is_stored_compressed_and_any_other_as_it_is() {
        let text = "{\"name\": \"Xbox Wireless Controller\", \"axes\": [0, 1, 2]}\n".repeat(40);
        let packed = web_bundle::pack("jsdb/xbox.json", text.as_bytes());
        assert_eq!(packed.content_type, "application/json");
        assert!(packed.bytes.len() * 10 <= text.len() * 9);
        assert_eq!(
            packed.gzip_etag.as_deref(),
            Some(web_bundle::etag(&packed.bytes).as_str()),
            "the encoding's tag is its own bytes' hash"
        );
        assert_eq!(gunzip(&packed.bytes).unwrap(), text.as_bytes());
        assert_eq!(packed.etag, web_bundle::etag(text.as_bytes()));

        let font = incompressible(4096);
        let packed = web_bundle::pack("assets/inter-abc.woff2", &font);
        assert_eq!(packed.content_type, "font/woff2");
        assert_eq!((packed.gzip_etag, packed.bytes), (None, font));
    }

    #[test]
    fn a_file_has_the_content_type_a_directory_serves_it_with() {
        for (name, expected) in [
            ("index.html", "text/html"),
            ("assets/index-FfFZgY7-.js", "text/javascript"),
            ("codecs/libopus-wasm/index.mjs", "application/javascript"),
            ("assets/index.css", "text/css"),
            ("selkies.svg", "image/svg+xml"),
            ("codecs/x.wasm", "application/wasm"),
            ("codecs/libopus-wasm/LICENSE", "application/octet-stream"),
        ] {
            assert_eq!(
                web_bundle::pack(name, b"x").content_type,
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn an_etag_is_a_quoted_hash_that_follows_the_content() {
        let tag = web_bundle::etag(b"one");
        assert!(tag.len() == 34 && tag.starts_with('"') && tag.ends_with('"'));
        assert!(tag[1..33].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(tag, web_bundle::etag(b"one"));
        assert_ne!(tag, web_bundle::etag(b"two"));
    }

    #[test]
    fn the_bundle_is_listed_by_slash_separated_names_in_order_through_links() {
        let root = crate::http::testing::tempfile_dir("selkies-web-bundle-");
        std::fs::create_dir_all(root.join("assets/fonts")).unwrap();
        std::fs::write(root.join("index.html"), "i").unwrap();
        std::fs::write(root.join("assets/fonts/a.woff2"), "f").unwrap();
        std::fs::write(root.join("assets/app.js"), "a").unwrap();
        std::os::unix::fs::symlink(root.join("assets/app.js"), root.join("alias.js")).unwrap();
        let names: Vec<String> = web_bundle::files(&root)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "alias.js",
                "assets/app.js",
                "assets/fonts/a.woff2",
                "index.html"
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_rendered_table_slices_the_blob_at_each_file() {
        let packed = [
            web_bundle::pack("a.js", "let a = 1;\n".repeat(50).as_bytes()),
            web_bundle::pack("b/c.png", &incompressible(64)),
        ];
        let path = std::path::Path::new("/out/web_client.bin");
        let (source, blob) = web_bundle::render(&packed, 784_111_777, path);
        assert_eq!(blob.len(), packed[0].bytes.len() + packed[1].bytes.len());
        assert_eq!(&blob[..packed[0].bytes.len()], packed[0].bytes.as_slice());
        assert!(source.contains(&format!(
            "name: \"b/c.png\", content_type: \"image/png\", start: {}, end: {}, ",
            packed[0].bytes.len(),
            blob.len()
        )));
        assert!(source.contains("gzip_etag: None"));
        assert!(source.contains("blob: include_bytes!(\"/out/web_client.bin\")"));
        assert!(source.contains("last_modified: 784111777"));
        let (empty, blob) = web_bundle::render(&[], 0, path);
        assert!(blob.is_empty() && empty.contains("blob: &[]"), "{empty}");
    }

    /// What `build.rs` embedded is the bundle it names, file for file.
    #[test]
    fn the_built_table_holds_the_bundle_it_was_built_from() {
        let from = env!("SELKIES_WEB_EMBEDDED_FROM");
        if from.is_empty() {
            assert!(EMBEDDED.is_empty(), "nothing embedded without a bundle");
            return;
        }
        let listed = web_bundle::files(std::path::Path::new(from)).unwrap();
        let names: Vec<&str> = EMBEDDED.files.iter().map(|file| file.name).collect();
        let expected: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, expected);
        assert!(EMBEDDED.get("index.html").is_some());
        for (name, path) in &listed {
            let content = std::fs::read(path).unwrap();
            let file = EMBEDDED.get(name).unwrap();
            assert_eq!(EMBEDDED.read(name).unwrap(), content, "{name}");
            assert_eq!(file.etag, web_bundle::etag(&content), "{name}");
        }
    }

    #[test]
    fn gzip_is_taken_by_its_weight_and_never_assumed() {
        for (pairs, expected) in [
            (vec![], false),
            (vec![("accept-encoding", "gzip, deflate, br, zstd")], true),
            (vec![("accept-encoding", "br;q=1.0, GZip;q=0.5")], true),
            (vec![("accept-encoding", "x-gzip")], true),
            (vec![("accept-encoding", "gzip;q=0")], false),
            (vec![("accept-encoding", "gzip; q=0.0, *")], false),
            (vec![("accept-encoding", "*")], true),
            (vec![("accept-encoding", "*;q=0")], false),
            (vec![("accept-encoding", "identity")], false),
            (
                vec![("accept-encoding", "br"), ("accept-encoding", "gzip")],
                true,
            ),
        ] {
            assert_eq!(accepts_gzip(&with(&pairs)), expected, "{pairs:?}");
        }
    }

    #[test]
    fn a_validator_matches_the_tag_or_else_the_date() {
        let tag = "\"0123\"";
        let date = 784_111_777;
        for (pairs, expected) in [
            (vec![], false),
            (vec![("if-none-match", "\"0123\"")], true),
            (vec![("if-none-match", "W/\"0123\"")], true),
            (vec![("if-none-match", "\"9\", \"0123\"")], true),
            (vec![("if-none-match", "*")], true),
            (vec![("if-none-match", "\"9\"")], false),
            (
                vec![
                    ("if-none-match", "\"9\""),
                    ("if-modified-since", "Sun, 06 Nov 1994 08:49:37 GMT"),
                ],
                false,
            ),
            (
                vec![("if-modified-since", "Sun, 06 Nov 1994 08:49:37 GMT")],
                true,
            ),
            (
                vec![("if-modified-since", "Sun, 06 Nov 1994 08:49:36 GMT")],
                false,
            ),
            (vec![("if-modified-since", "yesterday")], false),
        ] {
            assert_eq!(
                not_modified(&with(&pairs), tag, date),
                expected,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn last_modified_is_an_http_date() {
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    }
}
