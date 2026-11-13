/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The XDG base directories, read from the environment with the specification's defaults, so
//! the icon lookup and every caller searching user files agree on where they are.

use std::path::{Component, Path, PathBuf};

/// `dir` where it is absolute and names no parent. The XDG Base Directory specification has a
/// relative path in its variables ignored, and no file these directories hold is named through
/// a parent.
fn usable(dir: PathBuf) -> Option<PathBuf> {
    (dir.is_absolute() && !dir.components().any(|c| c == Component::ParentDir)).then_some(dir)
}

/// The user's home directory, the root when `HOME` is unset or unusable.
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(usable)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The user's configuration directory (`XDG_CONFIG_HOME`, else `~/.config`).
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .and_then(usable)
        .unwrap_or_else(|| home().join(".config"))
}

/// The user's state directory (`XDG_STATE_HOME`, else `~/.local/state`).
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .and_then(usable)
        .unwrap_or_else(|| home().join(".local/state"))
}

/// The user's data directory (`XDG_DATA_HOME`, else `~/.local/share`).
pub fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .and_then(usable)
        .unwrap_or_else(|| home().join(".local/share"))
}

/// The system data directories in precedence order, `/usr/share` and `/usr/local/share` always
/// among them.
pub fn data_dirs() -> Vec<PathBuf> {
    let raw = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty());
    let raw = raw.unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    let mut dirs: Vec<PathBuf> = raw
        .split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .filter_map(usable)
        .collect();
    for required in ["/usr/share", "/usr/local/share"] {
        if !dirs.iter().any(|d| d == Path::new(required)) {
            dirs.push(PathBuf::from(required));
        }
    }
    dirs
}

/// The `share` directory of the Selkies release bundle the running executable belongs to, which
/// carries what Selkies itself ships for a desktop (its logo): `share` beside the executable's
/// `bin/` where a `libexec/selkies` marks the tree a bundle, else `None`.
pub fn bundle_share() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let root = exe.parent()?.parent()?;
    root.join("libexec/selkies").is_dir().then(|| root.join("share"))
}

/// The system configuration directories in precedence order (`XDG_CONFIG_DIRS`, else `/etc/xdg`).
pub fn config_dirs() -> Vec<PathBuf> {
    let raw = std::env::var("XDG_CONFIG_DIRS").ok().filter(|v| !v.is_empty());
    let raw = raw.unwrap_or_else(|| "/etc/xdg".to_string());
    let mut dirs: Vec<PathBuf> = raw
        .split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .filter_map(usable)
        .collect();
    if !dirs.iter().any(|d| d == Path::new("/etc/xdg")) {
        dirs.push(PathBuf::from("/etc/xdg"));
    }
    dirs
}
