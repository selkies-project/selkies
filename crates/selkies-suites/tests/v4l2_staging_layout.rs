/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The V4L2 interposer (addons/v4l2-interposer) checks the staging layout its backend hands it.
//!
//! The frame ring's geometry arrives over the socket with the memfd it describes. A layout whose
//! frames run past the end of that memfd fails the device's open() with a log line, since a read
//! of a frame beyond the end of the file faults the application; a layout that fits is configured.
//!
//! Serves both layouts from a stand-in backend here and opens the device through the interposer
//! with the libc-only `tests/tools/v4l2probe`, both built with make; no pixelflux, no display, so
//! it runs with `cargo test --workspace`.

use std::io::{IoSlice, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use selkies_suites::server::run_within;
use selkies_suites::{Checks, repo, workdir};

const MAGIC: u32 = 0x434B_5753;
const VERSION: u32 = 1;
const MJPG: u32 = u32::from_le_bytes(*b"MJPG");
const DATA_OFFSET: u32 = 4096;
const CTRL_OFFSET: u32 = 128;
const CTRL_STRIDE: u32 = 64;
const SLOTS: u32 = 2;
const SLOT_SIZE: u32 = 1 << 20;

/// Sends `data` with `fd` passed beside it, as the backend hands the interposer its memfd.
fn send_with_fd(stream: &UnixStream, data: &[u8], fd: i32) -> std::io::Result<()> {
    let iov = [IoSlice::new(data)];
    let fd_size = u32::try_from(std::mem::size_of::<i32>()).expect("an fd's size");
    // SAFETY: CMSG_SPACE and CMSG_LEN compute sizes only.
    let (space, len) = unsafe { (libc::CMSG_SPACE(fd_size), libc::CMSG_LEN(fd_size)) };
    let mut control = vec![0u8; usize::try_from(space).expect("a control size")];
    // SAFETY: an all-zero msghdr is a valid empty one; its pointers are set below to buffers that
    // outlive the call, and the one control message is written inside `control`.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = iov.as_ptr().cast_mut().cast();
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len();
        let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = usize::try_from(len).expect("a control length");
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd);
        if libc::sendmsg(stream.as_raw_fd(), &raw const msg, 0) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Answers one interposer connection on `path` with the fixed layout over a file of
/// `memfd_size` bytes, which the interposer maps as it would the backend's memfd.
fn serve_once(path: &Path, staging: &Path, memfd_size: u64) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(path).expect("the webcam socket");
    let staging = staging.to_owned();
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else {
            return;
        };
        // Read and write, as a memfd is: the interposer maps it both ways.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&staging)
            .expect("the staging file");
        file.set_len(memfd_size).expect("the staging file's size");
        let fields = [
            MAGIC,
            VERSION,
            64,
            48,
            MJPG,
            30,
            1,
            SLOTS,
            SLOT_SIZE,
            DATA_OFFSET,
            CTRL_OFFSET,
            CTRL_STRIDE,
            0,
            4096,
        ];
        let mut cfg: Vec<u8> = fields.iter().flat_map(|v| v.to_le_bytes()).collect();
        cfg.extend_from_slice(&[0; 8]);
        send_with_fd(&conn, &cfg, file.as_raw_fd()).expect("the layout sent");
        let _ = conn.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = conn.read(&mut [0u8; 1]);
    })
}

/// What the probe logs opening the device with a memfd of `memfd_size` bytes.
fn open_through_interposer(dir: &Path, memfd_size: u64) -> String {
    let sockets = dir.join(format!("sockets-{memfd_size}"));
    let _ = std::fs::remove_dir_all(&sockets);
    std::fs::create_dir_all(&sockets).expect("the socket directory");
    let server = serve_once(
        &sockets.join("selkies_webcam0.sock"),
        &sockets.join("staging"),
        memfd_size,
    );
    let ran = run_within(
        Command::new(repo().join("tests/tools/v4l2probe"))
            .args(["--timeout", "300", "/dev/video0", "1"])
            .env(
                "LD_PRELOAD",
                repo().join("addons/v4l2-interposer/selkies_v4l2_interposer.so"),
            )
            .env("SELKIES_WEBCAM_SOCKET_PATH", &sockets)
            .env("SELKIES_WEBCAM_SOURCE", "socket")
            .env("WEBCAM_LOG", "1"),
        Duration::from_secs(30),
    );
    let _ = server.join();
    ran.map(|r| r.stderr).unwrap_or_else(|e| e)
}

/// The last `n` characters of `text`.
fn tail(text: &str, n: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars[chars.len().saturating_sub(n)..].iter().collect()
}

#[test]
fn the_interposer_checks_the_staging_layout() {
    for (dir, target) in [
        ("addons/v4l2-interposer", None),
        ("tests/tools", Some("v4l2probe")),
    ] {
        let mut make = Command::new("make");
        make.arg("-s").arg("-C").arg(repo().join(dir));
        if let Some(target) = target {
            make.arg(target);
        }
        let built = run_within(&mut make, Duration::from_secs(180)).expect("make ran");
        assert!(
            built.status.success(),
            "make -C {dir} failed: {}",
            built.stderr
        );
    }
    let dir = workdir().join(format!("v4l2-staging-layout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    let mut checks = Checks::new("v4l2-staging-layout");
    let fits = u64::from(DATA_OFFSET) + u64::from(SLOTS) * u64::from(SLOT_SIZE);
    let log = open_through_interposer(&dir, fits);
    checks.check(
        "a layout inside its memfd is configured",
        log.contains("configured 64x48"),
        tail(&log, 160),
    );
    let log = open_through_interposer(&dir, 2 * u64::from(DATA_OFFSET));
    checks.check(
        "frames past the end of the memfd fail the open",
        log.contains("staging layout outside its memfd") && !log.contains("configured 64x48"),
        tail(&log, 160),
    );
    checks.finish("");
}
