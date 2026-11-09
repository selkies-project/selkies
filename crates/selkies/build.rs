/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Embeds the web client: packs the bundle `scripts/ci/build-web.sh` writes into
//! `$OUT_DIR/web_client.rs` and `web_client.bin` (`src/http/web_bundle.rs`), which
//! `http::embedded` includes.
//!
//! The bundle is the directory `SELKIES_WEB_DIST` names, which must then hold an `index.html`
//! or the build fails, else `dist/web` in the tree. A build that finds neither embeds an empty
//! table and warns: the server then serves the tree's `dist/web` from disk, so a checkout
//! builds and tests before its client is built. Cargo reruns this when the bundle changes;
//! where the tree holds no `dist` at all there is nothing to watch, so a client built after
//! the server is served from disk until this script next runs.

#[path = "src/http/web_bundle.rs"]
mod web_bundle;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/http/web_bundle.rs");
    println!("cargo:rerun-if-env-changed=SELKIES_WEB_DIST");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let tree = manifest.join("../../dist/web");
    let configured = std::env::var_os("SELKIES_WEB_DIST")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let bundle = match configured {
        Some(dir) if dir.join("index.html").is_file() => Some(dir),
        Some(dir) => panic!(
            "SELKIES_WEB_DIST={} holds no index.html; build the client with scripts/ci/build-web.sh",
            dir.display()
        ),
        None => tree.join("index.html").is_file().then_some(tree.clone()),
    };
    let listed = match &bundle {
        Some(dir) => {
            println!("cargo:rerun-if-changed={}", dir.display());
            web_bundle::files(dir).expect("read the web client's bundle")
        }
        None => {
            if let Some(parent) = tree.parent().filter(|p| p.is_dir()) {
                println!("cargo:rerun-if-changed={}", parent.display());
            }
            println!(
                "cargo:warning=no web client in {}: none is embedded; build it with \
                 scripts/ci/build-web.sh",
                tree.display()
            );
            Vec::new()
        }
    };
    let packed: Vec<web_bundle::Packed> = listed
        .iter()
        .map(|(name, path)| {
            let raw =
                std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            web_bundle::pack(name, &raw)
        })
        .collect();
    let blob_path = out.join("web_client.bin");
    let (source, blob) = web_bundle::render(&packed, last_modified(&listed), &blob_path);
    std::fs::write(&blob_path, blob).expect("write the web client's blob");
    std::fs::write(out.join("web_client.rs"), source).expect("write the web client's table");
    let embedded_from = bundle
        .map(|dir| dir.canonicalize().unwrap_or(dir).display().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=SELKIES_WEB_EMBEDDED_FROM={embedded_from}");
}

/// The table's `Last-Modified`, in seconds since the epoch: `SOURCE_DATE_EPOCH` where a
/// reproducible build sets it, else the newest of the `listed` files.
fn last_modified(listed: &[(String, PathBuf)]) -> u64 {
    if let Some(epoch) = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
    {
        return epoch;
    }
    listed
        .iter()
        .filter_map(|(_, path)| modified(path))
        .max()
        .unwrap_or(0)
}

/// When `path` was last written, in seconds since the epoch.
fn modified(path: &Path) -> Option<u64> {
    let time = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(time.duration_since(UNIX_EPOCH).ok()?.as_secs())
}
