/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Who owns a display's media over WebRTC, and when it is released, with no media in it.
//!
//! A display's media graph follows its consumers: on the primary a lone viewer brings it up
//! (websockets parity), since the desktop exists whether or not anyone controls it; a
//! secondary display exists only through its controller's layout, so its viewer cannot bring
//! one up, its controller leaving ends it with its viewers, and a secondary whose controller
//! never connected leaves nothing behind. `DisplayGraphs` decides; the engine acts on what
//! it returns, deregistering a peer before closing its connection so every step runs once.
//!
//! A page reload drops and re-adds its peer within a second or two, so the primary capture's
//! stop is deferred by `StopGrace` and canceled when a consumer reclaims the display; a
//! secondary's stop is immediate.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::task::JoinHandle;

use super::wire::ClientType;

/// The display no controller has to bring up.
pub const PRIMARY: &str = "primary";

/// How long the primary capture outlives its last consumer.
pub const RECONNECT_GRACE: Duration = Duration::from_secs(3);

/// A peer may not join: its display has no graph and it cannot build one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "Cannot create peer connection: no media graph for display '{0}'. Controller may be disconnected."
)]
pub struct NoGraph(pub String);

/// What the engine does after a peer left or failed to start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Release {
    /// Stop this display's media and drop its graph.
    pub stop_display: Option<String>,
    /// Close these viewers, bound to a graph that is gone: a sender would block forever,
    /// frozen on the last frame, while ICE stays connected and the client never heals.
    pub close_viewers: Vec<String>,
}

/// The displays with a graph and the peers consuming them.
#[derive(Debug, Default)]
pub struct DisplayGraphs {
    graphs: HashSet<String>,
    peers: HashMap<String, (String, ClientType)>,
}

impl DisplayGraphs {
    /// Whether a display has a graph.
    pub fn has_graph(&self, display: &str) -> bool {
        self.graphs.contains(display)
    }

    /// Whether a peer is registered.
    pub fn has_peer(&self, peer: &str) -> bool {
        self.peers.contains_key(peer)
    }

    /// Before a peer's offer: reuse the display's graph, or build one where the peer may.
    /// Returns whether a graph was built.
    ///
    /// # Errors
    ///
    /// A viewer of a secondary display that has no graph.
    pub fn admit(&mut self, kind: ClientType, display: &str) -> Result<bool, NoGraph> {
        if self.graphs.contains(display) {
            return Ok(false);
        }
        if kind == ClientType::Controller || display == PRIMARY {
            self.graphs.insert(display.to_owned());
            return Ok(true);
        }
        Err(NoGraph(display.to_owned()))
    }

    /// After a peer's offer went out: the peer consumes the display.
    pub fn register(&mut self, peer: &str, kind: ClientType, display: &str) {
        self.peers
            .insert(peer.to_owned(), (display.to_owned(), kind));
    }

    /// A registered peer left: deregister it and release what that ends.
    pub fn leave(&mut self, peer: &str) -> Release {
        match self.peers.remove(peer) {
            Some((display, kind)) => {
                self.release_if_unconsumed(&display, kind == ClientType::Controller)
            }
            None => Release::default(),
        }
    }

    /// A start failed before the peer was registered: release the display it claimed, which
    /// no later event would.
    pub fn failed_start(&mut self, kind: ClientType, display: &str) -> Release {
        self.release_if_unconsumed(display, kind == ClientType::Controller)
    }

    /// A secondary display is its controller's: the controller leaving ends it. The
    /// primary's media serve the remaining viewers across a controller's departure and are
    /// released with the last consumer. A display with no graph and no departing controller
    /// was already released or never built.
    fn release_if_unconsumed(&mut self, display: &str, controller_left: bool) -> Release {
        let remaining = self.peers.values().any(|(d, _)| d == display);
        if remaining && !(controller_left && display != PRIMARY) {
            return Release::default();
        }
        if !self.graphs.contains(display) && !controller_left {
            return Release::default();
        }
        self.graphs.remove(display);
        Release {
            stop_display: Some(display.to_owned()),
            close_viewers: self
                .peers
                .iter()
                .filter(|(_, (d, kind))| d == display && *kind != ClientType::Controller)
                .map(|(peer, _)| peer.clone())
                .collect(),
        }
    }
}

/// The primary capture's deferred stop: at most one is pending.
#[derive(Default)]
pub struct StopGrace {
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl StopGrace {
    /// A grace with nothing pending.
    pub fn new() -> StopGrace {
        StopGrace::default()
    }

    /// Whether a stop is pending.
    pub fn is_pending(&self) -> bool {
        self.task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    /// Run `stop` after `grace` unless canceled or a consumer holds the display then; nothing
    /// is done when one is already pending.
    pub fn schedule<C, S, F>(&self, grace: Duration, has_consumer: C, stop: S)
    where
        C: FnOnce() -> bool + Send + 'static,
        S: FnOnce() -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let mut pending = self.task.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        *pending = Some(tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if has_consumer() {
                log::info!(target: "webrtc", "Primary reclaimed within the grace; capture kept.");
            } else {
                log::info!(target: "webrtc", "Primary unclaimed after the grace; stopping its capture.");
                stop().await;
            }
        }));
    }

    /// Drop the pending stop: a consumer reclaimed the display, or the service is stopping.
    pub fn cancel(&self) {
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}
