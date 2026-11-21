/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the WebSockets data plane does for every video frame it sends, timed for CodSpeed: an
//! encoded frame as pixelflux delivers it shared into the chunk the fan-out offers
//! (`capture::frame_bytes`, `RelayItem`), the display owner's `VideoRelay` taking it
//! (`offer`: its header, budget, rows, and the frames the page holds) and draining it through
//! the socket's writer (`Client::send_live_pinged`, `run_writer`), and tokio-websockets framing
//! it into a socket that is memory, not a kernel's: the bytes are counted and let go. The
//! frame counts as sent once the relay's page holds it (`CommonFrames`), the last step of a
//! frame's way out.
//!
//! The relay serves through a stand-in for the server (`RelayHost`): the page owns the display,
//! no ping is written behind a frame, and the encoder is told nothing, so the server's
//! registry and link bookkeeping (`WsService::fan_out`) are not in the figure. Every task runs
//! on one current-thread runtime, and a timed iteration sends `BURST` frames, each a new one
//! predicting from the one before (or a key frame), built outside the timed call, after a key
//! frame and a few deltas checked to reach the socket whole.
//!
//! `cargo bench -p selkies --bench ws_send` runs them under Criterion; CI runs them under
//! CodSpeed's instruction counting (`cargo codspeed`), which keys its history on the names
//! below.

use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use futures::StreamExt;
use futures::stream::SplitStream;
use parking_lot::Mutex;
use pixelflux::encoders::reference::Reference;
use pixelflux::encoders::{Codec, FRAME_DELTA, FRAME_KEY, push_video_header};
use pixelflux::sink::{Frame, FrameTiming};
use selkies::capture::frame_bytes;
use selkies::display::capture::CommonFrames;
use selkies::ws::client::{Client, ClientId, Liveness, NoLink, Perms, run_writer};
use selkies::ws::link::DisplayLink;
use selkies::ws::relay::{RelayHost, RelayItem, RunEvent, VideoRelay};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio_websockets::{ServerBuilder, WebSocketStream};

const DISPLAY: &str = "primary";
const PAGE: ClientId = 1;
const W: u16 = 1280;
const H: u16 = 720;

/// The socket under the WebSocket: what is written is counted and let go, and nothing is ever
/// read.
struct Wire(Arc<AtomicU64>);

impl AsyncRead for Wire {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// The server as the relay sees it: one page owning the display, whose frames held are told to
/// `written`.
struct Host {
    queued: Mutex<CommonFrames<ClientId>>,
    held: Mutex<CommonFrames<ClientId>>,
}

impl Host {
    fn new(written: Arc<Notify>) -> Self {
        Host {
            queued: Mutex::new(CommonFrames::new(|_| {})),
            held: Mutex::new(CommonFrames::new(move |_| written.notify_one())),
        }
    }
}

impl RelayHost for Host {
    fn owns_display(&self, client: ClientId, _: &str) -> bool {
        client == PAGE
    }

    fn relay_lost(&self, _: ClientId, _: &str, _: u16, _: bool) -> bool {
        false
    }

    fn with_common_frames(
        &self,
        _: &str,
        held: bool,
        f: &mut dyn FnMut(&mut CommonFrames<ClientId>),
    ) {
        let common = if held { &self.held } else { &self.queued };
        f(&mut common.lock());
    }

    fn note_send(&self, _: ClientId, _: &str, _: &RelayItem) -> bool {
        false
    }

    fn connection_verdict(&self, _: ClientId, _: bool) {}

    fn socket_died(&self, _: ClientId) {}

    fn relay_finished(&self, _: &str, _: &Arc<VideoRelay>) {}

    fn relay_run(&self, _: &str, _: RunEvent) {}

    fn note_padding(
        &self,
        _: ClientId,
        _: &str,
        _: usize,
    ) -> Option<(Arc<Mutex<DisplayLink>>, u64)> {
        None
    }
}

/// One page's socket and the display owner's relay to it, on a runtime of their own.
struct Plane {
    runtime: Runtime,
    relay: Arc<VideoRelay>,
    written: Arc<Notify>,
    wire: Arc<AtomicU64>,
    /// The socket's read half, which nothing reads, kept so the write half stays whole.
    _stream: SplitStream<WebSocketStream<Wire>>,
}

impl Plane {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _entered = runtime.enter();
        let wire = Arc::new(AtomicU64::new(0));
        let (sink, stream) = ServerBuilder::new().serve(Wire(wire.clone())).split();
        let perms = Perms {
            token: None,
            role: "controller".into(),
            slot: serde_json::Value::Null,
            id: "0badc0de".into(),
            connected_at: 0.0,
        };
        let (client, outbox, abort) = Client::new(
            PAGE,
            "127.0.0.1:8080".parse().expect("address"),
            perms,
            Box::new(NoLink),
            Liveness::default(),
        );
        tokio::spawn(run_writer(client.clone(), outbox, abort, sink));
        let written = Arc::new(Notify::new());
        let host: Arc<dyn RelayHost> = Arc::new(Host::new(written.clone()));
        let relay = VideoRelay::new(host, DISPLAY, client, usize::MAX);
        relay.start();
        Plane {
            runtime,
            relay,
            written,
            wire,
            _stream: stream,
        }
    }

    /// Send `frame` through the plane, returning once its page holds it.
    fn send(&self, frame: &Frame) {
        self.runtime.block_on(async {
            self.relay.offer(RelayItem {
                data: frame_bytes(frame),
                frame_id: (frame.frame_id & 0xFFFF) as u16,
                capture_ns: frame.timing.capture_ns,
                encode_start_ns: frame.timing.encode_start_ns,
                encode_end_ns: frame.timing.encode_end_ns,
            });
            self.written.notified().await;
        });
    }
}

/// The encoded frames of one stream, one after another: each `len` bytes with its header, the
/// first a key frame, each later one a key frame where `keys`, else a delta predicting from the
/// one before.
struct Stream {
    len: usize,
    keys: bool,
    next: u16,
}

impl Stream {
    fn frame(&mut self) -> Frame {
        let id = self.next;
        self.next = id.wrapping_add(1);
        let key = self.keys || id == 0;
        let mut data = Vec::with_capacity(self.len);
        let (kind, reference) = if key {
            (FRAME_KEY, Reference::None)
        } else {
            (FRAME_DELTA, Reference::Frame(id.wrapping_sub(1)))
        };
        push_video_header(&mut data, Codec::H264, kind, id, 0, W, H, reference);
        data.resize(self.len, 0x5a);
        Frame {
            data: Arc::new(data),
            codec: Codec::H264,
            stripe_y_start: 0,
            stripe_height: i32::from(H),
            frame_id: i32::from(id),
            timing: FrameTiming::default(),
            reference,
        }
    }
}

/// Frames a timed iteration sends: the order in which the runtime polls the relay, the writer,
/// and the waits within each step of one frame differs from run to run (`tokio::select!` starts
/// at a random branch), and over this many it evens out.
const BURST: usize = 16;

/// Time `BURST` frames of `len` bytes through the plane, every one a key frame where `keys`.
fn bench_send(c: &mut Criterion, name: &str, len: usize, keys: bool) {
    let plane = Plane::new();
    let mut stream = Stream { len, keys, next: 0 };
    for _ in 0..4 {
        let frame = stream.frame();
        let before = plane.wire.load(Ordering::Relaxed);
        plane.send(&frame);
        let id = (frame.frame_id & 0xFFFF) as u16;
        assert!(
            plane.relay.was_sent(id) && plane.wire.load(Ordering::Relaxed) - before > len as u64,
            "{name}: frame {id} did not reach the socket whole"
        );
    }
    c.bench_function(name, |b| {
        b.iter_batched(
            || (0..BURST).map(|_| stream.frame()).collect::<Vec<_>>(),
            |frames| {
                for frame in &frames {
                    plane.send(black_box(frame));
                }
                frames
            },
            BatchSize::PerIteration,
        );
    });
}

fn ws_send(c: &mut Criterion) {
    bench_send(c, "ws_send_16_video_deltas_16k", 16 * 1024, false);
    bench_send(c, "ws_send_16_video_keys_256k", 256 * 1024, true);
}

criterion_group!(benches, ws_send);
criterion_main!(benches);
