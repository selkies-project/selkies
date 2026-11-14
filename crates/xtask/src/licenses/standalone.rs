/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! pixelflux's own graph, as its wheels build it, and the listing its `LICENSES.md` is written
//! from.
//!
//! Inside a larger workspace (Selkies'), cargo unifies the features every member asks of a
//! dependency, so the non-GPL graph would carry the `x264` and `x265` features the server
//! enables. A crate that belongs to another workspace is therefore resolved from a copy of its
//! manifest and its own `Cargo.lock` in a scratch directory, beside links to the rest of the
//! crate; the `git archive` copy `scripts/ci/crate-licenses.sh` hands the gate is a workspace of
//! its own already.
//!
//! Each configuration prints one block: a summary line, a row a crate in aligned columns or,
//! under `--markdown`, a table naming each native library and how it is used, then the
//! violations or a `PASS` line.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use super::spdx::Rank;
use super::{Crate, Options, Scope, audit, cargo_metadata, table};
use crate::Scratch;

/// What a configuration asks of cargo, as `setup.py` selects it: the default features (`gpl`),
/// or OpenH264 and kvazaar in their place for `PIXELFLUX_ENABLE_GPL=0`.
fn features(set: &str) -> &'static [&'static str] {
    if set == "gpl" {
        &[]
    } else {
        &["--no-default-features", "--features", "openh264"]
    }
}

/// Resolve one configuration of the crate at `dir` as it builds on its own.
fn resolve(dir: &Path, set: &str, targets: &[&str]) -> Result<Value, String> {
    let manifest = fs::canonicalize(dir.join("Cargo.toml"))
        .map_err(|e| format!("--crate {}: {e}", dir.display()))?;
    let args: Vec<&str> = ["--locked"].iter().chain(features(set)).copied().collect();
    let located = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .map_err(|e| format!("cargo: {e}"))?;
    if !located.status.success() {
        return Err(format!(
            "cargo locate-project failed:\n{}",
            String::from_utf8_lossy(&located.stderr)
        ));
    }
    let workspace = String::from_utf8_lossy(&located.stdout);
    if fs::canonicalize(workspace.trim()).ok().as_ref() == Some(&manifest) {
        return cargo_metadata(&manifest, &args, targets);
    }
    let source = manifest.parent().unwrap_or(dir);
    let scratch = Scratch::new("pixelflux-licenses")?;
    let io = |e: std::io::Error| format!("{}: {e}", scratch.0.display());
    for name in ["Cargo.toml", "Cargo.lock"] {
        fs::copy(source.join(name), scratch.0.join(name)).map_err(io)?;
    }
    for entry in fs::read_dir(source).map_err(io)? {
        let name = entry.map_err(io)?.file_name();
        if !["Cargo.toml", "Cargo.lock", "target"]
            .iter()
            .any(|kept| name == *kept)
        {
            std::os::unix::fs::symlink(source.join(&name), scratch.0.join(&name)).map_err(io)?;
        }
    }
    let meta = cargo_metadata(&scratch.0.join("Cargo.toml"), &args, targets)?;
    let root = meta["workspace_root"].as_str().unwrap_or_default();
    if fs::canonicalize(root).ok() != fs::canonicalize(&scratch.0).ok() {
        return Err(format!(
            "cargo resolved {root} in place of the crate's own workspace"
        ));
    }
    Ok(meta)
}

/// The category a row reads, MPL-2.0 and the other file-level licenses as permissive.
fn category(rank: Rank) -> &'static str {
    match rank {
        Rank::FileLevel => "permissive (file-level copyleft)",
        other => other.label(),
    }
}

/// One configuration's summary line and its rows, aligned or as a Markdown table.
fn render(crates: &[Crate], set: &str, markdown: bool) -> String {
    let worst = crates
        .iter()
        .flat_map(|c| [c.rank, c.native_rank])
        .max()
        .unwrap_or(Rank::Permissive);
    let summary = format!(
        "{set} set: {} crates, worst category {}",
        crates.len(),
        category(worst)
    );
    let rows = crates.iter().map(|c| {
        let native = Scope::Crate.native(&c.name);
        let library = native.map(|(native, described)| {
            format!(
                "{} ({}, {})",
                described.library,
                described.license,
                native.rank.label()
            )
        });
        (c, library, native.map(|(_, described)| described.how))
    });
    if markdown {
        let rows: Vec<Vec<String>> = rows
            .map(|(c, library, how)| {
                vec![
                    c.name.clone(),
                    c.version.clone(),
                    c.license.clone(),
                    category(c.rank).into(),
                    library.unwrap_or_default(),
                    how.unwrap_or_default().into(),
                ]
            })
            .collect();
        let head = [
            "Crate",
            "Version",
            "License",
            "Category",
            "Native library",
            "How used",
        ];
        return format!("{summary}\n{}", table(&head, &rows));
    }
    let mut lines = vec![summary];
    for (c, library, _) in rows {
        let mut line = format!(
            "{:<22} {:<10} {:<45} {}",
            c.name,
            c.version,
            c.license,
            category(c.rank)
        );
        if let Some(library) = library {
            line += &format!("  [native: {library}]");
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Audit the crate at `dir` in each configuration asked for; `Ok(false)` on a violation.
pub fn run(dir: &Path, options: &Options) -> Result<bool, String> {
    let mut failed = false;
    for set in &options.sets {
        let meta = match options.metadata()? {
            Some(meta) => meta,
            None => resolve(dir, set, &options.targets())?,
        };
        let (crates, problems) = audit(&meta, set, Scope::Crate)?;
        let text = render(&crates, set, options.markdown);
        println!(
            "{}",
            if options.quiet {
                text.lines().next().unwrap_or_default()
            } else {
                &text
            }
        );
        for problem in &problems {
            println!("FAIL {set}: {problem}");
        }
        if problems.is_empty() {
            println!("PASS {set}: no copyleft component outside the allowed list");
        }
        println!();
        failed |= !problems.is_empty();
    }
    Ok(!failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, license: &str, rank: Rank, native_rank: Rank) -> Crate {
        Crate {
            name: name.into(),
            version: "0.1.0".into(),
            license: license.into(),
            expr: None,
            rank,
            native_rank,
            manifest: String::new(),
        }
    }

    #[test]
    fn rows_keep_the_columns_licenses_md_is_written_from() {
        let crates = [
            row("pixelflux", "MPL-2.0", Rank::FileLevel, Rank::Permissive),
            row("libc", "MIT OR Apache-2.0", Rank::Permissive, Rank::Weak),
        ];
        let runtime = "C runtime (glibc, or musl on musllinux wheels) (LGPL-2.1-or-later (glibc), MIT \
                       (musl), weak copyleft)";
        assert_eq!(
            render(&crates, "gpl", false),
            format!(
                "gpl set: 2 crates, worst category weak copyleft\n\
                 pixelflux              0.1.0      MPL-2.0                                       \
                 permissive (file-level copyleft)\n\
                 libc                   0.1.0      MIT OR Apache-2.0                             \
                 permissive  [native: {runtime}]"
            )
        );
        assert_eq!(
            render(&crates, "gpl", true),
            format!(
                "gpl set: 2 crates, worst category weak copyleft\n\
                 | Crate | Version | License | Category | Native library | How used |\n\
                 | --- | --- | --- | --- | --- | --- |\n\
                 | pixelflux | 0.1.0 | MPL-2.0 | permissive (file-level copyleft) |  |  |\n\
                 | libc | 0.1.0 | MIT OR Apache-2.0 | permissive | {runtime} | linked shared library \
                 (system), as for every program |"
            )
        );
    }
}
