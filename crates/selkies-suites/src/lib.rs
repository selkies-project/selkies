/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The harness the integration suites under `tests/` share.
//!
//! A suite drives a built `selkies` as a black box, over its command line, its HTTP routes, its
//! WebSocket and its log, so the crate depends on no crate of the workspace: it builds in seconds
//! beside a server a wheel installed, and what it tests is the binary that ships.
//!
//! - [`server`] starts that binary from an environment built from nothing, on a free port, under a
//!   home and a runtime directory of the run's own, and ends its process group when the test
//!   does, a panic included. Servers of one test binary take turns, since they share the test
//!   display.
//! - [`client`] is a WebSocket page: it reads its socket for as long as it lives, as a browser
//!   does, and keeps what arrived for the test to wait on.
//! - [`http`] is one request against a server.
//! - [`checks`] records a suite's checks, each named for the behavior it holds, and fails the test
//!   once with every one that did not hold and the server's log.
//!
//! Every test is `#[ignore]`d with what it needs, as the tests that need a GPU or an X server are:
//! `cargo test --workspace` builds them and runs none, and a run that names a suite
//! (`cargo test -p selkies-suites --test protocol -- --include-ignored`) fails, never skips, where
//! the server or the display is missing. A wait is a condition with a bound, never a sleep, and a
//! check that something did not happen watches for it over a stated window.

pub mod checks;
pub mod client;
pub mod http;
pub mod server;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use checks::Checks;
pub use client::Client;
pub use server::Server;

/// The repository the suites run from: `SELKIES_REPO`, else the tree this crate was built in.
pub fn repo() -> PathBuf {
    match std::env::var_os("SELKIES_REPO") {
        Some(repo) => PathBuf::from(repo),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
    }
}

/// The selkies server under test: `SELKIES_TEST_SERVER`, else the release build of this tree,
/// else the `selkies` on `PATH`.
pub fn server_binary() -> PathBuf {
    if let Some(configured) = std::env::var_os("SELKIES_TEST_SERVER").filter(|v| !v.is_empty()) {
        return PathBuf::from(configured);
    }
    let built = repo().join("target/release/selkies");
    if built.is_file() {
        return built;
    }
    on_path("selkies").unwrap_or_else(|| PathBuf::from("selkies"))
}

/// The first executable named `name` on `PATH`.
pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Where the suites keep server logs and scratch: `E2E_WORKDIR`, else `selkies-tests` under the
/// temporary directory. The run's driver copies the `.log` files a suite leaves here into that
/// suite's folder, which CI uploads.
///
/// # Panics
///
/// When `E2E_WORKDIR` is not an absolute path free of `..`, or the directory cannot be made.
pub fn workdir() -> &'static Path {
    static WORKDIR: OnceLock<PathBuf> = OnceLock::new();
    WORKDIR.get_or_init(|| {
        let dir = std::env::var_os("E2E_WORKDIR")
            .filter(|v| !v.is_empty())
            .map_or_else(|| std::env::temp_dir().join("selkies-tests"), PathBuf::from);
        assert!(
            dir.is_absolute()
                && !dir
                    .components()
                    .any(|c| c == std::path::Component::ParentDir),
            "E2E_WORKDIR={} is not an absolute path without '..'",
            dir.display()
        );
        std::fs::create_dir_all(&dir).expect("the work directory");
        dir
    })
}

/// A directory of the run's own under [`workdir`], private to the user, made on first use.
///
/// # Panics
///
/// When the directory cannot be made.
pub fn private_dir(name: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let dir = workdir().join(name);
    if !dir.is_dir() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("a private directory under the work directory");
    }
    dir
}

/// Write the executable script `body` at `path`, making its directory.
///
/// # Panics
///
/// When the file cannot be written.
pub fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("the script's directory");
    }
    std::fs::write(path, body).expect("the script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("the script's mode");
}

/// `dir` ahead of this process's `PATH`, as a server that must find a stand-in there takes it.
pub fn path_with(dir: &Path) -> String {
    let rest = std::env::var("PATH").unwrap_or_default();
    format!("{}:{rest}", dir.display())
}

/// The suite this test binary holds: its file name without the hash cargo appends.
pub fn suite() -> String {
    let exe = std::env::current_exe().expect("the test binary's path");
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            name.to_owned()
        }
        _ => stem,
    }
}

/// The last `lines` lines of `text`.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
