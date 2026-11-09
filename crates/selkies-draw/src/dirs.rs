/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The XDG base directories, read from the environment with the specification's defaults, so
//! the icon lookup and every caller searching user files agree on where they are.

use std::path::{Path, PathBuf};

/// The user's home directory, the root when `HOME` is unset.
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The user's configuration directory (`XDG_CONFIG_HOME`, else `~/.config`).
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

/// The user's state directory (`XDG_STATE_HOME`, else `~/.local/state`).
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}

/// The user's data directory (`XDG_DATA_HOME`, else `~/.local/share`).
pub fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
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
        .collect();
    for required in ["/usr/share", "/usr/local/share"] {
        if !dirs.iter().any(|d| d == Path::new(required)) {
            dirs.push(PathBuf::from(required));
        }
    }
    dirs
}

/// The system configuration directories in precedence order (`XDG_CONFIG_DIRS`, else `/etc/xdg`).
pub fn config_dirs() -> Vec<PathBuf> {
    let raw = std::env::var("XDG_CONFIG_DIRS").ok().filter(|v| !v.is_empty());
    let raw = raw.unwrap_or_else(|| "/etc/xdg".to_string());
    let mut dirs: Vec<PathBuf> = raw
        .split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect();
    if !dirs.iter().any(|d| d == Path::new("/etc/xdg")) {
        dirs.push(PathBuf::from("/etc/xdg"));
    }
    dirs
}
