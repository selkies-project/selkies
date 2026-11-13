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
//! - `licenses` audits the crate graphs and the web packages' lockfiles and holds the
//!   generated tables of `docs/licensing.md` to them ([`licenses`]).
//!
//! The sources are read as text and the crate graph through `cargo metadata`, so the tool
//! depends on no crate of the workspace and builds without the native libraries they link.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod file_index;
mod licenses;
mod settings_doc;

/// The repository root, two directories above this crate.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives under crates/")
        .to_path_buf()
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let task = args.next().unwrap_or_default();
    let args: Vec<String> = args.collect();
    let outcome = match task.as_str() {
        "settings-doc" => check_flag(&args).and_then(settings_doc::run),
        "file-index" => check_flag(&args).and_then(file_index::run),
        "licenses" => licenses::run(&args),
        _ => Err(
            "usage: cargo xtask settings-doc [--check] | file-index [--check] | licenses \
                  [--write | --markdown] [--set gpl|non-gpl] [--metadata FILE] [--page FILE] \
                  [--quiet]"
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
