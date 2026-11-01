/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The shared Opus audio fan-out.
//!
//! The audio engine (`crate::audio::AudioEngine`) owns the capture, its Opus+RED gate, and its
//! health; its frames arrive on a broadcast channel, each one's bytes shared with pcmflux's
//! buffer. One task reads them and queues each, header and all and never copied, for every
//! client the stream serves: every socket but the secondary displays' owners and the pages
//! whose audio is off (`WsService::audio_targets`). Every socket's frame is queued before any
//! send is awaited, each send then waiting on its own, so a socket's frames leave in order and
//! a slow or dead socket holds neither the shared stream nor another client's audio; a socket
//! that stops draining is dropped by `Client::send_live`'s rule. A surround capture's stereo
//! companion frames are WebRTC's and never go on this wire. A subscriber that falls behind
//! loses the oldest frames: the channel's depth is the bound.

use std::sync::Arc;

use log::debug;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::audio::AudioPacket;
use crate::ws::client::{Client, ClientId, Frame};

/// Who a frame goes to, and who leaves the registry for a dead socket.
pub trait AudioTargets: Send + Sync + 'static {
    /// The sockets the stream serves right now.
    fn audio_targets(&self) -> Vec<Arc<Client>>;
    /// A socket died under a send.
    fn audio_socket_died(&self, client: ClientId);
}

/// Fan `rx`'s frames out until the channel closes.
pub async fn run_fanout<T: AudioTargets>(host: Arc<T>, mut rx: broadcast::Receiver<AudioPacket>) {
    loop {
        let packet = match rx.recv().await {
            Ok(packet) => packet,
            Err(RecvError::Lagged(missed)) => {
                debug!(target: "ws", "Audio fan-out fell {missed} frames behind.");
                continue;
            }
            Err(RecvError::Closed) => return,
        };
        if packet.companion || packet.data.is_empty() {
            continue;
        }
        for client in host.audio_targets() {
            if client.closed() {
                host.audio_socket_died(client.id);
                continue;
            }
            let Ok(wait) = client.send_live_later(Frame::Binary(packet.data.clone()), "Audio")
            else {
                host.audio_socket_died(client.id);
                continue;
            };
            let (host, id) = (host.clone(), client.id);
            tokio::spawn(async move {
                if wait.await.is_err() {
                    host.audio_socket_died(id);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::client::Liveness;
    use crate::ws::client::testing::FakeSocket;
    use crate::ws::now;
    use bytes::Bytes;
    use parking_lot::Mutex;
    use std::time::Duration;

    struct Host {
        clients: Mutex<Vec<Arc<Client>>>,
    }

    impl AudioTargets for Host {
        fn audio_targets(&self) -> Vec<Arc<Client>> {
            self.clients.lock().clone()
        }
        fn audio_socket_died(&self, client: ClientId) {
            self.clients.lock().retain(|c| c.id != client);
        }
    }

    struct Run {
        chunks: usize,
        fast_got: usize,
        fast_late_max: f64,
        slow_kept: bool,
        slow_got: usize,
        slow_in_order: bool,
    }

    /// Twenty-millisecond chunks to a fast client and `slow` through the shared fan-out, then up
    /// to `drain_s` for what the slow socket's writer holds in order.
    async fn audio_run(slow: FakeSocket, liveness: Liveness, seconds: f64, drain_s: f64) -> Run {
        let fast = FakeSocket::new(1000, 10.0 * 1024.0 * 1024.0, 0, liveness);
        let host = Arc::new(Host {
            clients: Mutex::new(vec![fast.client.clone(), slow.client.clone()]),
        });
        let (tx, rx) = broadcast::channel(256);
        let task = tokio::spawn(run_fanout(host.clone(), rx));
        let start = now();
        let mut enqueued = Vec::new();
        let mut n = 0u16;
        while now() - start < seconds {
            let mut data = vec![0x01, 0, (n >> 8) as u8, n as u8];
            data.resize(1004, 0);
            enqueued.push(now());
            let _ = tx.send(AudioPacket {
                data: Bytes::from(data),
                pts: 0,
                rtp_pts: 0,
                channels: 2,
                companion: false,
            });
            n += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let deadline = now() + drain_s;
        while now() < deadline && slow.writes.lock().len() < n as usize {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let seq = |head: &[u8]| ((head[2] as usize) << 8) | head[3] as usize;
        let lateness: Vec<f64> = fast
            .writes
            .lock()
            .iter()
            .map(|(t, head, _)| t - enqueued[seq(head)])
            .collect();
        let order: Vec<usize> = slow
            .writes
            .lock()
            .iter()
            .map(|(_, head, _)| seq(head))
            .collect();
        let run = Run {
            chunks: n as usize,
            fast_got: lateness.len(),
            fast_late_max: lateness.iter().copied().fold(0.0, f64::max),
            slow_kept: host.clients.lock().iter().any(|c| c.id == slow.client.id),
            slow_got: order.len(),
            slow_in_order: order.windows(2).all(|w| w[1] > w[0]),
        };
        task.abort();
        fast.stop();
        slow.stop();
        run
    }

    #[tokio::test]
    async fn a_slow_clients_audio_is_kept_in_order_and_holds_no_one_else() {
        let liveness = Liveness::default();
        let slow = FakeSocket::new(1, 150.0 * 1024.0, 600 * 1024, liveness);
        let a = audio_run(slow, liveness, 3.0, 3.0).await;
        assert!(
            a.slow_kept,
            "a slow client's audio is never dropped for its drain"
        );
        assert!(
            a.slow_in_order && a.slow_got >= a.chunks - 2,
            "{}/{}",
            a.slow_got,
            a.chunks
        );
        assert!(
            a.fast_got >= a.chunks - 2 && a.fast_late_max < 0.1,
            "latest {} s",
            a.fast_late_max
        );
    }

    #[tokio::test]
    async fn a_dead_client_is_dropped_while_the_other_keeps_its_cadence() {
        let liveness = Liveness {
            stall_s: 1.5,
            probe_s: 0.1,
        };
        let dead = FakeSocket::new(1, 0.0, 600 * 1024, liveness);
        let a = audio_run(dead, liveness, 3.5, 0.0).await;
        assert!(
            !a.slow_kept,
            "a dead client is dropped from the audio fan-out"
        );
        assert!(
            a.fast_got >= a.chunks - 2 && a.fast_late_max < 0.1,
            "latest {} s",
            a.fast_late_max
        );
    }

    #[tokio::test]
    async fn companion_frames_stay_off_this_wire() {
        let page = crate::ws::client::testing::Page::new(9, "controller");
        let host = Arc::new(Host {
            clients: Mutex::new(vec![page.client.clone()]),
        });
        let (tx, rx) = broadcast::channel(8);
        let task = tokio::spawn(run_fanout(host, rx));
        let frame = |companion| AudioPacket {
            data: Bytes::from_static(&[1, 0, 9]),
            pts: 0,
            rtp_pts: 0,
            channels: 2,
            companion,
        };
        tx.send(frame(true)).unwrap();
        tx.send(frame(false)).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(page.sent.lock().len(), 1);
        task.abort();
    }
}
