/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The XDG base directories, read from the environment with the specification's defaults, so
//! the icon lookup and every caller searching user files agree on where they are.
//!
//! A session the server quarantined carries [`QUARANTINE_VAR`], naming the quarantine: a
//! directory of Selkies' own that the session reads and the user's own desktop does not. In
//! its full form the session's XDG homes are in it, so every file named through the homes here
//! lands there by itself. In its targeted form the homes stay the user's and the quarantine's
//! `config` and `data` lead the system directories the session searches: what Selkies writes
//! for the desktop's toolkits and window managers goes there ([`session_config_home`],
//! [`session_data_home`]), beneath anything the user configured, and a write that can only land
//! in the user's own configuration is left out ([`keeps_user_homes`]), but for the settings
//! daemons on a session bus of the session's own, which keep theirs in the quarantine
//! ([`settings_home`]). Either way
//! [`dotfile_home`] is where the few files a desktop reads from the home itself
//! (`~/.Xresources`, `~/.xsettingsd`) are written instead.

use std::path::{Component, Path, PathBuf};

/// The variable a quarantined session carries: the quarantine's root.
pub const QUARANTINE_VAR: &str = "SELKIES_QUARANTINE";

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

/// The root of the session's quarantine where [`QUARANTINE_VAR`] names a usable one, else
/// `None`.
pub fn quarantine() -> Option<PathBuf> {
    std::env::var_os(QUARANTINE_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .and_then(usable)
}

/// The directory the files a desktop reads from the home itself are written in: the home, or
/// `home` under the quarantine where the session has one, which nothing outside the session
/// reads.
pub fn dotfile_home() -> PathBuf {
    quarantine().map_or_else(home, |root| root.join("home"))
}

/// The variable a session whose settings daemons keep their settings in its quarantine carries:
/// the configuration home those daemons (dconf, xfconfd) were started with.
pub const SETTINGS_VAR: &str = "SELKIES_QUARANTINE_SETTINGS";

/// The configuration home the session's settings daemons keep their settings in, where the
/// quarantine gave them one of its own ([`SETTINGS_VAR`]), else `None`.
pub fn settings_home() -> Option<PathBuf> {
    std::env::var_os(SETTINGS_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .and_then(usable)
}

/// Whether the settings stores the session's desktop reads through a daemon (GSettings, xfconf)
/// may be written: everywhere but under a targeted quarantine whose settings daemons are the
/// user's own, where a write would reach the desktop the user logs into.
pub fn settings_stores_writable() -> bool {
    !keeps_user_homes() || settings_home().is_some()
}

/// Whether the session keeps the user's own XDG homes beside its quarantine (the targeted
/// quarantine): a file written in the configuration home reaches the desktop the user logs
/// into, so the session writes none, and the settings daemons write there unless the quarantine
/// gave them a home of their own ([`settings_stores_writable`]).
pub fn keeps_user_homes() -> bool {
    quarantine().is_some_and(|root| !config_home().starts_with(root))
}

/// Where the session writes the configuration it hands the desktop's toolkits and window
/// managers (`settings.ini`, `mimeapps.list`, `rc.xml`): the quarantine's `config` in a
/// quarantined session, which it reads beneath the user's own, else the configuration home.
pub fn session_config_home() -> PathBuf {
    quarantine().map_or_else(config_home, |root| root.join("config"))
}

/// Where the session writes the data it hands the desktop (desktop entries, window themes): the
/// quarantine's `data` in a quarantined session, else the data home.
pub fn session_data_home() -> PathBuf {
    quarantine().map_or_else(data_home, |root| root.join("data"))
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
