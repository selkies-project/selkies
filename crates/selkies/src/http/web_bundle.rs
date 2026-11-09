/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Packing the web client's bundle into the table the executable embeds.
//!
//! The build script reads every file of the bundle, keeps each one gzip-compressed where that
//! saves at least a tenth of it and as it is otherwise (the WOFF2 fonts and PNG icons are
//! compressed already), and writes the stored bytes end to end into one blob beside a table of
//! [`Packed`] entries sorted by name, which the server slices its responses from.
//! A file's content type is the one tower-http's `ServeDir` would send for it, its entity tags
//! are xxh3-128 hashes of the bytes each representation carries (the hash pixelflux already
//! takes of frame content: a strong validator needs a change detector, not a cryptographic
//! digest), and the table's one `Last-Modified` time is `SOURCE_DATE_EPOCH` or the bundle's
//! newest file. `build.rs` compiles this module, and so do the server's tests.

use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

/// A stored gzip encoding is kept when it is at most this fraction of the file:
/// `numerator / denominator`.
pub const GZIP_KEEP: (usize, usize) = (9, 10);

/// One file of the bundle as the table stores it.
#[derive(Debug)]
pub struct Packed {
    /// Its path under the bundle, `/`-separated: the name a request asks for.
    pub name: String,
    pub content_type: &'static str,
    /// The quoted entity tag of the file as it is.
    pub etag: String,
    /// The quoted entity tag of its gzip encoding, when that is what `bytes` holds.
    pub gzip_etag: Option<String>,
    /// What the table stores: the gzip encoding when `gzip_etag` is set, else the file.
    pub bytes: Vec<u8>,
}

/// Every regular file under `root`, symbolic links followed, as `(name, path)` sorted by name.
///
/// Fails on a name that is not UTF-8, which no request could ask for.
pub fn files(root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let meta = std::fs::metadata(&path)?;
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() {
                let relative = path.strip_prefix(root).expect("a path under the root");
                let parts: Option<Vec<&str>> = relative
                    .components()
                    .map(|c| c.as_os_str().to_str())
                    .collect();
                let Some(parts) = parts else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{} is not named in UTF-8", path.display()),
                    ));
                };
                found.push((parts.join("/"), path));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The quoted xxh3-128 entity tag of `bytes`.
pub fn etag(bytes: &[u8]) -> String {
    format!("\"{:032x}\"", xxhash_rust::xxh3::xxh3_128(bytes))
}

/// Pack the file `name` holding `raw`: its content type, its tags, and the bytes to store.
pub fn pack(name: &str, raw: &[u8]) -> Packed {
    let content_type = mime_guess::from_path(name)
        .first_raw()
        .unwrap_or("application/octet-stream");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    let gzip = encoder
        .write_all(raw)
        .and_then(|()| encoder.finish())
        .expect("gzip into memory");
    let (bytes, gzip_etag) = if gzip.len() * GZIP_KEEP.1 <= raw.len() * GZIP_KEEP.0 {
        let tag = etag(&gzip);
        (gzip, Some(tag))
    } else {
        (raw.to_vec(), None)
    };
    Packed {
        name: name.to_string(),
        content_type,
        etag: etag(raw),
        gzip_etag,
        bytes,
    }
}

/// The Rust source of the table over `packed`, whose stored bytes lie end to end in the blob
/// at `blob_path`, and that blob. The source is an `EmbeddedClient` value.
pub fn render(packed: &[Packed], last_modified: u64, blob_path: &Path) -> (String, Vec<u8>) {
    let mut blob = Vec::new();
    let mut source = String::from("EmbeddedClient {\n    files: &[\n");
    for file in packed {
        let start = blob.len();
        blob.extend_from_slice(&file.bytes);
        source.push_str(&format!(
            "        EmbeddedFile {{ name: {:?}, content_type: {:?}, start: {start}, end: {}, \
             etag: {:?}, gzip_etag: {:?} }},\n",
            file.name,
            file.content_type,
            blob.len(),
            file.etag,
            file.gzip_etag,
        ));
    }
    let blob_source = if blob.is_empty() {
        "&[]".to_string()
    } else {
        format!("include_bytes!({:?})", blob_path.to_string_lossy())
    };
    source.push_str(&format!(
        "    ],\n    blob: {blob_source},\n    last_modified: {last_modified},\n}}\n"
    ));
    (source, blob)
}
