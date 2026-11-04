/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X11 names where no X server runs: the monitor never builds, answering the absence the
//! way a Linux build answers a dead display, and every probe reports no display live, so the
//! clipboard's fallback ladder falls through to its last rung and the surface reports no
//! backend. The Windows clipboard milestone lands in the place this file holds.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{Entry, Payload};

/// The selection-owner change edge, the same wait-and-take contract the monitor loop
/// consumes; off Linux nothing raises it, so a wait runs out its timeout.
pub struct ChangeEdge {
    set: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Default for ChangeEdge {
    fn default() -> Self {
        ChangeEdge {
            set: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }
}

impl ChangeEdge {
    /// Whether the edge is raised within `timeout`; it stays raised. Off Linux it never is,
    /// so the wait is the timeout.
    pub async fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return self.set.load(Ordering::Acquire);
            }
        }
    }

    /// Consume the edge; whether it was raised. Off Linux it never was.
    pub fn take(&self) -> bool {
        self.set.swap(false, Ordering::AcqRel)
    }
}

/// The XFixes selection monitor where no X server runs: the build fails and the ladder falls
/// to its next rung, which is the answer a Linux build gives a display that is not there.
pub struct X11Monitor {
    changed: ChangeEdge,
}

impl X11Monitor {
    /// No X server runs off Linux to connect to.
    pub fn new(_display: Option<&str>) -> Result<X11Monitor, String> {
        Err("no X server runs on this OS".to_string())
    }

    /// The owner-change edge; nothing raises it off Linux.
    pub fn changed(&self) -> &ChangeEdge {
        &self.changed
    }

    /// No monitor runs off Linux.
    pub fn alive(&self) -> bool {
        false
    }

    /// No selection is owned off Linux.
    pub fn owns_selection(&self) -> bool {
        false
    }

    /// Nothing runs, so there is nothing to close.
    pub fn close(&self) {}

    /// No monitor runs to read through.
    pub fn read(&self, _use_binary: bool) -> Result<Option<Payload>, String> {
        Ok(None)
    }

    /// No monitor runs to offer through.
    pub fn offer(&self, _entries: Vec<Entry>) -> Result<bool, String> {
        Ok(false)
    }
}

/// No X display has a socket off Linux.
pub fn display_socket(_name: &str) -> Option<String> {
    None
}

/// No X display is live off Linux.
pub fn display_live(_name: &str) -> bool {
    false
}

/// No socket answers off Linux, so a compositor's bring-up never completes by this probe.
pub fn socket_live(_path: &Path) -> bool {
    false
}

/// No X server runs off Linux, so none is an Xwayland.
pub fn display_is_xwayland(_display: &str) -> bool {
    false
}

/// No file manager's uri-list resolves to a picture off Linux.
pub fn resolve_uri_list_image(_data: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    None
}
