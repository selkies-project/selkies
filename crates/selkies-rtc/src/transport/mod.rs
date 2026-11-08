/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One peer's transport: ICE, DTLS, and SRTP under a handle the media and data layers use.
//!
//! A `Transport` is one task per peer plus the socket readers that feed it. The task owns the
//! `IceConnection`, the `Dtls` association, and the SRTP receive context, and drives all three
//! sans-IO: it reads the peer's inbound channel, advances their clocks, and sends what they
//! queue over the selected pair. A datagram the ICE layer hands up is told apart by its first
//! byte as RFC 7983 does: 20-63 is DTLS and goes to the association, 128-191 is SRTP or SRTCP
//! and is unprotected and delivered on the incoming channel with the instant it was read, which
//! is the arrival time transport-wide congestion control reports. The association starts only
//! once a pair is selected, so its handshake timers never run out against an ICE still waiting
//! for the peer; DTLS datagrams that arrive before that are held in order and fed then, as a
//! browser may begin its handshake on a pair this side has yet to see nominated.
//!
//! The handle sends synchronously on the caller's task: `send_rtp` and `send_rtcp` protect the
//! packet under the SRTP send context and write it to the selected pair's socket at once, with
//! no hop through the task; they return the instant the packet was written, the send time
//! transport-wide congestion control pairs with the feedback. The protection itself copies the
//! packet once into a buffer of its own, since `rtc_srtp` offers no in-place form. Application data for SCTP (`send_data`) goes
//! through the task, which owns the association, as bytes in, and SCTP packets the peer sends
//! come out of the incoming channel as bytes.
//!
//! RTP routing, transport-wide congestion control, and pacing belong to the RTP layer built on
//! this handle.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::BytesMut;
use log::{debug, info};
use tokio::sync::mpsc;

use crate::dtls::{Certificate, Dtls, DtlsEvent, DtlsParameters, DtlsRole, DtlsState};
use crate::ice::connection::{
    ConnectionEvent, IceConnection, IceParameters, SelectedPath, send_on,
};
use crate::ice::lite::LiteTimings;
use crate::ice::mux::inbound_channel;
use crate::ice::{Candidate, IceConfig, IceError, IceState, Inbound, Packet};
use crate::srtp::{SrtpContext, SrtpError, SrtpSession, is_rtcp};

/// Packets the incoming channel holds before the task drops rather than waits.
const INCOMING_CAPACITY: usize = 4096;
/// Commands the handle may queue for the task.
const COMMAND_CAPACITY: usize = 1024;
/// DTLS datagrams held until the association is started and a pair selected.
const DTLS_BACKLOG: usize = 64;

#[cfg(test)]
mod tests;

/// What a transport fails on.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Ice(#[from] IceError),
    #[error(transparent)]
    Dtls(#[from] crate::dtls::DtlsError),
    #[error(transparent)]
    Srtp(#[from] SrtpError),
    #[error("cannot send encrypted {0}, not connected")]
    NotConnected(&'static str),
    #[error("the transport task is gone")]
    Closed,
    #[error("the transport is busy")]
    Busy,
}

/// What a transport builds from.
pub struct TransportConfig {
    pub ice: IceConfig,
    pub certificate: Arc<Certificate>,
    pub lite_timings: LiteTimings,
}

/// What the peer sends: media and feedback unprotected, SCTP packets as bytes.
#[derive(Debug)]
pub enum Incoming {
    Rtp { data: BytesMut, arrived: Instant },
    Rtcp { data: BytesMut, arrived: Instant },
    Data(BytesMut),
}

/// What a transport reports.
#[derive(Clone, Debug)]
pub enum TransportEvent {
    IceState(IceState),
    DtlsState(DtlsState),
    SelectedPair {
        local: Candidate,
        remote: Candidate,
    },
    /// The task ended; nothing more comes.
    Closed,
}

/// The receiving ends a transport hands to its user.
pub struct TransportIo {
    pub incoming: mpsc::Receiver<Incoming>,
    pub events: mpsc::Receiver<TransportEvent>,
}

/// Packets and bytes sent and received, as `RTCTransportStats` reports them.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransportStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
}

enum Command {
    Start {
        ice: IceParameters,
        dtls: DtlsParameters,
        role: DtlsRole,
    },
    RemoteCandidate(Option<Candidate>),
    SendData(Vec<u8>),
    Close,
}

struct Shared {
    selected: Arc<Mutex<Option<SelectedPath>>>,
    tx_srtp: Mutex<Option<SrtpContext>>,
    ice_state: Mutex<IceState>,
    dtls_state: Mutex<DtlsState>,
    selected_pair: Mutex<Option<(Candidate, Candidate)>>,
    last_received: Mutex<Instant>,
    ice_parameters: IceParameters,
    local_candidates: Vec<Candidate>,
    certificate: Arc<Certificate>,
    controlling: bool,
    lite: bool,
    tx_packets: AtomicU64,
    tx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    rx_bytes: AtomicU64,
}

/// The handle on one peer's transport.
pub struct Transport {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
}

impl Transport {
    /// Build a transport: gather its candidates and start its task.
    ///
    /// # Errors
    /// The ICE configuration is refused, or no socket could be bound at all.
    pub async fn new(config: TransportConfig) -> Result<(Transport, TransportIo), TransportError> {
        let (inbound_tx, mut inbound_rx) = inbound_channel();
        let mut connection = IceConnection::new(config.ice, inbound_tx, config.lite_timings)?;
        let local_candidates = connection.gather(&mut inbound_rx).await?;
        let deferred = connection.take_deferred();
        let shared = Arc::new(Shared {
            selected: connection.selected_handle(),
            tx_srtp: Mutex::new(None),
            ice_state: Mutex::new(IceState::New),
            dtls_state: Mutex::new(DtlsState::New),
            selected_pair: Mutex::new(None),
            last_received: Mutex::new(Instant::now()),
            ice_parameters: connection.local_parameters(),
            local_candidates,
            certificate: config.certificate.clone(),
            controlling: connection.is_controlling(),
            lite: connection.is_lite(),
            tx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
        });
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(256);
        let task = Task {
            connection,
            dtls: Dtls::new(config.certificate),
            rx_srtp: None,
            shared: shared.clone(),
            incoming: incoming_tx,
            events: events_tx,
            dtls_backlog: Vec::new(),
            pending_dtls: None,
        };
        tokio::spawn(task.run(inbound_rx, commands_rx, deferred));
        Ok((
            Transport {
                shared,
                commands: commands_tx,
            },
            TransportIo {
                incoming: incoming_rx,
                events: events_rx,
            },
        ))
    }

    /// The local ICE parameters to signal.
    pub fn ice_parameters(&self) -> IceParameters {
        self.shared.ice_parameters.clone()
    }

    /// The local candidates gathered.
    pub fn local_candidates(&self) -> Vec<Candidate> {
        self.shared.local_candidates.clone()
    }

    /// The local DTLS parameters to signal: the certificate's fingerprints.
    pub fn dtls_parameters(&self) -> DtlsParameters {
        DtlsParameters {
            fingerprints: self.shared.certificate.fingerprints(),
            role: DtlsRole::Auto,
        }
    }

    /// Whether the ICE agent is ICE-lite.
    pub fn is_lite(&self) -> bool {
        self.shared.lite
    }

    /// The ICE role, `controlling` or `controlled`.
    pub fn ice_role(&self) -> &'static str {
        if self.shared.controlling {
            "controlling"
        } else {
            "controlled"
        }
    }

    /// Start ICE against `ice` and DTLS against `dtls` in `role`; `Auto` takes the server
    /// role when ICE is controlling and the client role otherwise.
    pub async fn start(
        &self,
        ice: IceParameters,
        dtls: DtlsParameters,
        role: DtlsRole,
    ) -> Result<(), TransportError> {
        self.commands
            .send(Command::Start { ice, dtls, role })
            .await
            .map_err(|_| TransportError::Closed)
    }

    /// Add a remote candidate, or `None` for end-of-candidates.
    pub async fn add_remote_candidate(
        &self,
        candidate: Option<Candidate>,
    ) -> Result<(), TransportError> {
        self.commands
            .send(Command::RemoteCandidate(candidate))
            .await
            .map_err(|_| TransportError::Closed)
    }

    /// Protect and send one RTP packet on the selected pair, returning the instant it went out.
    ///
    /// # Errors
    /// `NotConnected` before DTLS completed or a pair is selected.
    pub fn send_rtp(&self, packet: &[u8]) -> Result<Instant, TransportError> {
        let protected = {
            let mut guard = self.shared.tx_srtp.lock().unwrap();
            let srtp = guard.as_mut().ok_or(TransportError::NotConnected("RTP"))?;
            srtp.protect_rtp(packet)?
        };
        self.send_protected(&protected, "RTP")
    }

    /// Protect one RTP packet in its own buffer, where it lies, and send it on the selected pair.
    ///
    /// # Errors
    /// `NotConnected` before DTLS completed or a pair is selected.
    pub fn send_rtp_owned(&self, mut packet: BytesMut) -> Result<Instant, TransportError> {
        {
            let mut guard = self.shared.tx_srtp.lock().unwrap();
            let srtp = guard.as_mut().ok_or(TransportError::NotConnected("RTP"))?;
            srtp.protect_rtp_in_place(&mut packet)?;
        }
        self.send_protected(&packet, "RTP")
    }

    /// Protect and send one RTCP packet on the selected pair.
    ///
    /// # Errors
    /// `NotConnected` before DTLS completed or a pair is selected.
    pub fn send_rtcp(&self, packet: &[u8]) -> Result<Instant, TransportError> {
        let protected = {
            let mut guard = self.shared.tx_srtp.lock().unwrap();
            let srtp = guard.as_mut().ok_or(TransportError::NotConnected("RTCP"))?;
            srtp.protect_rtcp(packet)?
        };
        self.send_protected(&protected, "RTCP")
    }

    fn send_protected(&self, data: &[u8], what: &'static str) -> Result<Instant, TransportError> {
        let sent = send_on(&self.shared.selected, data).map_err(|e| match e {
            IceError::NotConnected => TransportError::NotConnected(what),
            other => TransportError::Ice(other),
        })?;
        self.shared.tx_packets.fetch_add(1, Ordering::Relaxed);
        self.shared
            .tx_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(sent)
    }

    /// Queue one SCTP packet for the DTLS association.
    ///
    /// # Errors
    /// `Busy` when the task's queue is full, `Closed` when it is gone.
    pub fn send_data(&self, data: &[u8]) -> Result<(), TransportError> {
        match self.commands.try_send(Command::SendData(data.to_vec())) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(TransportError::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TransportError::Closed),
        }
    }

    /// The pair media rides, once nominated.
    pub fn selected_pair(&self) -> Option<(Candidate, Candidate)> {
        self.shared.selected_pair.lock().unwrap().clone()
    }

    /// The ICE state.
    pub fn ice_state(&self) -> IceState {
        *self.shared.ice_state.lock().unwrap()
    }

    /// The DTLS state.
    pub fn dtls_state(&self) -> DtlsState {
        *self.shared.dtls_state.lock().unwrap()
    }

    /// When the peer was last heard from on the ICE path: media, DTLS, SCTP, or its checks.
    pub fn peer_heard_at(&self) -> Instant {
        *self.shared.last_received.lock().unwrap()
    }

    /// Packets and bytes carried so far.
    pub fn stats(&self) -> TransportStats {
        TransportStats {
            packets_sent: self.shared.tx_packets.load(Ordering::Relaxed),
            bytes_sent: self.shared.tx_bytes.load(Ordering::Relaxed),
            packets_received: self.shared.rx_packets.load(Ordering::Relaxed),
            bytes_received: self.shared.rx_bytes.load(Ordering::Relaxed),
        }
    }

    /// End the transport: DTLS sends `close_notify`, ICE releases its sockets.
    pub async fn close(&self) {
        let _ = self.commands.send(Command::Close).await;
    }
}

struct Task {
    connection: IceConnection,
    dtls: Dtls,
    rx_srtp: Option<SrtpContext>,
    shared: Arc<Shared>,
    incoming: mpsc::Sender<Incoming>,
    events: mpsc::Sender<TransportEvent>,
    dtls_backlog: Vec<BytesMut>,
    pending_dtls: Option<(bool, DtlsParameters)>,
}

impl Task {
    async fn run(
        mut self,
        mut inbound: mpsc::Receiver<Inbound>,
        mut commands: mpsc::Receiver<Command>,
        deferred: Vec<Inbound>,
    ) {
        for message in deferred {
            self.on_inbound(message);
        }
        self.after_step();
        loop {
            let deadline = [self.connection.poll_timeout(), self.dtls.poll_timeout()]
                .into_iter()
                .flatten()
                .min()
                .map(tokio::time::Instant::from_std);
            tokio::select! {
                message = inbound.recv() => match message {
                    Some(message) => self.on_inbound(message),
                    None => break,
                },
                command = commands.recv() => match command {
                    Some(Command::Start { ice, dtls, role }) => self.on_start(ice, dtls, role),
                    Some(Command::RemoteCandidate(candidate)) => {
                        self.connection.add_remote_candidate(Instant::now(), candidate);
                    }
                    Some(Command::SendData(data)) => {
                        if let Err(e) = self.dtls.write(Instant::now(), &data) {
                            debug!(target: "webrtc.dtls", "data not sent: {e}");
                        }
                    }
                    Some(Command::Close) | None => break,
                },
                _ = async { match deadline { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => {
                    let now = Instant::now();
                    self.connection.handle_timeout(now);
                    self.dtls.handle_timeout(now);
                }
            }
            self.after_step();
            if matches!(self.connection.state(), IceState::Failed | IceState::Closed)
                || matches!(self.dtls.state(), DtlsState::Failed | DtlsState::Closed)
            {
                break;
            }
        }
        self.shutdown();
    }

    fn on_start(&mut self, ice: IceParameters, dtls: DtlsParameters, role: DtlsRole) {
        let now = Instant::now();
        self.connection.set_remote_parameters(ice);
        if let Err(e) = self.connection.start(now) {
            info!(target: "webrtc", "{} ICE did not start: {e}", self.connection.name());
            return;
        }
        let is_client = match role {
            DtlsRole::Client => true,
            DtlsRole::Server => false,
            DtlsRole::Auto => !self.connection.is_controlling(),
        };
        debug!(
            target: "webrtc.dtls",
            "{} DTLS role {}",
            self.connection.name(),
            if is_client { "client" } else { "server" }
        );
        self.pending_dtls = Some((is_client, dtls));
    }

    /// Start the association once ICE has a pair to run it over.
    fn start_dtls_when_selected(&mut self) {
        if self.shared.selected.lock().unwrap().is_none() {
            return;
        }
        if let Some((is_client, remote)) = self.pending_dtls.take()
            && let Err(e) = self.dtls.start(Instant::now(), is_client, &remote)
        {
            info!(target: "webrtc", "{} DTLS did not start: {e}", self.connection.name());
        }
    }

    fn on_inbound(&mut self, message: Inbound) {
        let Some(packet) = self.connection.handle_inbound(message) else {
            return;
        };
        self.shared.rx_packets.fetch_add(1, Ordering::Relaxed);
        self.shared
            .rx_bytes
            .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
        match packet.data.first() {
            Some(20..=63) => self.on_dtls(packet.data),
            Some(128..=191) => self.on_srtp(packet),
            _ => {}
        }
    }

    fn on_dtls(&mut self, data: BytesMut) {
        if !self.dtls.started() {
            if self.dtls_backlog.len() < DTLS_BACKLOG {
                self.dtls_backlog.push(data);
            }
            return;
        }
        self.dtls.handle_datagram(Instant::now(), data);
    }

    fn on_srtp(&mut self, packet: Packet) {
        let Some(rx) = self.rx_srtp.as_mut() else {
            return;
        };
        let Packet { now, data, .. } = packet;
        let incoming = if is_rtcp(&data) {
            match rx.unprotect_rtcp(&data) {
                Ok(data) => Incoming::Rtcp { data, arrived: now },
                Err(e) => {
                    debug!(target: "webrtc.dtls", "{e}");
                    return;
                }
            }
        } else {
            match rx.unprotect_rtp(&data) {
                Ok(data) => Incoming::Rtp { data, arrived: now },
                Err(e) => {
                    debug!(target: "webrtc.dtls", "{e}");
                    return;
                }
            }
        };
        let _ = self.incoming.try_send(incoming);
    }

    /// After every input: feed DTLS what it was waiting for, send what the layers queued, and
    /// publish their events.
    fn after_step(&mut self) {
        let now = Instant::now();
        while let Some(event) = self.connection.poll_event() {
            match event {
                ConnectionEvent::State(state) => {
                    *self.shared.ice_state.lock().unwrap() = state;
                    let _ = self.events.try_send(TransportEvent::IceState(state));
                }
                ConnectionEvent::SelectedPair { local, remote } => {
                    *self.shared.selected_pair.lock().unwrap() =
                        Some((local.clone(), remote.clone()));
                    let _ = self
                        .events
                        .try_send(TransportEvent::SelectedPair { local, remote });
                }
            }
        }
        *self.shared.last_received.lock().unwrap() = self.connection.last_received();
        self.start_dtls_when_selected();
        if self.dtls.started() && !self.dtls_backlog.is_empty() {
            for data in std::mem::take(&mut self.dtls_backlog) {
                self.dtls.handle_datagram(now, data);
            }
        }
        while let Some(event) = self.dtls.poll_event() {
            match event {
                DtlsEvent::Connected {
                    profile,
                    keying_material,
                    is_client,
                } => {
                    match SrtpSession::from_keying_material(profile, &keying_material, is_client) {
                        Ok(session) => {
                            info!(
                                target: "webrtc",
                                "{} DTLS connected as {} with {}",
                                self.connection.name(),
                                if is_client { "client" } else { "server" },
                                profile.name()
                            );
                            self.rx_srtp = Some(session.rx);
                            *self.shared.tx_srtp.lock().unwrap() = Some(session.tx);
                            self.publish_dtls_state();
                        }
                        Err(e) => {
                            info!(target: "webrtc", "{} SRTP keying failed: {e}", self.connection.name());
                            *self.shared.dtls_state.lock().unwrap() = DtlsState::Failed;
                            let _ = self
                                .events
                                .try_send(TransportEvent::DtlsState(DtlsState::Failed));
                        }
                    }
                }
                DtlsEvent::Data(data) => {
                    let _ = self.incoming.try_send(Incoming::Data(data));
                }
                DtlsEvent::Closed => self.publish_dtls_state(),
                DtlsEvent::Failed(reason) => {
                    info!(target: "webrtc", "{} DTLS handshake failed ({reason})", self.connection.name());
                    self.publish_dtls_state();
                }
            }
        }
        while let Some(data) = self.dtls.poll_transmit() {
            match self.connection.send(&data) {
                Ok(_) => {
                    self.shared.tx_packets.fetch_add(1, Ordering::Relaxed);
                    self.shared
                        .tx_bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                Err(e) => debug!(target: "webrtc.dtls", "DTLS record not sent: {e}"),
            }
        }
        if self.dtls.state() != *self.shared.dtls_state.lock().unwrap() {
            self.publish_dtls_state();
        }
    }

    fn publish_dtls_state(&mut self) {
        let state = self.dtls.state();
        let previous = std::mem::replace(&mut *self.shared.dtls_state.lock().unwrap(), state);
        if previous != state {
            let _ = self.events.try_send(TransportEvent::DtlsState(state));
        }
    }

    fn shutdown(mut self) {
        let now = Instant::now();
        self.dtls.close(now);
        while let Some(data) = self.dtls.poll_transmit() {
            let _ = self.connection.send(&data);
        }
        *self.shared.tx_srtp.lock().unwrap() = None;
        self.connection.close();
        self.after_step();
        *self.shared.ice_state.lock().unwrap() = IceState::Closed;
        let _ = self.events.try_send(TransportEvent::Closed);
    }
}
