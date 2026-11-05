/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one error type the public API returns. Each variant names the part of pixelflux
//! that refused: the settings a consumer handed over, a capture, the compositor, or a
//! capability the session lacks (a compositor without the virtual-keyboard protocol). The
//! Python bridge maps them to its exceptions (`ValueError` for settings,
//! `VirtualKeyboardUnavailable` for a missing capability, `RuntimeError` otherwise).

use std::fmt;

/// Why a call into pixelflux failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A setting is outside what the backend takes.
    Settings(String),
    /// A capture could not start, stop, or be reconfigured.
    Capture(String),
    /// The compositor refused or did not answer a command.
    Compositor(String),
    /// The session lacks the capability the call needs.
    Unavailable(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Settings(m)
            | Error::Capture(m)
            | Error::Compositor(m)
            | Error::Unavailable(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(m: String) -> Self {
        Error::Capture(m)
    }
}
