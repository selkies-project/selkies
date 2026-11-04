/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Fans one source out to the peers of a display, reading it only for consumers that wait.
//!
//! The relay reads the source only once every consumer is waiting for its next frame, so a consumer
//! that needs longer per frame than the source takes leaves the frames it cannot keep up with in
//! the source, whose own policy drops them (the bridges keep the newest frame and tell the encoder
//! what they dropped), instead of queueing all of them behind itself: its latency stays one frame's
//! handling rather than growing for as long as it lags. Every consumer is handed the same frames,
//! and one that stops holds no one back. A consumer counts from its first `recv`, so a peer still
//! connecting holds nothing up.
//!
//! The worker is one task per source and hands a frame on in a lock section, so it never waits on
//! a consumer's socket.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// What a relay reads its frames from.
pub trait RelaySource<T>: Send + Sync + 'static {
    /// The next frame.
    fn next(&self) -> Pin<Box<dyn Future<Output = T> + Send + '_>>;
}

struct Consumer<T> {
    queue: VecDeque<T>,
    waiting: bool,
    wake: Arc<Notify>,
}

struct Shared<T> {
    consumers: HashMap<u64, Consumer<T>>,
    next_id: u64,
    stopped: bool,
}

impl<T> Shared<T> {
    fn all_idle(&self) -> bool {
        !self.consumers.is_empty()
            && self
                .consumers
                .values()
                .all(|c| c.waiting && c.queue.is_empty())
    }
}

/// One source's fan-out.
pub struct MediaRelay<T> {
    shared: Arc<Mutex<Shared<T>>>,
    wanted: Arc<Notify>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// A consumer of a relay. Dropping it, or `stop`, takes it out.
pub struct RelayTrack<T> {
    id: u64,
    shared: Arc<Mutex<Shared<T>>>,
    wanted: Arc<Notify>,
    wake: Arc<Notify>,
    registered: bool,
}

impl<T: Clone + Send + 'static> MediaRelay<T> {
    /// A relay reading `source` on a task of its own, once consumers ask.
    pub fn new(source: Arc<dyn RelaySource<T>>) -> Arc<Self> {
        let shared = Arc::new(Mutex::new(Shared {
            consumers: HashMap::new(),
            next_id: 0,
            stopped: false,
        }));
        let wanted = Arc::new(Notify::new());
        let relay = Arc::new(Self {
            shared: shared.clone(),
            wanted: wanted.clone(),
            worker: Mutex::new(None),
        });
        let worker = tokio::spawn(async move {
            loop {
                loop {
                    let notified = wanted.notified();
                    {
                        let s = shared.lock();
                        if s.stopped {
                            return;
                        }
                        if s.all_idle() {
                            break;
                        }
                    }
                    notified.await;
                }
                let frame = source.next().await;
                let mut s = shared.lock();
                if s.stopped {
                    return;
                }
                for consumer in s.consumers.values_mut() {
                    consumer.queue.push_back(frame.clone());
                    consumer.wake.notify_one();
                }
            }
        });
        *relay.worker.lock() = Some(worker);
        relay
    }

    /// A new consumer.
    pub fn subscribe(&self) -> RelayTrack<T> {
        let mut s = self.shared.lock();
        let id = s.next_id;
        s.next_id += 1;
        RelayTrack {
            id,
            shared: self.shared.clone(),
            wanted: self.wanted.clone(),
            wake: Arc::new(Notify::new()),
            registered: false,
        }
    }

    /// End the worker; consumers wait on nothing from here on.
    pub fn stop(&self) {
        self.shared.lock().stopped = true;
        self.wanted.notify_one();
        if let Some(worker) = self.worker.lock().take() {
            worker.abort();
        }
    }
}

impl<T> Drop for MediaRelay<T> {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.lock().take() {
            worker.abort();
        }
    }
}

/// Clears a consumer's waiting mark when its `recv` is dropped, so a select that took another
/// branch does not leave it counted as idle.
struct Waiting<'a, T> {
    track: &'a RelayTrack<T>,
}

impl<T> Drop for Waiting<'_, T> {
    fn drop(&mut self) {
        if let Some(consumer) = self.track.shared.lock().consumers.get_mut(&self.track.id) {
            consumer.waiting = false;
        }
    }
}

impl<T> RelayTrack<T> {
    /// The next frame. Cancel safe: a frame handed on while no `recv` is pending waits in this
    /// consumer's queue.
    pub async fn recv(&mut self) -> T {
        if !self.registered {
            self.registered = true;
            self.shared.lock().consumers.insert(
                self.id,
                Consumer {
                    queue: VecDeque::new(),
                    waiting: false,
                    wake: self.wake.clone(),
                },
            );
        }
        let this = &*self;
        let _waiting = Waiting { track: this };
        loop {
            let notified = this.wake.notified();
            let (frame, idle) = {
                let mut s = this.shared.lock();
                let frame = s.consumers.get_mut(&this.id).and_then(|c| {
                    let frame = c.queue.pop_front();
                    c.waiting = frame.is_none();
                    frame
                });
                (frame, s.all_idle())
            };
            if let Some(frame) = frame {
                return frame;
            }
            if idle {
                this.wanted.notify_one();
            }
            notified.await;
        }
    }

    /// Stop consuming: the relay no longer waits for this consumer.
    pub fn stop(&mut self) {
        self.shared.lock().consumers.remove(&self.id);
        self.registered = false;
        self.wanted.notify_one();
    }
}

impl<T> Drop for RelayTrack<T> {
    fn drop(&mut self) {
        let mut s = self.shared.lock();
        s.consumers.remove(&self.id);
        let idle = s.all_idle();
        drop(s);
        if idle {
            self.wanted.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    //! A source that keeps only its newest frame, as a video bridge does.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::webrtc::bridge::{BridgeItem, PipelineBridge};

    #[derive(Clone, Debug)]
    struct Item(u32, Instant);

    impl BridgeItem for Item {}

    struct Latest {
        bridge: PipelineBridge<Item>,
        reads: AtomicUsize,
    }

    impl RelaySource<Item> for Latest {
        fn next(&self) -> Pin<Box<dyn Future<Output = Item> + Send + '_>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Box::pin(self.bridge.get_data())
        }
    }

    fn latest() -> Arc<Latest> {
        Arc::new(Latest {
            bridge: PipelineBridge::new(1, None, None),
            reads: AtomicUsize::new(0),
        })
    }

    async fn produce(source: &Latest, count: u32, interval: Duration) {
        for i in 0..count {
            source.bridge.set_data(Item(i, Instant::now()), true);
            tokio::time::sleep(interval).await;
        }
    }

    async fn consume(
        mut track: RelayTrack<Item>,
        per_frame: Duration,
        limit: usize,
    ) -> Vec<(u32, f64)> {
        let mut got = Vec::new();
        while got.len() < limit {
            let frame = track.recv().await;
            got.push((frame.0, frame.1.elapsed().as_secs_f64()));
            tokio::time::sleep(per_frame).await;
        }
        got
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slower_consumer_gets_recent_frames_not_a_backlog() {
        let source = latest();
        let relay = MediaRelay::new(source.clone());
        let consumer = tokio::spawn(consume(relay.subscribe(), Duration::from_millis(12), 30));
        produce(&source, 120, Duration::from_millis(4)).await;
        let got = consumer.await.unwrap();
        let oldest = got[5..].iter().map(|(_, a)| *a).fold(0.0, f64::max);
        assert!(oldest < 0.030, "oldest {oldest}");
        assert!(got.last().unwrap().0 - got[0].0 >= 2 * got.len() as u32);
        let mut ids: Vec<u32> = got.iter().map(|(i, _)| *i).collect();
        ids.dedup();
        assert_eq!(ids.len(), got.len());
        assert!(source.reads.load(Ordering::Relaxed) <= got.len() + 1);
        relay.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_consumer_is_handed_the_same_frames() {
        let source = latest();
        let relay = MediaRelay::new(source.clone());
        let fast = tokio::spawn(consume(relay.subscribe(), Duration::ZERO, 20));
        let slow = tokio::spawn(consume(relay.subscribe(), Duration::from_millis(10), 20));
        produce(&source, 80, Duration::from_millis(4)).await;
        let (fast, slow) = (fast.await.unwrap(), slow.await.unwrap());
        let ids = |v: &[(u32, f64)]| v.iter().map(|(i, _)| *i).collect::<Vec<_>>();
        // Each counts from its first `recv`, which the tasks reach at their own moment.
        let (fast, slow) = (ids(&fast), ids(&slow));
        let first = fast[0].max(slow[0]);
        let fast: Vec<u32> = fast.into_iter().filter(|i| *i >= first).collect();
        let slow: Vec<u32> = slow.into_iter().filter(|i| *i >= first).collect();
        let common = fast.len().min(slow.len());
        assert!(common >= 15, "{fast:?} {slow:?}");
        assert_eq!(fast[..common], slow[..common]);
        relay.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_consumer_that_stops_holds_no_one_back() {
        let source = latest();
        let relay = MediaRelay::new(source.clone());
        let leaving = tokio::spawn(consume(relay.subscribe(), Duration::ZERO, 3));
        let staying = tokio::spawn(consume(relay.subscribe(), Duration::ZERO, 15));
        produce(&source, 40, Duration::from_millis(4)).await;
        assert_eq!(leaving.await.unwrap().len(), 3);
        assert_eq!(staying.await.unwrap().len(), 15);
        relay.stop();
    }
}
