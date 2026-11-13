/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A page on the data WebSocket, speaking the wire protocol by hand.
//!
//! The server streams to an attached display for as long as the session lives, and a client that
//! stops reading stops answering pings with it, so the connection would die mid-test rather than
//! where the test is looking. One task therefore owns the socket from the handshake on: it reads
//! everything, keeps every text message and the head of every binary one with its arrival time,
//! and sends what the test hands it. The test waits on what was kept.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_websockets::{ClientBuilder, Limits, Message};

use crate::Server;

/// How long the server is given to accept the socket and send its first message.
const HELLO: Duration = Duration::from_secs(10);

/// A binary message as it arrived: when, its first four bytes, and its length.
#[derive(Clone, Copy)]
pub struct Binary {
    pub at: Instant,
    pub head: [u8; 4],
    pub len: usize,
}

impl Binary {
    /// The frame id a video message carries, `None` for any other binary message.
    pub fn video_frame_id(&self) -> Option<u16> {
        (self.len >= 4 && matches!(self.head[0], 0x03 | 0x04))
            .then(|| u16::from_be_bytes([self.head[2], self.head[3]]))
    }
}

/// What the socket delivered so far.
#[derive(Default)]
struct Inbox {
    texts: Vec<String>,
    binaries: Vec<Binary>,
    closed: bool,
}

/// A connected page.
pub struct Client {
    /// What the pump sends next; `None` closes the socket.
    outgoing: mpsc::UnboundedSender<Option<Message>>,
    inbox: Arc<Mutex<Inbox>>,
    arrived: Arc<Notify>,
    pump: JoinHandle<()>,
}

impl Client {
    /// Connect to `server`'s data WebSocket with `query` (`?role=viewer`, or nothing) and wait
    /// for the server's first message.
    ///
    /// # Panics
    ///
    /// When the socket is refused or the server sends nothing within ten seconds; the message
    /// carries the server's log.
    pub async fn connect(server: &Server, query: &str) -> Client {
        let uri = server.websocket(query);
        server.page_turn().await;
        let connecting = async {
            ClientBuilder::new()
                .uri(&uri)
                .expect("a WebSocket address")
                .limits(Limits::unlimited())
                .connect()
                .await
        };
        let mut socket = match tokio::time::timeout(HELLO, connecting).await {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(e)) => panic!("{uri} refused the socket: {e}\n{}", server.log_tail()),
            Err(_) => panic!(
                "{uri} did not accept the socket within {HELLO:?}\n{}",
                server.log_tail()
            ),
        };
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let arrived = Arc::new(Notify::new());
        let (outgoing, mut queued) = mpsc::unbounded_channel::<Option<Message>>();
        let pump = tokio::spawn({
            let inbox = Arc::clone(&inbox);
            let arrived = Arc::clone(&arrived);
            async move {
                loop {
                    tokio::select! {
                        message = socket.next() => {
                            let mut inbox = inbox.lock().unwrap_or_else(PoisonError::into_inner);
                            match message {
                                Some(Ok(message)) => {
                                    if let Some(text) = message.as_text() {
                                        inbox.texts.push(text.to_owned());
                                    } else if message.is_binary() {
                                        let payload = message.as_payload();
                                        let mut head = [0u8; 4];
                                        let taken = payload.len().min(4);
                                        head[..taken].copy_from_slice(&payload[..taken]);
                                        inbox.binaries.push(Binary {
                                            at: Instant::now(),
                                            head,
                                            len: payload.len(),
                                        });
                                    }
                                }
                                _ => inbox.closed = true,
                            }
                            let closed = inbox.closed;
                            drop(inbox);
                            arrived.notify_waiters();
                            if closed {
                                return;
                            }
                        }
                        message = queued.recv() => {
                            let Some(Some(message)) = message else {
                                let _ = socket.close().await;
                                inbox.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
                                arrived.notify_waiters();
                                return;
                            };
                            if socket.send(message).await.is_err() {
                                inbox.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
                                arrived.notify_waiters();
                                return;
                            }
                        }
                    }
                }
            }
        });
        let client = Client {
            outgoing,
            inbox,
            arrived,
            pump,
        };
        assert!(
            client.text(0, HELLO, |_| true).await.is_some(),
            "{uri} sent nothing within {HELLO:?}\n{}",
            server.log_tail()
        );
        client
    }

    /// Send the text message `verb`. A socket the server closed takes nothing; the checks that
    /// follow say so.
    pub fn send(&self, verb: &str) {
        let _ = self.outgoing.send(Some(Message::text(verb.to_owned())));
    }

    /// How many text messages have arrived: a mark for [`Client::text`].
    pub fn texts(&self) -> usize {
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .texts
            .len()
    }

    /// The server's first message, which names the transport (`MODE websockets`).
    pub fn hello(&self) -> String {
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .texts
            .first()
            .cloned()
            .unwrap_or_default()
    }

    /// The first text message from index `mark` on that `wanted` accepts, waiting up to `timeout`
    /// for it; `None` when none arrived.
    pub async fn text(
        &self,
        mark: usize,
        timeout: Duration,
        wanted: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let arrival = self.arrived.notified();
            tokio::pin!(arrival);
            // Registered before the look, so a message landing between the two still wakes it.
            arrival.as_mut().enable();
            {
                let inbox = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(found) = inbox.texts.iter().skip(mark).find(|t| wanted(t)) {
                    return Some(found.clone());
                }
                if inbox.closed {
                    return None;
                }
            }
            if tokio::time::timeout_at(deadline, arrival).await.is_err() {
                return None;
            }
        }
    }

    /// The value the connect-time settings payload publishes for the setting `name`, `None` when
    /// no payload arrived within ten seconds or it carries no such setting.
    pub async fn published_setting(&self, name: &str) -> Option<serde_json::Value> {
        let payload = self
            .text(0, HELLO, |text| {
                text.starts_with('{') && text.contains("\"server_settings\"")
            })
            .await?;
        let payload: serde_json::Value = serde_json::from_str(&payload).ok()?;
        payload.get("settings")?.get(name)?.get("value").cloned()
    }

    /// Every text message from index `mark` on that `wanted` accepts, read for the whole of
    /// `window`.
    pub async fn texts_during(
        &self,
        mark: usize,
        window: Duration,
        wanted: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        tokio::time::sleep(window).await;
        let inbox = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        inbox
            .texts
            .iter()
            .skip(mark)
            .filter(|t| wanted(t))
            .cloned()
            .collect()
    }

    /// The binary messages that have arrived from index `mark` on.
    pub fn binaries(&self, mark: usize) -> Vec<Binary> {
        let inbox = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        inbox.binaries.iter().skip(mark).copied().collect()
    }

    /// Close the socket and wait until the close went out.
    pub async fn close(mut self) {
        let _ = self.outgoing.send(None);
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.pump).await;
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.pump.abort();
    }
}
