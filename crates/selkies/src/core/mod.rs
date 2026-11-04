/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Types and helpers every part of the server shares: display and client identities, roles,
//! and the expansion of a configured path that starts at the home directory.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// The display a page renders: `primary`, or the id a second display page was given.
pub type DisplayId = String;

/// The primary display's id.
pub const PRIMARY_DISPLAY: &str = "primary";

/// What a connection may do, from the threat model's principals: a shared viewer watches, a
/// collaborator also drives keyboard, mouse, and clipboard, a controller everything a
/// desktop user can.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Viewer,
    Collaborator,
    Controller,
}

impl Role {
    /// The role's wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Collaborator => "collaborator",
            Role::Controller => "controller",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `~` and `~/...` under `$HOME`, any other path as written; see [`expand_user_in`].
pub fn expand_user(path: &str) -> PathBuf {
    expand_user_in(path, std::env::var_os("HOME").as_deref())
}

/// `~` and `~/...` under `home`, the shell's rule for a configured path.
///
/// `home` is taken as bytes, so a home directory that is not UTF-8 still expands; a trailing
/// `/` on it is dropped so `~/x` never doubles the separator, and an empty one leaves the
/// root. `~name` is another user's home, which the server never looks up, so it stays as
/// written, as does every path when `home` is unset.
pub fn expand_user_in(path: &str, home: Option<&OsStr>) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return PathBuf::from(path),
    };
    let Some(home) = home else {
        return PathBuf::from(path);
    };
    let mut bytes = home.as_bytes().to_vec();
    while bytes.last() == Some(&b'/') {
        bytes.pop();
    }
    bytes.extend_from_slice(rest.as_bytes());
    if bytes.is_empty() {
        bytes.push(b'/');
    }
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(path: &str, home: &str) -> PathBuf {
        expand_user_in(path, Some(OsStr::new(home)))
    }

    #[test]
    fn a_tilde_path_lands_under_the_home_directory() {
        assert_eq!(expand("~", "/home/u"), PathBuf::from("/home/u"));
        assert_eq!(
            expand("~/Desktop", "/home/u"),
            PathBuf::from("/home/u/Desktop")
        );
    }

    #[test]
    fn a_trailing_slash_on_home_does_not_double_the_separator() {
        assert_eq!(expand("~/x", "/home/u/"), PathBuf::from("/home/u/x"));
        assert_eq!(expand("~", "/"), PathBuf::from("/"));
    }

    #[test]
    fn an_empty_home_leaves_the_root() {
        assert_eq!(expand("~/x", ""), PathBuf::from("/x"));
        assert_eq!(expand("~", ""), PathBuf::from("/"));
    }

    #[test]
    fn another_users_home_and_plain_paths_stay_as_written() {
        assert_eq!(expand("~other/x", "/home/u"), PathBuf::from("~other/x"));
        assert_eq!(expand("/srv/files", "/home/u"), PathBuf::from("/srv/files"));
        assert_eq!(expand("rel/~/x", "/home/u"), PathBuf::from("rel/~/x"));
    }

    #[test]
    fn an_unset_home_leaves_the_path_as_written() {
        assert_eq!(expand_user_in("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn a_home_that_is_not_utf8_still_expands() {
        let home = OsStr::from_bytes(b"/home/\xff");
        let expanded = expand_user_in("~/x", Some(home));
        assert_eq!(expanded.as_os_str().as_bytes(), b"/home/\xff/x");
    }
}
