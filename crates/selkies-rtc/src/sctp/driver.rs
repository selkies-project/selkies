/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The tokio task that drives an association over a DTLS datagram path.
//!
//! `spawn` owns the `Sctp` on one task: datagrams from the DTLS transport, the engine's
//! commands, and the association's timers are the three things it waits on, and after each
//! it sends what the association produced through `DtlsDatagrams` and hands its events to
//! the engine. `SctpTransport` and `DataChannel` are the handles the engine holds; every
//! call on them is a message to the task, so nothing locks across an await and the
//! association never sees two callers at once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rtc_sctp::Side;
use tokio::sync::{mpsc, oneshot};

use super::association::{OpenError, Sctp, SctpCapabilities, SctpEvent, SendError};
use super::channel::{DataChannelParameters, Message};

/// The path SCTP packets take to the wire: the DTLS transport's application data.
///
/// The transport integrator implements it over the DTLS record layer; a send that fails is
/// a lost packet SCTP's own retransmission repairs, so it returns nothing.
pub trait DtlsDatagrams: Send + Sync + 'static {
    /// Hand one SCTP packet to the DTLS transport.
    fn send(&self, packet: &[u8]);
}

/// Raw bytes per bulk (clipboard, file) chunk before base64 expansion. A chunk is the unit at
/// which input and the media streams overtake a transfer, so it is sized for latency, and a
/// multiple of 3 so chunks concatenate as base64.
pub const DATA_CHANNEL_BULK_CHUNK_SIZE: usize = 16 * 1024 / 3 * 3;

/// The SCTP queue cap a bulk transfer waits below, bounding what it holds away from input
/// and the media streams.
pub const DATA_CHANNEL_BULK_HIGH_WATER: usize = 64 * 1024;

/// The raw-byte chunk size for base64 payloads, `DATA_CHANNEL_BULK_CHUNK_SIZE` lowered to
/// what the connected peers negotiated (`a=max-message-size`) where one of them advertises
/// less, so a peer is never overrun.
pub fn adjusted_chunk_size(remote_max_message_sizes: impl IntoIterator<Item = usize>) -> usize {
    let limit = remote_max_message_sizes
        .into_iter()
        .filter(|size| *size > 0)
        .min();
    match limit {
        Some(limit) => {
            let usable = limit.saturating_sub(512);
            (usable * 3 / 4).min(DATA_CHANNEL_BULK_CHUNK_SIZE)
        }
        None => DATA_CHANNEL_BULK_CHUNK_SIZE,
    }
}

enum Command {
    Packet(Bytes),
    Open(
        DataChannelParameters,
        oneshot::Sender<Result<u16, OpenError>>,
    ),
    Send(u16, Message, oneshot::Sender<Result<(), SendError>>),
    Close(u16),
    BufferedAmount(u16, oneshot::Sender<usize>),
    LowThreshold(u16, usize),
    Drain(u16, usize, oneshot::Sender<bool>),
    Stop,
}

/// The engine's handle on a running association.
#[derive(Clone)]
pub struct SctpTransport {
    commands: mpsc::UnboundedSender<Command>,
    remote_max_message_size: usize,
}

/// One data channel on a running association.
#[derive(Clone)]
pub struct DataChannel {
    id: u16,
    label: String,
    ordered: bool,
    transport: SctpTransport,
}

impl SctpTransport {
    /// Start an association on its own task.
    ///
    /// `incoming` carries the SCTP packets the DTLS transport decrypted; `datagrams` takes
    /// those the association sends. The events the association raises arrive on the
    /// returned receiver until `SctpEvent::Closed`.
    pub fn spawn(
        side: Side,
        remote_caps: Option<SctpCapabilities>,
        datagrams: Arc<dyn DtlsDatagrams>,
        mut incoming: mpsc::Receiver<Bytes>,
    ) -> (SctpTransport, mpsc::UnboundedReceiver<SctpEvent>) {
        let (commands, mut command_rx) = mpsc::unbounded_channel::<Command>();
        let (events, event_rx) = mpsc::unbounded_channel::<SctpEvent>();
        let now = Instant::now();
        let mut sctp = Sctp::new(side, remote_caps, now);
        let remote_max_message_size = sctp.remote_max_message_size();
        tokio::spawn(async move {
            let mut closed = false;
            let mut drains: Vec<(u16, usize, oneshot::Sender<bool>)> = Vec::new();
            loop {
                let now = Instant::now();
                while let Some(packet) = sctp.poll_transmit(now) {
                    datagrams.send(&packet);
                }
                while let Some(event) = sctp.poll_event() {
                    if matches!(event, SctpEvent::Closed) {
                        closed = true;
                    }
                    if events.send(event).is_err() {
                        closed = true;
                    }
                }
                if closed {
                    return;
                }
                drains.retain_mut(|(id, high, reply)| {
                    if reply.is_closed() {
                        return false;
                    }
                    if sctp.buffered_amount(*id) > *high {
                        return true;
                    }
                    let (done, _) = oneshot::channel();
                    let _ = std::mem::replace(reply, done).send(true);
                    false
                });
                let deadline = sctp
                    .poll_timeout()
                    .unwrap_or_else(|| now + Duration::from_secs(3600));
                tokio::select! {
                    biased;
                    command = command_rx.recv() => match command {
                        Some(Command::Packet(packet)) => sctp.handle_packet(packet, Instant::now()),
                        Some(Command::Open(params, reply)) => {
                            let _ = reply.send(sctp.open_channel(params, Instant::now()));
                        }
                        Some(Command::Send(id, message, reply)) => {
                            let _ = reply.send(sctp.send(id, message, Instant::now()));
                        }
                        Some(Command::Close(id)) => sctp.close_channel(id, Instant::now()),
                        Some(Command::BufferedAmount(id, reply)) => {
                            let _ = reply.send(sctp.buffered_amount(id));
                        }
                        Some(Command::LowThreshold(id, threshold)) => {
                            sctp.set_buffered_amount_low_threshold(id, threshold);
                        }
                        Some(Command::Drain(id, high, reply)) => drains.push((id, high, reply)),
                        Some(Command::Stop) | None => sctp.stop(Instant::now()),
                    },
                    packet = incoming.recv() => match packet {
                        Some(packet) => sctp.handle_packet(packet, Instant::now()),
                        None => sctp.stop(Instant::now()),
                    },
                    _ = tokio::time::sleep_until(deadline.into()) => {
                        sctp.handle_timeout(Instant::now());
                    }
                }
            }
        });
        (
            SctpTransport {
                commands,
                remote_max_message_size,
            },
            event_rx,
        )
    }

    /// Hand the association one decrypted SCTP packet, for an integrator without a channel.
    pub fn receive(&self, packet: Bytes) {
        let _ = self.commands.send(Command::Packet(packet));
    }

    /// The largest message the peer receives, in bytes.
    pub fn remote_max_message_size(&self) -> usize {
        self.remote_max_message_size
    }

    /// Open a data channel.
    pub async fn open_channel(
        &self,
        params: DataChannelParameters,
    ) -> Result<DataChannel, OpenError> {
        let (reply, answer) = oneshot::channel();
        let label = params.label.clone();
        let ordered = params.ordered;
        self.commands
            .send(Command::Open(params, reply))
            .map_err(|_| OpenError::Closed)?;
        let id = answer.await.map_err(|_| OpenError::Closed)??;
        Ok(DataChannel {
            id,
            label,
            ordered,
            transport: self.clone(),
        })
    }

    /// A handle on a channel the peer opened (`SctpEvent::ChannelOpened`).
    pub fn channel(&self, id: u16, params: &DataChannelParameters) -> DataChannel {
        DataChannel {
            id,
            label: params.label.clone(),
            ordered: params.ordered,
            transport: self.clone(),
        }
    }

    /// Abort the association.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
    }
}

impl DataChannel {
    /// The channel's stream id.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// The channel's label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the channel delivers in order.
    pub fn ordered(&self) -> bool {
        self.ordered
    }

    /// Send a string message.
    pub async fn send_text(&self, text: impl Into<String>) -> Result<(), SendError> {
        self.send(Message::Text(text.into())).await
    }

    /// Send a binary message.
    pub async fn send_binary(&self, bytes: impl Into<Bytes>) -> Result<(), SendError> {
        self.send(Message::Binary(bytes.into())).await
    }

    /// Send a message.
    pub async fn send(&self, message: Message) -> Result<(), SendError> {
        let (reply, answer) = oneshot::channel();
        self.transport
            .commands
            .send(Command::Send(self.id, message, reply))
            .map_err(|_| SendError::Transport("the SCTP transport is closed".into()))?;
        answer
            .await
            .map_err(|_| SendError::Transport("the SCTP transport is closed".into()))?
    }

    /// Bytes queued for the peer and not yet acknowledged.
    pub async fn buffered_amount(&self) -> usize {
        let (reply, answer) = oneshot::channel();
        if self
            .transport
            .commands
            .send(Command::BufferedAmount(self.id, reply))
            .is_err()
        {
            return 0;
        }
        answer.await.unwrap_or(0)
    }

    /// Set the buffered amount at which `SctpEvent::BufferedAmountLow` fires.
    pub fn set_buffered_amount_low_threshold(&self, threshold: usize) {
        let _ = self
            .transport
            .commands
            .send(Command::LowThreshold(self.id, threshold));
    }

    /// Wait until the channel has at most `high` bytes queued, for a bulk transfer's pacing.
    /// The timeout keeps a stalled or closing channel from wedging the sender.
    ///
    /// Returns whether it drained: `false` when it did not within `timeout`, still stalled.
    pub async fn drain(&self, high: usize, timeout: Duration) -> bool {
        let (reply, answer) = oneshot::channel();
        if self
            .transport
            .commands
            .send(Command::Drain(self.id, high, reply))
            .is_err()
        {
            return false;
        }
        matches!(tokio::time::timeout(timeout, answer).await, Ok(Ok(true)))
    }

    /// Close the channel.
    pub fn close(&self) {
        let _ = self.transport.commands.send(Command::Close(self.id));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn chunk_size_follows_the_smallest_peer() {
        assert_eq!(adjusted_chunk_size([]), DATA_CHANNEL_BULK_CHUNK_SIZE);
        assert_eq!(adjusted_chunk_size([0]), DATA_CHANNEL_BULK_CHUNK_SIZE);
        assert_eq!(adjusted_chunk_size([262_144]), DATA_CHANNEL_BULK_CHUNK_SIZE);
        assert_eq!(
            adjusted_chunk_size([65_536, 262_144]),
            DATA_CHANNEL_BULK_CHUNK_SIZE
        );
        assert_eq!(adjusted_chunk_size([16_384]), (16_384 - 512) * 3 / 4);
    }

    /// Packets from one transport go to the other's incoming channel.
    struct Wire(mpsc::Sender<Bytes>, Arc<AtomicBool>);

    impl DtlsDatagrams for Wire {
        fn send(&self, packet: &[u8]) {
            if !self.1.load(Ordering::SeqCst) {
                let _ = self.0.try_send(Bytes::copy_from_slice(packet));
            }
        }
    }

    #[tokio::test]
    async fn two_transports_over_a_datagram_path_open_channels_and_drain() {
        let lost = Arc::new(AtomicBool::new(false));
        let (to_server, server_in) = mpsc::channel(4096);
        let (to_client, client_in) = mpsc::channel(4096);
        let (client, mut client_events) = SctpTransport::spawn(
            Side::Client,
            None,
            Arc::new(Wire(to_server, Arc::clone(&lost))),
            client_in,
        );
        let (server, mut server_events) = SctpTransport::spawn(
            Side::Server,
            None,
            Arc::new(Wire(to_client, Arc::clone(&lost))),
            server_in,
        );
        let input = server
            .open_channel(DataChannelParameters::new("input", true))
            .await
            .expect("opens");
        assert_eq!(
            (input.id(), input.label(), input.ordered()),
            (0, "input", true)
        );
        let params = loop {
            match client_events.recv().await {
                Some(SctpEvent::ChannelOpened(0, params)) => break params,
                Some(_) => {}
                None => panic!("the client's transport ended"),
            }
        };
        assert_eq!(params.label, "input");
        loop {
            match server_events.recv().await {
                Some(SctpEvent::ChannelState(0, super::super::ChannelState::Open)) => break,
                Some(_) => {}
                None => panic!("the server's transport ended"),
            }
        }
        input.send_text("hello").await.expect("sent");
        let peer_channel = client.channel(0, &params);
        peer_channel.send_binary(vec![1u8, 2]).await.expect("sent");
        let mut heard = 0;
        while heard < 1 {
            match client_events.recv().await {
                Some(SctpEvent::Message(0, Message::Text(text))) => {
                    assert_eq!(text, "hello");
                    heard += 1;
                }
                Some(_) => {}
                None => panic!("the client's transport ended"),
            }
        }
        // The path goes silent, so past the congestion window a large message waits, and the
        // drain says so; the path back, the retransmission timer moves it out.
        lost.store(true, Ordering::SeqCst);
        input
            .send_binary(vec![0u8; 256 * 1024])
            .await
            .expect("sent");
        assert!(input.buffered_amount().await > DATA_CHANNEL_BULK_HIGH_WATER);
        assert!(
            !input
                .drain(DATA_CHANNEL_BULK_HIGH_WATER, Duration::from_millis(100))
                .await
        );
        lost.store(false, Ordering::SeqCst);
        assert!(
            input
                .drain(DATA_CHANNEL_BULK_HIGH_WATER, Duration::from_secs(10))
                .await
        );
        assert!(input.buffered_amount().await <= DATA_CHANNEL_BULK_HIGH_WATER);
        let too_large = input.send_binary(vec![0u8; 262_145]).await;
        assert!(matches!(too_large, Err(SendError::TooLarge { .. })));
        server.stop();
        loop {
            match server_events.recv().await {
                Some(SctpEvent::Closed) | None => break,
                Some(_) => {}
            }
        }
        loop {
            match client_events.recv().await {
                Some(SctpEvent::Closed) | None => break,
                Some(_) => {}
            }
        }
        assert!(input.send_text("late").await.is_err());
    }
}
