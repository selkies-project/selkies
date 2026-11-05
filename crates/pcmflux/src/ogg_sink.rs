/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Ogg Opus fan-out on a Unix socket: every consumer that connects receives the stream's
//! `OpusHead` and `OpusTags` pages, then one page per encoded packet, so the socket reads
//! as a standard Ogg Opus stream (`ffmpeg -i unix:<path>`, the pixelflux recorder).
//!
//! The tap never perturbs the capture: the capture thread only hands each page to a
//! bounded per-consumer channel drained by that consumer's writer thread, and a consumer
//! that falls behind is dropped rather than waited on.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Per-consumer backlog: two seconds of 20 ms packets before a consumer is dropped.
const CLIENT_QUEUE_CAP: usize = 100;
/// Readers at once, each with a writer thread and a queue; more are refused.
const MAX_CONSUMERS: usize = 16;
/// The Ogg page checksum's polynomial, applied to the whole page with the field zeroed.
const CRC_POLY: u32 = 0x04c1_1db7;

/// What `OpusHead` tells a decoder; `mapping` is `(streams, coupled, table)` for channel
/// mapping family 1, absent for family 0 (mono and stereo).
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub mapping: Option<(u8, u8, Vec<u8>)>,
}

/// Whether a socket's peer runs as this process's user or as root, who passes the
/// socket's mode anyway: one that connected before the mode was set is refused here.
fn peer_is_session_user(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    r == 0 && (cred.uid == unsafe { libc::geteuid() } || cred.uid == 0)
}

struct Client {
    tx: SyncSender<Arc<Vec<u8>>>,
    stop: Arc<AtomicBool>,
}

/// One Ogg Opus stream served to every consumer of a Unix socket.
pub struct OggSink {
    path: String,
    clients: Arc<Mutex<Vec<Client>>>,
    shutdown: Arc<AtomicBool>,
    serial: u32,
    seq: u32,
}

/// Ogg's CRC-32 of every byte value, a byte at a time in place of a bit.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ CRC_POLY
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |crc, &b| {
        (crc << 8) ^ CRC_TABLE[((crc >> 24) ^ b as u32) as usize]
    })
}

/// One page carrying one packet: the lacing table splits it into 255-byte segments, a
/// final short segment (empty when the length is a multiple of 255) closing it.
fn page(serial: u32, seq: u32, granule: u64, header_type: u8, packet: &[u8]) -> Vec<u8> {
    let mut lacing: Vec<u8> = vec![255; packet.len() / 255];
    lacing.push((packet.len() % 255) as u8);
    let mut out = Vec::with_capacity(27 + lacing.len() + packet.len());
    out.extend_from_slice(b"OggS");
    out.push(0);
    out.push(header_type);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.push(lacing.len() as u8);
    out.extend_from_slice(&lacing);
    out.extend_from_slice(packet);
    let crc = crc32(&out).to_le_bytes();
    out[22..26].copy_from_slice(&crc);
    out
}

/// The identification and comment headers as the stream's first two pages.
fn header_pages(serial: u32, head: &OpusHead) -> Vec<u8> {
    let mut id = b"OpusHead".to_vec();
    id.push(1);
    id.push(head.channels);
    id.extend_from_slice(&head.pre_skip.to_le_bytes());
    id.extend_from_slice(&head.input_sample_rate.to_le_bytes());
    id.extend_from_slice(&0i16.to_le_bytes());
    match &head.mapping {
        None => id.push(0),
        Some((streams, coupled, table)) => {
            id.push(1);
            id.push(*streams);
            id.push(*coupled);
            id.extend_from_slice(table);
        }
    }
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&(b"pcmflux".len() as u32).to_le_bytes());
    tags.extend_from_slice(b"pcmflux");
    tags.extend_from_slice(&0u32.to_le_bytes());
    let mut out = page(serial, 0, 0, 0x02, &id);
    out.extend_from_slice(&page(serial, 1, 0, 0, &tags));
    out
}

impl OggSink {
    /// Bind the socket at `path`, or say why it cannot be bound. A file there that another
    /// account owns is left alone and refused: in a shared directory it is that account's
    /// listener, and a reader of this stream would reach it. A file that is not a socket is
    /// refused too, whoever owns it: a path named by mistake is never deleted.
    pub fn bind(path: &str, head: &OpusHead) -> Result<Self, String> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let stale = match fs::symlink_metadata(path) {
            Ok(meta) if meta.uid() != unsafe { libc::geteuid() } => {
                Err("another account owns it".to_string())
            }
            Ok(meta) if !meta.file_type().is_socket() => Err("it is not a socket".to_string()),
            Ok(_) => fs::remove_file(path).map_err(|e| e.to_string()),
            Err(_) => Ok(()),
        };
        let listener = stale
            .and_then(|_| {
                UnixListener::bind(path)
                    .and_then(|l| {
                        use std::os::unix::fs::PermissionsExt;
                        // Its user's alone, whatever the umask: a reader hears the desktop.
                        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                        l.set_nonblocking(true).map(|_| l)
                    })
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| format!("output_socket {path} cannot be bound: {e}"))?;
        let serial = std::process::id()
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0));
        let headers = Arc::new(header_pages(serial, head));
        let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (clients_acc, shutdown_acc, path_log) =
            (clients.clone(), shutdown.clone(), path.to_string());
        thread::spawn(move || {
            while !shutdown_acc.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if !peer_is_session_user(&stream)
                            || clients_acc.lock().unwrap().len() >= MAX_CONSUMERS
                        {
                            continue;
                        }
                        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
                        let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(CLIENT_QUEUE_CAP);
                        let stop = Arc::new(AtomicBool::new(false));
                        let stop_writer = stop.clone();
                        let _ = tx.try_send(headers.clone());
                        thread::spawn(move || {
                            let mut stream = stream;
                            for bytes in rx.iter() {
                                if stop_writer.load(Ordering::Relaxed)
                                    || stream.write_all(&bytes).is_err()
                                {
                                    break;
                                }
                            }
                        });
                        clients_acc.lock().unwrap().push(Client { tx, stop });
                        eprintln!("[pcmflux] ogg sink consumer connected on {path_log}");
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL_INTERVAL)
                    }
                    Err(e) => {
                        eprintln!("[pcmflux] ogg sink accept error: {e}");
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        });
        Ok(Self {
            path: path.to_string(),
            clients,
            shutdown,
            serial,
            seq: 2,
        })
    }

    /// Send one packet as a page; `granule` is the 48 kHz sample count at its end.
    pub fn write_packet(&mut self, packet: &[u8], granule: u64) {
        let bytes = Arc::new(page(self.serial, self.seq, granule, 0, packet));
        self.seq = self.seq.wrapping_add(1);
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| match c.tx.try_send(bytes.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                c.stop.store(true, Ordering::Relaxed);
                eprintln!("[pcmflux] ogg sink dropping a consumer that stopped reading");
                false
            }
        });
    }
}

impl Drop for OggSink {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for c in self.clients.lock().unwrap().drain(..) {
            c.stop.store(true, Ordering::Relaxed);
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    /// A leftover this account owns is removed and the socket bound, and the sink removes
    /// its socket when it is dropped.
    #[test]
    fn a_stale_socket_of_this_session_is_replaced() {
        let path = format!("/tmp/pcmflux-ogg-stale-{}.sock", std::process::id());
        let _ = fs::remove_file(&path);
        // A listener gone without removing its socket file, as after a crash.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let head = OpusHead {
            channels: 2,
            pre_skip: 312,
            input_sample_rate: 48000,
            mapping: None,
        };
        let sink = OggSink::bind(&path, &head);
        assert!(
            sink.is_ok(),
            "an own leftover is removed and the socket bound"
        );
        drop(sink);
        assert!(
            fs::symlink_metadata(&path).is_err(),
            "the socket is removed with the sink"
        );
    }

    /// A path that cannot be bound is an error naming it, not a sink that serves nobody.
    /// The socket is its user's alone whatever the umask, and a peer of that user is served.
    #[test]
    fn the_socket_is_its_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pcmflux-ogg-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let path = dir.join("out.sock").to_string_lossy().into_owned();
        let head = OpusHead {
            channels: 2,
            pre_skip: 312,
            input_sample_rate: 48000,
            mapping: None,
        };
        let old = unsafe { libc::umask(0) };
        let sink = OggSink::bind(&path, &head);
        unsafe { libc::umask(old) };
        let sink = sink.expect("bind");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let peer = UnixStream::connect(&path).unwrap();
        assert!(peer_is_session_user(&peer));
        drop(sink);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The table gives what the polynomial gives a bit at a time: nothing for nothing, the
    /// check value, and the same checksum over any bytes, zeros and every byte value included.
    #[test]
    fn the_checksum_table_agrees_with_the_polynomial() {
        fn bitwise(data: &[u8]) -> u32 {
            let mut crc = 0u32;
            for &b in data {
                crc ^= (b as u32) << 24;
                for _ in 0..8 {
                    crc = if crc & 0x8000_0000 != 0 {
                        (crc << 1) ^ CRC_POLY
                    } else {
                        crc << 1
                    };
                }
            }
            crc
        }
        assert_eq!(crc32(&[]), 0);
        assert_eq!(crc32(&[0; 64]), 0);
        assert_eq!(crc32(b"123456789"), 0x89a1_897f);
        let every: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        for data in [&every[..], &every[..255], &every[1..256], &[0xff; 300][..]] {
            assert_eq!(crc32(data), bitwise(data));
        }
    }

    /// A file at the socket's path that is not a socket stays, whoever owns it.
    #[test]
    fn a_file_that_is_not_a_socket_is_left_alone() {
        let path = format!("/tmp/pcmflux-ogg-file-{}", std::process::id());
        fs::write(&path, b"kept").unwrap();
        let head = OpusHead {
            channels: 2,
            pre_skip: 312,
            input_sample_rate: 48000,
            mapping: None,
        };
        let err = OggSink::bind(&path, &head)
            .err()
            .expect("a regular file is refused");
        assert!(err.contains("not a socket"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"kept");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn an_unbindable_path_is_an_error() {
        let path = format!("/tmp/pcmflux-ogg-missing-{}/rec.sock", std::process::id());
        let head = OpusHead {
            channels: 2,
            pre_skip: 312,
            input_sample_rate: 48000,
            mapping: None,
        };
        let err = OggSink::bind(&path, &head)
            .err()
            .expect("a missing directory is refused");
        assert!(err.contains(&path), "{err}");
    }

    /// A consumer that connects reads the two header pages and then every packet page, each
    /// with the granule it was given. A packet that fills its last segment exactly is closed by
    /// a zero lacing value, and the checksum is Ogg's own, whose check value over `123456789` is
    /// 0x89a1897f: recomputing a page's checksum with this module's function alone would pass
    /// for any CRC-32, while libogg rejects every page of another.
    #[test]
    fn stream_pages_are_well_formed() {
        assert_eq!(crc32(b"123456789"), 0x89a1_897f);
        let path = format!("/tmp/pcmflux-ogg-test-{}.sock", std::process::id());
        let head = OpusHead {
            channels: 2,
            pre_skip: 312,
            input_sample_rate: 48000,
            mapping: None,
        };
        let mut sink = OggSink::bind(&path, &head).expect("bind");
        let mut consumer = UnixStream::connect(&path).expect("connect");
        consumer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        thread::sleep(Duration::from_millis(150));
        sink.write_packet(&[0xfc, 1, 2, 3], 960);
        sink.write_packet(&vec![0xfc; 255], 1920);
        thread::sleep(Duration::from_millis(150));
        let mut buf = vec![0u8; 4096];
        let mut got = Vec::new();
        while let Ok(n) = consumer.read(&mut buf) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            if got.len() >= 27 * 4 + 19 + 27 + 4 + 259 {
                break;
            }
        }
        let mut pages = Vec::new();
        let mut laces = Vec::new();
        let mut at = 0;
        while at + 27 <= got.len() {
            assert_eq!(&got[at..at + 4], b"OggS");
            let segments = got[at + 26] as usize;
            let body: usize = got[at + 27..at + 27 + segments]
                .iter()
                .map(|&l| l as usize)
                .sum();
            let end = at + 27 + segments + body;
            let mut copy = got[at..end].to_vec();
            let stored = u32::from_le_bytes(copy[22..26].try_into().unwrap());
            copy[22..26].copy_from_slice(&[0; 4]);
            assert_eq!(crc32(&copy), stored, "page {} checksum", pages.len());
            let granule = u64::from_le_bytes(copy[6..14].try_into().unwrap());
            laces.push(copy[27..27 + segments].to_vec());
            pages.push((granule, copy[27 + segments..].to_vec()));
            at = end;
        }
        assert_eq!(pages.len(), 4);
        assert!(pages[0].1.starts_with(b"OpusHead"));
        assert_eq!(pages[0].1[9], 2);
        assert!(pages[1].1.starts_with(b"OpusTags"));
        assert_eq!(pages[2], (960, vec![0xfc, 1, 2, 3]));
        assert_eq!(pages[3].0, 1920);
        assert_eq!(pages[3].1.len(), 255);
        assert_eq!(laces[3], vec![255, 0]);
        drop(sink);
        assert!(!std::path::Path::new(&path).exists());
    }
}
