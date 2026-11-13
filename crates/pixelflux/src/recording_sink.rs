//! Unix-socket video fan-out for external recording.
//!
//! Frames are intercepted at the delivery layer (not inside each encoder), so the tap works
//! uniformly for every full-frame encoder. The pixelflux wire header is skipped so consumers
//! receive a plain elementary stream that is directly muxable: Annex-B for H.264 and H.265,
//! a temporal-unit OBU stream for AV1 (each unit opened by a temporal delimiter), and IVF for
//! VP8 and VP9, whose raw frames carry no framing of their own.
//!
//! The tap must never perturb the live viewer transport, and it never copies frame bytes:
//! stripe payloads are `Arc`-shared, so the encode thread only clones a handle into a bounded
//! per-client channel drained by a dedicated writer thread. A slow or stalled recorder blocks
//! nothing but itself and is dropped once its queue overflows. A newly connected client arms
//! [`RecordingSink::should_force_idr`] so the next encode emits an IDR it can decode from.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::encoders::codec::{Codec, VIDEO_HEADER_LEN, WIRE_VIDEO};
use crate::encoders::frame_rate::FrameRate;
use crate::encoders::software::EncodedStripe;

/// Per-write timeout on a client stream; a stalled write surfaces as a soft error that
/// [`write_all_frame`] retries, keeping the writer thread responsive to teardown.
const WRITE_TIMEOUT: Duration = Duration::from_millis(100);

/// How often the non-blocking accept loop retries when no client is waiting.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Per-client backlog bound. A recorder that falls this far behind is dropped rather than
/// allowed to grow memory or push back on the shared encode thread.
const CLIENT_QUEUE_CAP: usize = 256;

/// A queued frame: the container bytes the stream needs ahead of it (an IVF file or frame
/// header, an AV1 temporal delimiter; usually nothing), the `Arc`-shared payload, and the
/// byte offset where the recordable stream starts (past the wire header, or `0` when the
/// payload is bare).
struct QueuedFrame {
    prefix: Vec<u8>,
    #[expect(
        clippy::rc_buffer,
        reason = "the encoder's buffer as it hands it over; an `Arc<[u8]>` would copy every frame"
    )]
    data: Arc<Vec<u8>>,
    offset: usize,
}

/// The sink's handle to one connected recorder: the feed end of its bounded queue, a kill
/// switch for its writer thread, and how many frames it has been sent (the IVF frame index).
/// The socket itself is owned solely by that writer thread.
struct ClientHandle {
    tx: SyncSender<QueuedFrame>,
    stop: Arc<AtomicBool>,
    frames: u64,
}

/// Unix-socket fan-out that broadcasts every encoded video frame to connected consumers.
///
/// A listener thread accepts connections and gives each its own bounded queue and writer thread
/// (see [`ClientHandle`]) so one slow reader cannot stall the others or the encode thread.
pub struct RecordingSink {
    /// Filesystem path of the Unix socket; removed on drop.
    path: String,
    /// Feed handles for the connected clients, shared with the accept thread.
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    /// Signals the accept thread to exit; set in [`Drop`].
    shutdown: Arc<AtomicBool>,
    /// Flipped to `true` each time a new client connects; consumed by [`should_force_idr`].
    ///
    /// [`should_force_idr`]: RecordingSink::should_force_idr
    client_connected: Arc<AtomicBool>,
    /// The capture frame rate an IVF header declares.
    fps: FrameRate,
}

impl RecordingSink {
    /// Bind a Unix socket at `settings_path`, or return `None` when no path is configured or the
    /// bind fails. Recording is an optional tap that must never take the pipeline down, so a bind
    /// error is logged and swallowed.
    pub fn try_bind(settings_path: &str, fps: f64) -> Option<Arc<Self>> {
        if settings_path.is_empty() {
            return None;
        }
        match Self::bind(settings_path.to_string(), FrameRate::of(fps)) {
            Ok(sink) => Some(Arc::new(sink)),
            Err(e) => {
                eprintln!("[recording_sink] bind failed: {:?}", e);
                None
            }
        }
    }

    /// Create the socket and spawn the accept thread. Each accepted connection gets a write
    /// timeout, a bounded queue, and a writer thread; the sink keeps only the feed handle.
    fn bind(path: String, fps: FrameRate) -> std::io::Result<Self> {
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        crate::webcam::server::restrict_to_owner(&path)?;
        listener.set_nonblocking(true)?;

        let clients: Arc<Mutex<Vec<ClientHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let client_connected = Arc::new(AtomicBool::new(false));

        let clients_acc = clients.clone();
        let shutdown_acc = shutdown.clone();
        let client_connected_acc = client_connected.clone();
        let path_log = path.clone();

        thread::spawn(move || {
            eprintln!("[recording_sink] listening on {}", path_log);
            while !shutdown_acc.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if !crate::webcam::server::peer_is_session_user(stream.as_raw_fd()) {
                            continue;
                        }
                        if let Err(e) = stream.set_write_timeout(Some(WRITE_TIMEOUT)) {
                            eprintln!("[recording_sink] set_write_timeout failed: {:?}", e);
                            continue;
                        }

                        let (tx, rx) = sync_channel::<QueuedFrame>(CLIENT_QUEUE_CAP);
                        let stop = Arc::new(AtomicBool::new(false));
                        let stop_writer = stop.clone();
                        thread::spawn(move || {
                            let mut stream = stream;
                            for QueuedFrame {
                                prefix,
                                data,
                                offset,
                            } in rx.iter()
                            {
                                if stop_writer.load(Ordering::Relaxed) {
                                    break;
                                }
                                let written = if prefix.is_empty() {
                                    write_all_frame(&mut stream, &data[offset..], &stop_writer)
                                } else {
                                    write_all_frame(&mut stream, &prefix, &stop_writer).and_then(
                                        |()| {
                                            write_all_frame(
                                                &mut stream,
                                                &data[offset..],
                                                &stop_writer,
                                            )
                                        },
                                    )
                                };
                                if let Err(e) = written {
                                    eprintln!(
                                        "[recording_sink] writer thread exiting; write failed: {:?}",
                                        e
                                    );
                                    break;
                                }
                            }
                        });

                        let mut guard = clients_acc.lock().unwrap();
                        guard.push(ClientHandle {
                            tx,
                            stop,
                            frames: 0,
                        });
                        client_connected_acc.store(true, Ordering::Relaxed);
                        eprintln!("[recording_sink] client connected; total {}", guard.len());
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL_INTERVAL);
                    }
                    Err(e) => {
                        eprintln!("[recording_sink] accept error: {:?}", e);
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            eprintln!("[recording_sink] listener thread exiting");
        });

        Ok(Self {
            path,
            clients,
            shutdown,
            client_connected,
            fps,
        })
    }

    /// Returns `true` exactly once after a new client connects, signaling that the next encode
    /// should produce an IDR so the consumer starts from a clean reference frame.
    pub fn should_force_idr(&self) -> bool {
        self.client_connected.swap(false, Ordering::Relaxed)
    }

    /// Delivery-layer tap for one encoded frame: its video frame, JPEG carrying none. The wire
    /// header is skipped via the queued offset, and the container bytes the codec's stream needs
    /// are prefixed per client: an IVF file header on a VP8/VP9 client's first frame and an IVF
    /// frame header on every one, a temporal delimiter ahead of an AV1 unit that lacks one.
    ///
    /// Never blocks and never copies the payload: the `Arc` is cloned into each client's
    /// bounded queue with `try_send`, and a client whose queue is full or whose writer
    /// died is dropped.
    pub fn write_frame(&self, stripes: &[EncodedStripe], full_width: i32, full_height: i32) {
        let Some(stripe) = stripes
            .iter()
            .find(|s| s.codec.is_video() && !s.data.is_empty())
        else {
            return;
        };
        let offset = if stripe.data.len() >= VIDEO_HEADER_LEN && stripe.data[0] == WIRE_VIDEO {
            VIDEO_HEADER_LEN
        } else {
            0
        };
        if stripe.data.len() == offset {
            return;
        }
        let payload = &stripe.data[offset..];

        let mut clients = self.clients.lock().unwrap();
        if clients.is_empty() {
            return;
        }
        let mut to_remove: Vec<usize> = Vec::new();
        for (idx, client) in clients.iter_mut().enumerate() {
            let prefix = stream_prefix(
                stripe.codec,
                payload,
                client.frames,
                (full_width as u16, full_height as u16),
                self.fps,
            );
            client.frames += 1;
            let queued = QueuedFrame {
                prefix,
                data: stripe.data.clone(),
                offset,
            };
            match client.tx.try_send(queued) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    eprintln!("[recording_sink] dropping slow client (idx {})", idx);
                    to_remove.push(idx);
                }
                Err(TrySendError::Disconnected(_)) => {
                    to_remove.push(idx);
                }
            }
        }
        for idx in to_remove.into_iter().rev() {
            let removed = clients.swap_remove(idx);
            removed.stop.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for RecordingSink {
    /// Stop accepting, release every writer thread (set each `stop`, then drop its sender so an
    /// idle writer parked on `rx.iter()` wakes), and remove the socket file.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Ok(mut clients) = self.clients.lock() {
            for client in clients.iter() {
                client.stop.store(true, Ordering::Relaxed);
            }
            clients.clear();
        }
        let _ = fs::remove_file(&self.path);
    }
}

/// The container bytes a client's stream needs ahead of its `index`th frame: an IVF file
/// header (on the first frame) and frame header for VP8 and VP9, a temporal delimiter for an
/// AV1 unit that does not open with one, nothing for the Annex-B codecs. The IVF header's time
/// base is one frame at `fps`, which the frame index counts in.
fn stream_prefix(
    codec: Codec,
    payload: &[u8],
    index: u64,
    size: (u16, u16),
    fps: FrameRate,
) -> Vec<u8> {
    match codec {
        Codec::Vp8 | Codec::Vp9 => {
            let mut prefix = Vec::with_capacity(44);
            if index == 0 {
                prefix.extend_from_slice(b"DKIF");
                prefix.extend_from_slice(&0u16.to_le_bytes());
                prefix.extend_from_slice(&32u16.to_le_bytes());
                prefix.extend_from_slice(if codec == Codec::Vp8 {
                    b"VP80"
                } else {
                    b"VP90"
                });
                prefix.extend_from_slice(&size.0.to_le_bytes());
                prefix.extend_from_slice(&size.1.to_le_bytes());
                prefix.extend_from_slice(&fps.num.to_le_bytes());
                prefix.extend_from_slice(&fps.den.to_le_bytes());
                prefix.extend_from_slice(&0u32.to_le_bytes());
                prefix.extend_from_slice(&0u32.to_le_bytes());
            }
            prefix.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            prefix.extend_from_slice(&index.to_le_bytes());
            prefix
        }
        Codec::Av1 if payload.first().is_none_or(|&b| (b >> 3) & 0x0f != 2) => vec![0x12, 0x00],
        _ => Vec::new(),
    }
}

/// Write one whole frame to a recorder's socket, resuming across the soft timeouts a slow reader
/// induces so a partial NAL or OBU is never left behind. Aborts if `stop` is set (the client was
/// dropped by [`RecordingSink::write_frame`]) or a hard error occurs.
fn write_all_frame<W: Write>(stream: &mut W, buf: &[u8], stop: &AtomicBool) -> std::io::Result<()> {
    let mut written = 0usize;
    while written < buf.len() {
        if stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("writer stopped (client dropped)"));
        }
        match stream.write(&buf[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "failed to write whole frame",
                ));
            }
            Ok(n) => written += n,
            Err(ref e) if e.kind() == ErrorKind::TimedOut => {}
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod prefix_tests {
    use super::*;

    /// The Annex-B codecs need nothing ahead of a frame; VP8 and VP9 get an IVF file header
    /// on a client's first frame and a frame header on every one, carrying the size and the
    /// frame index; an AV1 unit is opened with a temporal delimiter only when it lacks one.
    #[test]
    fn prefixes_follow_the_codec() {
        assert!(
            stream_prefix(
                Codec::H264,
                &[0, 0, 1, 0x65],
                0,
                (1280, 720),
                FrameRate::of(60.0)
            )
            .is_empty()
        );
        assert!(
            stream_prefix(
                Codec::H265,
                &[0, 0, 1, 0x26, 1],
                3,
                (1280, 720),
                FrameRate::of(60.0)
            )
            .is_empty()
        );

        let first = stream_prefix(
            Codec::Vp9,
            &[0x82, 0x49, 0x83],
            0,
            (1280, 720),
            FrameRate::of(60.0),
        );
        assert_eq!(first.len(), 44);
        assert_eq!(&first[..4], b"DKIF");
        assert_eq!(&first[8..12], b"VP90");
        assert_eq!(u16::from_le_bytes([first[12], first[13]]), 1280);
        assert_eq!(u16::from_le_bytes([first[14], first[15]]), 720);
        assert_eq!(
            u32::from_le_bytes([first[16], first[17], first[18], first[19]]),
            60
        );
        assert_eq!(
            u32::from_le_bytes([first[20], first[21], first[22], first[23]]),
            1
        );
        assert_eq!(
            u32::from_le_bytes([first[32], first[33], first[34], first[35]]),
            3
        );
        assert_eq!(u64::from_le_bytes(first[36..44].try_into().unwrap()), 0);
        let later = stream_prefix(Codec::Vp8, &[0u8; 100], 7, (1280, 720), FrameRate::of(60.0));
        assert_eq!(later.len(), 12);
        assert_eq!(u32::from_le_bytes(later[..4].try_into().unwrap()), 100);
        assert_eq!(u64::from_le_bytes(later[4..].try_into().unwrap()), 7);
        assert_eq!(
            &stream_prefix(Codec::Vp8, &[], 0, (64, 64), FrameRate::of(30.0))[8..12],
            b"VP80"
        );

        assert!(
            stream_prefix(
                Codec::Av1,
                &[0x12, 0x00, 0x32, 0x01, 0x10],
                0,
                (64, 64),
                FrameRate::of(30.0)
            )
            .is_empty()
        );
        assert_eq!(
            stream_prefix(
                Codec::Av1,
                &[0x32, 0x01, 0x10],
                0,
                (64, 64),
                FrameRate::of(30.0)
            ),
            vec![0x12, 0x00]
        );
        assert_eq!(
            stream_prefix(Codec::Av1, &[], 0, (64, 64), FrameRate::of(30.0)),
            vec![0x12, 0x00]
        );
        let ntsc = stream_prefix(
            Codec::Vp8,
            &[],
            0,
            (64, 64),
            FrameRate::of(60000.0 / 1001.0),
        );
        assert_eq!(
            u32::from_le_bytes(ntsc[16..20].try_into().unwrap()),
            60000,
            "59.94 fps is 1001/60000 s a frame"
        );
        assert_eq!(u32::from_le_bytes(ntsc[20..24].try_into().unwrap()), 1001);
    }
}

#[cfg(test)]
mod cost_tests {
    //! The sink's isolation contract, measured: feeding a frame must cost nothing when no
    //! recorder is connected (empty-clients early return), microseconds when a healthy
    //! recorder drains its queue, and stay bounded (a lock + `try_send`, never a blocking
    //! write) when a recorder stalls completely — until the bounded queue overflows and the
    //! client is dropped, returning the tap to idle cost.

    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    fn frame(len: usize) -> EncodedStripe {
        let mut data = vec![0u8; len];
        data[0] = WIRE_VIDEO;
        EncodedStripe {
            data: Arc::new(data),
            codec: Codec::H264,
            stripe_y_start: 0,
            stripe_height: 720,
            frame_id: 0,
            timing: Default::default(),
            reference: Default::default(),
        }
    }

    /// The time each of `n` feeds took, in microseconds, sorted.
    fn feed_timed(sink: &RecordingSink, n: usize, len: usize) -> Vec<f64> {
        let f = frame(len);
        let mut us: Vec<f64> = (0..n)
            .map(|_| {
                let t = Instant::now();
                sink.write_frame(std::slice::from_ref(&f), 1280, 720);
                let elapsed = t.elapsed().as_secs_f64() * 1e6;
                thread::sleep(Duration::from_micros(200));
                elapsed
            })
            .collect();
        us.sort_by(f64::total_cmp);
        us
    }

    /// Each phase is judged by the median and the 99th percentile of its feeds, which one
    /// preemption of the test thread does not move, where a mean or a maximum would.
    #[test]
    fn stalled_recorder_isolation_cost() {
        let path = format!("/tmp/pf-sink-cost-{}.sock", std::process::id());
        let sink = RecordingSink::try_bind(&path, 60.0).expect("bind");

        // Idle: no client connected.
        let idle = feed_timed(&sink, 500, 100_000);

        // Healthy: a client draining as fast as it can.
        let mut healthy = UnixStream::connect(&path).expect("connect");
        thread::sleep(Duration::from_millis(200));
        let drain = thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 20];
            while healthy.read(&mut buf).map(|n| n > 0).unwrap_or(false) {}
        });
        let healthy = feed_timed(&sink, 500, 100_000);

        // Stalled: a connected client that never reads. The socket buffer fills, then the
        // bounded queue fills, then the client is dropped (~256 frames later).
        let reader = UnixStream::connect(&path).expect("connect");
        thread::sleep(Duration::from_millis(200));
        let stalled = feed_timed(&sink, 500, 100_000);
        drop(reader);

        let median = |us: &[f64]| us[us.len() / 2];
        let p99 = |us: &[f64]| us[us.len() * 99 / 100];
        for (phase, us) in [
            ("idle   ", &idle),
            ("healthy", &healthy),
            ("stalled", &stalled),
        ] {
            println!(
                "[sink-cost] {phase} median {:.3}us p99 {:.3}us",
                median(us),
                p99(us)
            );
        }
        drop(sink);
        let _ = drain.join();

        assert!(
            median(&idle) < 5.0,
            "idle feed should be sub-5us, was {:.3}us",
            median(&idle)
        );
        assert!(
            median(&healthy) < 100.0,
            "healthy feed should be tens of us, was {:.3}us",
            median(&healthy)
        );
        assert!(
            p99(&stalled) < 10_000.0,
            "a stalled recorder must never block the tap >10ms, was {:.3}us",
            p99(&stalled)
        );
    }
}
