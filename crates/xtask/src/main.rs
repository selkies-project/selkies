/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The repository's own tooling, run as `cargo xtask <task>` from anywhere in the tree.
//!
//! Each task keeps a committed file in step with the source it is derived from, and fails
//! under `--check` (or, for the license gate, by default) when the two differ:
//!
//! - `settings-doc` renders `docs/settings.md` from the server's setting table
//!   ([`settings_doc`]);
//! - `file-index` writes the nginx fancyindex fragments from the file index the server
//!   serves ([`file_index`]);
//! - `logo-assets` regenerates every image from the logo artwork ([`logo_assets`]);
//! - `licenses` audits the crate graphs and the web packages' lockfiles and holds the
//!   generated tables of `docs/licensing.md` to them, and with `--crate` audits pixelflux's
//!   own graph, as its wheels build it ([`licenses`]);
//! - `wheel` repackages a release bundle as the selkies wheel of its platform ([`wheel`]).
//!
//! The sources are read as text and the crate graph through `cargo metadata`, so the tool
//! depends on no crate of the workspace and builds without the native libraries they link.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod file_index;
mod licenses;
mod logo_assets;
mod settings_doc;
mod wheel;

/// The repository root, two directories above this crate.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives under crates/")
        .to_path_buf()
}

/// A scratch directory of the task's own, removed when it goes out of scope.
struct Scratch(PathBuf);

impl Scratch {
    fn new(prefix: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
        std::fs::create_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let task = args.next().unwrap_or_default();
    let args: Vec<String> = args.collect();
    let outcome = match task.as_str() {
        "settings-doc" => check_flag(&args).and_then(settings_doc::run),
        "file-index" => check_flag(&args).and_then(file_index::run),
        "logo-assets" => check_flag(&args).and_then(logo_assets::run),
        "licenses" => licenses::run(&args),
        "wheel" => wheel::run(&args),
        _ => Err(
            "usage: cargo xtask settings-doc [--check] | file-index [--check] | logo-assets [--check] | licenses \
                  [--crate DIR] [--write | --markdown] [--set gpl|non-gpl] [--metadata FILE] \
                  [--target TRIPLE] [--page FILE] [--quiet] | wheel <bundle.tar.gz> <out-dir>"
                .into(),
        ),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}

/// Whether a generator was asked to check its output rather than write it.
fn check_flag(args: &[String]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--check" => Ok(true),
        _ => Err(format!("unexpected arguments: {}", args.join(" "))),
    }
}
