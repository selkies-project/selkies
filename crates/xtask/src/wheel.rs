/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The selkies wheel of a platform, repackaged from its Selkies bundle.
//!
//! The bundle (`infra/bundle/build-in-image.sh`) is already everything the wheel installs, so
//! nothing is compiled or repaired here: its `bin/` becomes the wheel's scripts, which pip puts
//! on the environment's PATH, and its `lib/`, `libexec/`, and `share/` become the wheel's data,
//! which pip installs under the same prefix. That keeps the tree as the bundle lays it out
//! (`<prefix>/bin/selkies` beside `<prefix>/libexec/selkies`), so the executable finds its
//! payload and its libraries exactly as it does unpacked from the bundle, in a virtualenv, a
//! `--user` install, or a system prefix alike. The one Python module, the Jupyter entry point,
//! comes from `infra/wheel/selkies`, and the metadata from `pyproject.toml`.
//!
//! The platform tag is the bundle's C library floor, the platform of the image it is built in,
//! which `infra/bundle/toolchain.sh platform` reads from where `infra/bundle/Dockerfile` pins
//! it; the wheel is `py3-none`, since no part of it is bound to an interpreter. Symbolic links,
//! which a wheel cannot carry, are stored as the files they name.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use regex::Regex;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::{Scratch, repo};

const USAGE: &str = "usage: cargo xtask wheel <bundle.tar.gz> <out-dir>";
/// The bundle's directories pip installs under the prefix, beside `bin/`.
const DATA_DIRS: [&str; 3] = ["lib", "libexec", "share"];

/// Build the wheel of the bundle the arguments name into the directory they name, and print
/// its path.
pub fn run(args: &[String]) -> Result<bool, String> {
    let [bundle, out] = args else {
        return Err(USAGE.into());
    };
    let wheel = build(Path::new(bundle), Path::new(out))?;
    println!("{}", wheel.display());
    Ok(true)
}

/// What a bundle's file name says: its version, architecture, and C library.
#[derive(Debug, PartialEq)]
struct Bundle {
    name: String,
    version: String,
    arch: String,
    libc: String,
}

fn parse_name(file_name: &str) -> Result<Bundle, String> {
    let name = [".tar.gz", ".tgz"]
        .iter()
        .find_map(|suffix| file_name.strip_suffix(suffix))
        .unwrap_or(file_name);
    let pattern =
        Regex::new(r"^selkies-(?P<version>.+)-(?P<arch>x86_64|aarch64)-linux-(?P<libc>gnu|musl)$")
            .expect("a valid pattern");
    let captures = pattern.captures(name).ok_or_else(|| {
        format!("{file_name}: not a bundle named selkies-<version>-<arch>-linux-<gnu|musl>")
    })?;
    Ok(Bundle {
        name: name.to_string(),
        version: captures["version"].to_string(),
        arch: captures["arch"].to_string(),
        libc: captures["libc"].to_string(),
    })
}

/// The wheel platform tag of a bundle built for `libc` on `arch`.
fn platform_tag(libc: &str, arch: &str) -> Result<String, String> {
    let toolchain_libc = if libc == "musl" { "musl" } else { "glibc" };
    let output = Command::new("sh")
        .arg(repo().join("infra/bundle/toolchain.sh"))
        .args(["platform", toolchain_libc, arch])
        .output()
        .map_err(|e| format!("infra/bundle/toolchain.sh: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "infra/bundle/toolchain.sh platform {toolchain_libc} {arch}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let platform = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(format!("{platform}_{arch}"))
}

/// The `[project]` table of `pyproject.toml`.
fn project() -> Result<toml::Table, String> {
    let text = fs::read_to_string(repo().join("pyproject.toml"))
        .map_err(|e| format!("pyproject.toml: {e}"))?;
    let mut document: toml::Table = text.parse().map_err(|e| format!("pyproject.toml: {e}"))?;
    match document.remove("project") {
        Some(toml::Value::Table(project)) => Ok(project),
        _ => Err("pyproject.toml has no [project] table".into()),
    }
}

fn string<'a>(table: &'a toml::Table, key: &str) -> Result<&'a str, String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("pyproject.toml: [project] has no string {key}"))
}

fn strings(value: &toml::Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(toml::Value::as_str).collect())
        .unwrap_or_default()
}

/// The core metadata (version 2.4) of the selkies distribution, from `pyproject.toml`.
fn metadata(project: &toml::Table, version: &str) -> Result<String, String> {
    let mut lines = vec![
        "Metadata-Version: 2.4".to_string(),
        format!("Name: {}", string(project, "name")?),
        format!("Version: {version}"),
        format!("Summary: {}", string(project, "description")?),
        format!("License-Expression: {}", string(project, "license")?),
        "License-File: LICENSE".to_string(),
    ];
    for author in project
        .get("authors")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let author = author
            .as_table()
            .ok_or("pyproject.toml: an author that is not a table")?;
        lines.push(format!(
            "Author-email: {} <{}>",
            string(author, "name")?,
            string(author, "email")?
        ));
    }
    for classifier in project.get("classifiers").map(strings).unwrap_or_default() {
        lines.push(format!("Classifier: {classifier}"));
    }
    for (label, url) in project
        .get("urls")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
    {
        let url = url
            .as_str()
            .ok_or_else(|| format!("pyproject.toml: the URL {label} is not a string"))?;
        lines.push(format!("Project-URL: {label}, {url}"));
    }
    let extras = project
        .get("optional-dependencies")
        .and_then(toml::Value::as_table);
    for (extra, requirements) in extras.into_iter().flatten() {
        lines.push(format!("Provides-Extra: {extra}"));
        for requirement in strings(requirements) {
            lines.push(format!(
                "Requires-Dist: {requirement}; extra == \"{extra}\""
            ));
        }
    }
    lines.push("Description-Content-Type: text/markdown".into());
    let readme = string(project, "readme")?;
    let readme = fs::read_to_string(repo().join(readme)).map_err(|e| format!("{readme}: {e}"))?;
    Ok(lines.join("\n") + "\n\n" + &readme)
}

/// The entry point groups `pyproject.toml` declares, as `entry_points.txt`.
fn entry_points(project: &toml::Table) -> Result<String, String> {
    let mut text = String::new();
    for (group, entries) in project
        .get("entry-points")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
    {
        let entries = entries.as_table().ok_or_else(|| {
            format!("pyproject.toml: the entry point group {group} is not a table")
        })?;
        text += &format!("[{group}]\n");
        for (name, target) in entries {
            let target = target
                .as_str()
                .ok_or_else(|| format!("pyproject.toml: the entry point {name} is not a string"))?;
            text += &format!("{name} = {target}\n");
        }
        text += "\n";
    }
    Ok(text)
}

/// Every file under `root` as (path under it, path on disk), in path order, links followed;
/// a link to a directory is skipped, as one a wheel cannot carry, and one out of `root` fails.
fn bundle_files(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in
            fs::read_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))?
        {
            let path = entry
                .map_err(|e| format!("{}: {e}", directory.display()))?
                .path();
            let link =
                fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let relative = path
                .strip_prefix(root)
                .expect("under the root")
                .to_path_buf();
            if link.is_dir() {
                directories.push(path);
                continue;
            }
            let target = path
                .canonicalize()
                .map_err(|e| format!("{}: {e}", relative.display()))?;
            if target.is_dir() {
                continue;
            }
            if !target.starts_with(&canonical_root) {
                return Err(format!("{}: a link out of the bundle", relative.display()));
            }
            files.push((relative, path));
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|(relative, path)| {
            let name = relative
                .to_str()
                .ok_or_else(|| format!("{}: not a UTF-8 path", relative.display()))?
                .to_string();
            Ok((name, path))
        })
        .collect()
}

/// A bundle file's mode as the wheel stores it: readable and writable by its owner, executable
/// by group and others only where by its owner, and writable by no one else, with no set-id or
/// sticky bit.
fn stored_mode(mode: u32) -> u32 {
    let mut mode = mode & 0o755;
    if mode & 0o100 == 0 {
        mode &= !0o111;
    }
    mode | 0o600
}

/// The RECORD line of `path` holding `data`.
fn record_line(path: &str, data: &[u8]) -> String {
    let digest = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(data));
    format!("{path},sha256={digest},{}", data.len())
}

/// The wheel's entries as they are written, each one's RECORD line kept for the last.
struct Wheel {
    zip: ZipWriter<File>,
    record: Vec<String>,
}

impl Wheel {
    fn add(&mut self, path: &str, data: &[u8], mode: u32) -> Result<(), String> {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(9))
            .last_modified_time(
                DateTime::from_date_and_time(2020, 1, 1, 0, 0, 0).expect("a valid date"),
            )
            .unix_permissions(mode & 0o777)
            .large_file(data.len() >= u32::MAX as usize);
        self.zip
            .start_file(path, options)
            .map_err(|e| format!("{path}: {e}"))?;
        self.zip
            .write_all(data)
            .map_err(|e| format!("{path}: {e}"))?;
        self.record.push(record_line(path, data));
        Ok(())
    }
}

/// Write the wheel of `bundle` into `out` and return its path.
fn build(bundle: &Path, out: &Path) -> Result<PathBuf, String> {
    let file_name = bundle
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{}: not a bundle", bundle.display()))?;
    let parsed = parse_name(file_name)?;
    let tag = format!("py3-none-{}", platform_tag(&parsed.libc, &parsed.arch)?);
    let project = project()?;
    let dist = format!("selkies-{}", parsed.version);
    let data_dir = format!("{dist}.data");
    let info_dir = format!("{dist}.dist-info");

    let scratch = Scratch::new("selkies-wheel")?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(bundle)
        .arg("-C")
        .arg(&scratch.0)
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    if !status.success() {
        return Err(format!("{file_name}: tar could not unpack it"));
    }
    let files = bundle_files(&scratch.0.join(&parsed.name))?;
    if !files.iter().any(|(path, _)| path == "bin/selkies") {
        return Err(format!("{file_name}: no bin/selkies"));
    }

    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let path = out.join(format!("{dist}-{tag}.whl"));
    let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut wheel = Wheel {
        zip: ZipWriter::new(file),
        record: Vec::new(),
    };

    let modules = repo().join("infra/wheel/selkies");
    let mut sources: Vec<PathBuf> = fs::read_dir(&modules)
        .map_err(|e| format!("{}: {e}", modules.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "py"))
        .collect();
    sources.sort();
    for source in sources {
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a module name");
        let data = fs::read(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        wheel.add(&format!("selkies/{name}"), &data, 0o644)?;
    }
    for (relative, source) in files {
        let (top, rest) = relative.split_once('/').unwrap_or((&relative, ""));
        let entry = if top == "bin" {
            format!("{data_dir}/scripts/{rest}")
        } else if DATA_DIRS.contains(&top) {
            format!("{data_dir}/data/{relative}")
        } else {
            return Err(format!(
                "{relative}: outside the directories a wheel installs"
            ));
        };
        let data = fs::read(&source).map_err(|e| format!("{relative}: {e}"))?;
        let mode = fs::metadata(&source)
            .map_err(|e| format!("{relative}: {e}"))?
            .permissions()
            .mode();
        wheel.add(&entry, &data, stored_mode(mode))?;
    }
    wheel.add(
        &format!("{info_dir}/METADATA"),
        metadata(&project, &parsed.version)?.as_bytes(),
        0o644,
    )?;
    let generator = format!(
        "Wheel-Version: 1.0\nGenerator: selkies cargo xtask wheel\nRoot-Is-Purelib: false\nTag: {tag}\n"
    );
    wheel.add(&format!("{info_dir}/WHEEL"), generator.as_bytes(), 0o644)?;
    wheel.add(
        &format!("{info_dir}/entry_points.txt"),
        entry_points(&project)?.as_bytes(),
        0o644,
    )?;
    let license = fs::read(repo().join("LICENSE")).map_err(|e| format!("LICENSE: {e}"))?;
    wheel.add(&format!("{info_dir}/licenses/LICENSE"), &license, 0o644)?;

    let mut record = std::mem::take(&mut wheel.record);
    record.push(format!("{info_dir}/RECORD,,"));
    let record = record.join("\n") + "\n";
    wheel.add(&format!("{info_dir}/RECORD"), record.as_bytes(), 0o644)?;
    wheel
        .zip
        .finish()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn a_bundle_name_gives_its_version_architecture_and_c_library() {
        assert_eq!(
            parse_name("selkies-2.3.0rc1-aarch64-linux-musl.tar.gz"),
            Ok(Bundle {
                name: "selkies-2.3.0rc1-aarch64-linux-musl".into(),
                version: "2.3.0rc1".into(),
                arch: "aarch64".into(),
                libc: "musl".into(),
            })
        );
        assert!(parse_name("selkies-2.3.0-riscv64-linux-gnu.tar.gz").is_err());
        assert!(parse_name("pixelflux-2.3.0-x86_64-linux-gnu.tar.gz").is_err());
    }

    #[test]
    fn a_stored_mode_is_the_owners_to_write_and_no_one_elses() {
        assert_eq!(stored_mode(0o100444), 0o644);
        assert_eq!(stored_mode(0o104777), 0o755);
        assert_eq!(stored_mode(0o100651), 0o640);
        assert_eq!(stored_mode(0o100700), 0o700);
    }

    #[test]
    fn a_record_line_holds_the_unpadded_url_safe_digest_and_size() {
        assert_eq!(
            record_line("a", b""),
            "a,sha256=47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU,0"
        );
    }

    #[test]
    fn the_metadata_and_entry_points_come_from_pyproject() {
        let project = project().expect("pyproject.toml");
        let metadata = metadata(&project, "1.2.3").expect("metadata");
        assert!(metadata.starts_with("Metadata-Version: 2.4\nName: selkies\nVersion: 1.2.3\n"));
        assert!(metadata.contains("\nLicense-File: LICENSE\n"));
        assert!(metadata.contains("\nDescription-Content-Type: text/markdown\n\n"));
        let entry_points = entry_points(&project).expect("entry points");
        assert!(
            entry_points.contains("[jupyter_serverproxy_servers]\n"),
            "{entry_points}"
        );
    }

    /// A bundle of a script, a library behind a link, and a data file builds a wheel that
    /// installs each where the bundle has it, with its mode, and records each.
    #[test]
    fn a_bundle_becomes_scripts_and_data_under_one_prefix() {
        let scratch = Scratch::new("selkies-wheel-test").expect("scratch");
        let name = "selkies-9.8.7-x86_64-linux-gnu";
        let tree = scratch.0.join("tree").join(name);
        for directory in ["bin", "lib/selkies", "share/selkies"] {
            fs::create_dir_all(tree.join(directory)).expect("tree");
        }
        fs::write(tree.join("bin/selkies"), b"#!/bin/sh\n").expect("script");
        fs::set_permissions(tree.join("bin/selkies"), fs::Permissions::from_mode(0o755))
            .expect("mode");
        fs::write(tree.join("lib/selkies/libx.so.1.0"), b"library").expect("library");
        std::os::unix::fs::symlink("libx.so.1.0", tree.join("lib/selkies/libx.so.1"))
            .expect("link");
        fs::write(tree.join("share/selkies/icon.svg"), b"<svg/>").expect("data");
        fs::set_permissions(
            tree.join("share/selkies/icon.svg"),
            fs::Permissions::from_mode(0o444),
        )
        .expect("mode");
        let bundle = scratch.0.join(format!("{name}.tar.gz"));
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&bundle)
            .arg("-C")
            .arg(scratch.0.join("tree"))
            .arg(name)
            .status()
            .expect("tar");
        assert!(status.success());

        let wheel = build(&bundle, &scratch.0.join("out")).expect("wheel");
        let file_name = wheel
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a name");
        assert!(
            file_name.starts_with("selkies-9.8.7-py3-none-manylinux_2_"),
            "{file_name}"
        );
        assert!(file_name.ends_with("_x86_64.whl"), "{file_name}");

        let mut archive = zip::ZipArchive::new(File::open(&wheel).expect("open")).expect("zip");
        let mut contents = std::collections::BTreeMap::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).expect("entry");
            let mut data = Vec::new();
            entry.read_to_end(&mut data).expect("read");
            contents.insert(
                entry.name().expect("a name").to_string(),
                (data, entry.unix_mode().expect("a mode")),
            );
        }
        let data = "selkies-9.8.7.data";
        assert_eq!(
            contents[&format!("{data}/scripts/selkies")],
            (b"#!/bin/sh\n".to_vec(), 0o100755)
        );
        assert_eq!(
            contents[&format!("{data}/data/lib/selkies/libx.so.1")].0,
            b"library"
        );
        assert_eq!(
            contents[&format!("{data}/data/lib/selkies/libx.so.1.0")].0,
            b"library"
        );
        assert_eq!(
            contents[&format!("{data}/data/share/selkies/icon.svg")],
            (b"<svg/>".to_vec(), 0o100644)
        );
        assert!(contents.contains_key("selkies/__init__.py"));
        let info = "selkies-9.8.7.dist-info";
        let wheel_file =
            String::from_utf8(contents[&format!("{info}/WHEEL")].0.clone()).expect("text");
        assert!(
            wheel_file.contains("Root-Is-Purelib: false\nTag: py3-none-manylinux_2_"),
            "{wheel_file}"
        );
        let record =
            String::from_utf8(contents[&format!("{info}/RECORD")].0.clone()).expect("text");
        let lines: Vec<&str> = record.lines().collect();
        assert_eq!(lines.len(), contents.len());
        assert_eq!(lines.last(), Some(&format!("{info}/RECORD,,").as_str()));
        for (path, (data, _)) in &contents {
            if !path.ends_with("/RECORD") {
                assert!(
                    lines.contains(&record_line(path, data).as_str()),
                    "{path} unrecorded"
                );
            }
        }
    }
}
