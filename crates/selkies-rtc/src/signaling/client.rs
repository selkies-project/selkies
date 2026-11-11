/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The server peer's side of signaling: the client the streaming service registers with.
//!
//! `SignalingClient::spawn` keeps one connection to the signaling server up through a
//! `Connector`, registers it with the server peer's hello, and turns what arrives into
//! `ClientEvent`s; it reconnects on a failure, and a malformed or unrecognized line is logged
//! and dropped, never allowed to tear down the connection every peer shares.
//! `RegistryConnector` connects in process, straight to the `PeerRegistry` with no socket
//! between; a deployment that signals over a WebSocket or a Unix socket brings its own.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use log::{debug, warn};
use tokio::sync::{mpsc, watch};

use super::registry::PeerRegistry;
use super::socket::{Inbound, Outbound, socket};
use super::wire::{
    IceBody, ServerLine, SessionStart, ice_line, parse_server_line, sdp_line, server_hello,
};

/// The wait after a connection that failed.
pub const RECONNECT_AFTER_FAILURE: Duration = Duration::from_secs(2);
/// The wait after a connection that ended.
pub const RECONNECT_AFTER_END: Duration = Duration::from_millis(100);

/// What the service hears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    SessionStart(SessionStart),
    SessionEnd {
        peer_id: String,
        client_type: String,
    },
    Sdp {
        peer_id: String,
        kind: String,
        sdp: String,
    },
    Ice {
        peer_id: String,
        ice: IceBody,
    },
    /// A server `ERROR` line.
    Error(String),
    /// The connection ended; the client is reconnecting.
    Disconnected,
}

/// Why a send was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignalingError {
    /// There is no connection to send on.
    #[error("WebSocket connection not available")]
    NotConnected,
}

/// One open connection to the signaling server, as lines each way.
pub struct ClientLink {
    pub outgoing: mpsc::UnboundedSender<String>,
    pub incoming: mpsc::UnboundedReceiver<String>,
}

/// A future a `Connector` returns.
pub type ConnectFuture<'a> = Pin<Box<dyn Future<Output = Result<ClientLink, String>> + Send + 'a>>;

/// How the client reaches the signaling server.
pub trait Connector: Send + Sync + 'static {
    /// Open a connection.
    fn connect(&self) -> ConnectFuture<'_>;
}

/// Connects in process, as one more peer of the registry.
pub struct RegistryConnector(pub Arc<PeerRegistry>);

impl Connector for RegistryConnector {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(async move {
            let registry = Arc::clone(&self.0);
            let (peer_socket, mut outbox) = socket();
            let (inbound_tx, inbound_rx) = mpsc::channel(1024);
            let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
            let (incoming_tx, incoming) = mpsc::unbounded_channel::<String>();
            tokio::spawn(async move {
                registry
                    .serve(peer_socket, inbound_rx, "in-process", None, None)
                    .await;
            });
            tokio::spawn(async move {
                while let Some(line) = outgoing_rx.recv().await {
                    if inbound_tx.send(Inbound::Text(line)).await.is_err() {
                        return;
                    }
                }
                let _ = inbound_tx.send(Inbound::CloseFrame).await;
            });
            tokio::spawn(async move {
                while let Some(outbound) = outbox.recv().await {
                    match outbound {
                        Outbound::Text(line) => {
                            if incoming_tx.send(line).is_err() {
                                return;
                            }
                        }
                        Outbound::Close { .. } | Outbound::Abort => return,
                        Outbound::Ping(_) | Outbound::Pong(_) => {}
                    }
                }
            });
            Ok(ClientLink { outgoing, incoming })
        })
    }
}

/// The running server peer.
pub struct SignalingClient {
    outgoing: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    stop: watch::Sender<bool>,
}

impl SignalingClient {
    /// Start the connection loop. `server_token` is what proves this peer may claim the
    /// server role: the master token in secure mode, else the registry's
    /// `server_peer_secret`.
    pub fn spawn(
        connector: Arc<dyn Connector>,
        server_token: Option<String>,
    ) -> (SignalingClient, mpsc::UnboundedReceiver<ClientEvent>) {
        let (events, event_rx) = mpsc::unbounded_channel();
        let outgoing = Arc::new(Mutex::new(None));
        let (stop, stop_rx) = watch::channel(false);
        tokio::spawn(run(
            connector,
            server_token,
            events,
            Arc::clone(&outgoing),
            stop_rx,
        ));
        (SignalingClient { outgoing, stop }, event_rx)
    }

    fn send_line(&self, line: String) -> Result<(), SignalingError> {
        self.outgoing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .ok_or(SignalingError::NotConnected)?
            .send(line)
            .map_err(|_| SignalingError::NotConnected)
    }

    /// Send a description to a peer.
    ///
    /// # Errors
    ///
    /// There is no connection.
    pub fn send_sdp(&self, kind: &str, sdp: &str, peer_id: &str) -> Result<(), SignalingError> {
        self.send_line(sdp_line(peer_id, kind, sdp))
    }

    /// Send a candidate to a peer.
    ///
    /// # Errors
    ///
    /// There is no connection.
    pub fn send_ice(
        &self,
        mline_index: u32,
        candidate: &str,
        peer_id: &str,
    ) -> Result<(), SignalingError> {
        self.send_line(ice_line(peer_id, mline_index, candidate))
    }

    /// Stop the client and end its connection.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

async fn run(
    connector: Arc<dyn Connector>,
    server_token: Option<String>,
    events: mpsc::UnboundedSender<ClientEvent>,
    outgoing: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    mut stop: watch::Receiver<bool>,
) {
    while !*stop.borrow() {
        debug!(target: "signaling", "Connecting to signaling server");
        let link = tokio::select! {
            link = connector.connect() => link,
            _ = stop.changed() => return,
        };
        let wait = match link {
            Err(err) => {
                warn!(target: "signaling", "Connection failed, retrying... {err}");
                RECONNECT_AFTER_FAILURE
            }
            Ok(mut link) => {
                let _ = link.outgoing.send(server_hello(server_token.as_deref()));
                *outgoing.lock().unwrap_or_else(PoisonError::into_inner) = Some(link.outgoing);
                loop {
                    let line = tokio::select! {
                        line = link.incoming.recv() => line,
                        _ = stop.changed() => {
                            outgoing.lock().unwrap_or_else(PoisonError::into_inner).take();
                            return;
                        }
                    };
                    let Some(line) = line else {
                        break;
                    };
                    dispatch(&line, &events);
                }
                outgoing
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                let _ = events.send(ClientEvent::Disconnected);
                RECONNECT_AFTER_END
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.changed() => return,
        }
    }
}

fn dispatch(line: &str, events: &mpsc::UnboundedSender<ClientEvent>) {
    let event = match parse_server_line(line) {
        ServerLine::Hello => {
            debug!(target: "signaling", "Connection established with the signaling server");
            return;
        }
        ServerLine::SessionStart(start) => ClientEvent::SessionStart(start),
        ServerLine::SessionEnd {
            peer_id,
            client_type,
        } => ClientEvent::SessionEnd {
            peer_id,
            client_type,
        },
        ServerLine::Error(text) => ClientEvent::Error(text),
        ServerLine::Sdp { peer_id, body } => ClientEvent::Sdp {
            peer_id,
            kind: body.kind,
            sdp: body.sdp,
        },
        ServerLine::Ice { peer_id, body } => ClientEvent::Ice { peer_id, ice: body },
        ServerLine::Ignored(why) => {
            warn!(target: "signaling", "Ignoring {why}");
            return;
        }
    };
    let _ = events.send(event);
}
