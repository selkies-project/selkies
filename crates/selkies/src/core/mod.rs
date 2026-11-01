/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Types every part of the server shares: display and client identities, roles, and the
//! errors the modules return.

use std::fmt;

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
