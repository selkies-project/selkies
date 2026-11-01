/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The keyboard and pointer a session publishes to its applications, when
//! `publish_input_devices` asks for it.
//!
//! These devices carry no input to the desktop: every key and pointer event reaches it
//! through the compositor's virtual keyboard or XTEST, and is copied here afterwards, for
//! the few applications that enumerate evdev instead of the display server (fullscreen
//! games, remappers). Where `/dev/uinput` is writable the kernel serves the device, so every
//! application finds it without a preload; otherwise the Input Interposer's dynamic pool
//! does: a socket `selkies_event<N>.sock` (N from 3000) carrying the event stream beside a
//! descriptor `selkies_event<N>.desc` a reader answers `EVIOCG*` from, and from which
//! fake-udev derives the device's `ID_INPUT_*` class. The descriptor is written before the
//! socket is bound, so a reader that sees the socket always finds the identity behind it,
//! and names this process's `struct input_event` size (descriptor version 2), so a 32-bit
//! reader parses a 64-bit server's stream. The descriptor layout is shared with the
//! interposer's `udyn_desc_t` and fake-udev, so the three change together.
//!
//! Two exceptions keep the kernel's copy off the session: host capture, where the session
//! belongs to another compositor that reads the kernel's own devices, publishes nothing
//! (`publish_wanted`); and an X server that reads kernel devices itself, as a desktop host's
//! Xorg does through udev hotplug (`display_reads_kernel_input`), gets the interposer pool,
//! else every event would arrive twice.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use log::{debug, info, warn};
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;

use crate::gamepad::abi::*;
use crate::gamepad::uinput::{
    ioctl_buf, ioctl_int, set_uinput_phys, uinput_sysname, uinput_writable,
};

/// The Input Interposer's dynamic-device pool, served as `/dev/input/event<N>`.
pub const UDYN_EVENT_BASE: u32 = 3000;
pub const UDYN_MAX: u32 = 16;
pub const UDYN_DESC_MAGIC: u32 = 0x4A44_4E55;
pub const UDYN_DESC_VERSION: u32 = 2;
/// The descriptor's size: identity, name, the six capability bitmaps, an alignment byte,
/// 64 `input_absinfo`, and the event size.
pub const UDYN_DESC_SIZE: usize =
    16 + UINPUT_MAX_NAME_SIZE + 4 + 96 + 2 + 8 + 1 + 4 + 1 + ABS_CNT * 24 + 4;

/// Events queued per reader of an interposed device before the oldest are dropped.
const READER_QUEUE: usize = 1024;

/// The evdev capability sets a device advertises.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub evbits: Vec<u16>,
    pub keybits: Vec<u16>,
    pub relbits: Vec<u16>,
    pub absbits: Vec<u16>,
}

/// Serialize one dynamic device's identity and capabilities for its descriptor. `EV_SYN` is
/// always advertised: every reader expects the `SYN_REPORT` that terminates a report.
pub fn pack_udyn_desc(
    name: &str,
    vendor: u16,
    product: u16,
    version: u16,
    caps: &Capabilities,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(UDYN_DESC_SIZE);
    out.extend_from_slice(&UDYN_DESC_MAGIC.to_ne_bytes());
    out.extend_from_slice(&UDYN_DESC_VERSION.to_ne_bytes());
    for v in [BUS_VIRTUAL, vendor, product, version] {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    let mut name_field = [0u8; UINPUT_MAX_NAME_SIZE];
    let name = truncate_name(name, UINPUT_MAX_NAME_SIZE - 1);
    name_field[..name.len()].copy_from_slice(name);
    out.extend_from_slice(&name_field);
    out.extend_from_slice(&evdev_bitmap(
        caps.evbits.iter().copied().chain([EV_SYN]),
        EV_MAX,
    ));
    out.extend_from_slice(&evdev_bitmap(caps.keybits.iter().copied(), KEY_MAX));
    out.extend_from_slice(&evdev_bitmap(caps.relbits.iter().copied(), REL_MAX));
    out.extend_from_slice(&evdev_bitmap(caps.absbits.iter().copied(), ABS_MAX));
    out.extend_from_slice(&evdev_bitmap([], MSC_MAX));
    out.extend_from_slice(&evdev_bitmap([], INPUT_PROP_MAX));
    out.push(0);
    out.resize(out.len() + ABS_CNT * 24, 0);
    out.extend_from_slice(&(LOCAL_INPUT_EVENT_SIZE as u32).to_ne_bytes());
    debug_assert_eq!(out.len(), UDYN_DESC_SIZE);
    out
}

/// A device the session publishes: its key in the table, name, product id, and capability
/// sets.
pub struct DeviceSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub product: u16,
    pub caps: Capabilities,
}

/// The Linux Foundation vendor id the published devices carry.
pub const PUBLISHED_VENDOR: u16 = 0x1D6B;

/// The keyboard and pointer a session publishes: every keyboard key, and the five pointer
/// buttons with both wheels.
pub fn published_device_specs() -> Vec<DeviceSpec> {
    vec![
        DeviceSpec {
            key: "keyboard",
            name: "Selkies Virtual Keyboard",
            product: 0x0001,
            caps: Capabilities {
                evbits: vec![EV_KEY],
                keybits: (1..BTN_MISC).collect(),
                ..Capabilities::default()
            },
        },
        DeviceSpec {
            key: "pointer",
            name: "Selkies Virtual Pointer",
            product: 0x0002,
            caps: Capabilities {
                evbits: vec![EV_KEY, EV_REL],
                keybits: vec![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA],
                relbits: vec![REL_X, REL_Y, REL_WHEEL, REL_HWHEEL],
                absbits: vec![],
            },
        },
    ]
}

/// Whether the session publishes its keyboard and pointer: only when asked
/// (`publish_input_devices`), and never under host capture (`wayland_host_display` set),
/// where the session belongs to a compositor reading the kernel's own devices.
pub fn publish_wanted(enabled: bool, wayland_host_display: &str) -> bool {
    enabled && wayland_host_display.trim().is_empty()
}

/// Whether the X server reads kernel input devices itself, as a desktop host's Xorg does
/// through udev hotplug: each one it opened is listed with its `Device Node` property. Xvfb
/// and the images' Xorg, hotplug off, list none.
pub fn display_reads_kernel_input<C: Connection>(conn: &C) -> bool {
    let probe = || -> Result<bool, Box<dyn std::error::Error>> {
        let node = conn.intern_atom(true, b"Device Node")?.reply()?.atom;
        if node == x11rb::NONE {
            return Ok(false);
        }
        let devices = conn
            .xinput_xi_query_device(u16::from(xinput::Device::ALL))?
            .reply()?
            .infos;
        for device in devices {
            let props = conn
                .xinput_xi_list_properties(device.deviceid)?
                .reply()?
                .properties;
            if props.contains(&node) {
                return Ok(true);
            }
        }
        Ok(false)
    };
    match probe() {
        Ok(reads) => reads,
        Err(e) => {
            debug!(target: "gamepad", "XInput device query failed: {e}");
            false
        }
    }
}

struct DevInner {
    kernel: Option<OwnedFd>,
    event_num: Option<u32>,
    node: Option<String>,
    readers: Vec<mpsc::Sender<Bytes>>,
    tasks: Vec<JoinHandle<()>>,
    cancel: CancellationToken,
}

/// One keyboard or pointer Selkies publishes to the session's applications.
pub struct VirtualInputDevice {
    name: String,
    phys: String,
    vendor: u16,
    product: u16,
    caps: Capabilities,
    sock_dir: PathBuf,
    inner: Mutex<DevInner>,
}

impl VirtualInputDevice {
    /// A device not yet opened; a kernel device reports `phys` as its physical path.
    pub fn new(
        name: &str,
        vendor: u16,
        product: u16,
        caps: Capabilities,
        sock_dir: &Path,
        phys: &str,
    ) -> Arc<VirtualInputDevice> {
        Arc::new(VirtualInputDevice {
            name: name.to_string(),
            phys: phys.to_string(),
            vendor,
            product,
            caps,
            sock_dir: sock_dir.to_path_buf(),
            inner: Mutex::new(DevInner {
                kernel: None,
                event_num: None,
                node: None,
                readers: Vec::new(),
                tasks: Vec::new(),
                cancel: CancellationToken::new(),
            }),
        })
    }

    /// A published device from its spec.
    pub fn from_spec(spec: &DeviceSpec, sock_dir: &Path) -> Arc<VirtualInputDevice> {
        VirtualInputDevice::new(
            spec.name,
            PUBLISHED_VENDOR,
            spec.product,
            spec.caps.clone(),
            sock_dir,
            &format!("selkies/virtinput/{}", spec.key),
        )
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `/dev/input` node applications open, once the device is up.
    pub fn node(&self) -> Option<String> {
        self.inner.lock().node.clone()
    }

    /// Whether the kernel serves the device.
    pub fn is_kernel(&self) -> bool {
        self.inner.lock().kernel.is_some()
    }

    /// Register the device with the kernel; false when uinput refuses it.
    fn create_kernel(&self) -> bool {
        let file = match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UINPUT_PATH)
        {
            Ok(file) => file,
            Err(_) => return false,
        };
        let fd: OwnedFd = file.into();
        let raw = fd.as_raw_fd();
        let setup = || -> std::io::Result<()> {
            for ev in self.caps.evbits.iter().copied().chain([EV_SYN]) {
                ioctl_int(raw, UI_SET_EVBIT, ev as i64)?;
            }
            for code in &self.caps.keybits {
                ioctl_int(raw, UI_SET_KEYBIT, *code as i64)?;
            }
            for code in &self.caps.relbits {
                ioctl_int(raw, UI_SET_RELBIT, *code as i64)?;
            }
            if !self.phys.is_empty() {
                set_uinput_phys(raw, &self.phys)?;
            }
            let mut buf =
                pack_uinput_setup(BUS_VIRTUAL, self.vendor, self.product, 1, &self.name, 0);
            ioctl_buf(raw, UI_DEV_SETUP, &mut buf)?;
            ioctl_int(raw, UI_DEV_CREATE, 0)
        };
        if let Err(e) = setup() {
            warn!(target: "gamepad", "{}: kernel uinput setup failed ({e}).", self.name);
            return false;
        }
        let node = uinput_sysname(raw)
            .and_then(|s| {
                crate::gamepad::uinput::device_nodes(Path::new(UINPUT_SYSFS_BASE), &s, false)
                    .into_iter()
                    .next()
            })
            .unwrap_or_else(|| UINPUT_PATH.to_string());
        let mut inner = self.inner.lock();
        inner.kernel = Some(fd);
        inner.node = Some(node);
        true
    }

    /// Whether a socket file is left over from a device nobody serves.
    fn stale(path: &Path) -> bool {
        std::os::unix::net::UnixStream::connect(path).is_err()
    }

    /// Claim a dynamic node: descriptor first, so a reader that sees the socket always finds
    /// the identity behind it.
    async fn create_interposed(self: &Arc<Self>) -> bool {
        let desc = pack_udyn_desc(&self.name, self.vendor, self.product, 1, &self.caps);
        if let Err(e) = std::fs::create_dir_all(&self.sock_dir) {
            warn!(target: "gamepad", "{}: cannot create {}: {e}", self.name, self.sock_dir.display());
            return false;
        }
        for num in UDYN_EVENT_BASE..UDYN_EVENT_BASE + UDYN_MAX {
            let sock = self.sock_dir.join(format!("selkies_event{num}.sock"));
            if sock.exists() && (!Self::stale(&sock) || std::fs::remove_file(&sock).is_err()) {
                continue;
            }
            let desc_path = self.sock_dir.join(format!("selkies_event{num}.desc"));
            let tmp = self.sock_dir.join(format!(
                "selkies_event{num}.desc.tmp.{}",
                std::process::id()
            ));
            let bound = std::fs::write(&tmp, &desc)
                .and_then(|_| std::fs::rename(&tmp, &desc_path))
                .and_then(|_| UnixListener::bind(&sock));
            let listener = match bound {
                Ok(listener) => listener,
                Err(e) => {
                    debug!(target: "gamepad", "{}: node {num} unavailable ({e}).", self.name);
                    let _ = std::fs::remove_file(&tmp);
                    continue;
                }
            };
            let mut inner = self.inner.lock();
            inner.event_num = Some(num);
            inner.node = Some(format!("/dev/input/event{num}"));
            let me = self.clone();
            inner.tasks.push(tokio::spawn(me.accept_loop(listener)));
            return true;
        }
        warn!(target: "gamepad", "{}: no free interposer node.", self.name);
        false
    }

    /// Hold each reader's connection open; the dynamic path carries events alone, the
    /// descriptor having already answered for the identity.
    async fn accept_loop(self: Arc<Self>, listener: UnixListener) {
        let cancel = self.inner.lock().cancel.clone();
        loop {
            let accepted = tokio::select! {
                _ = cancel.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, _)) = accepted else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            let (tx, mut rx) = mpsc::channel::<Bytes>(READER_QUEUE);
            self.inner.lock().readers.push(tx);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let (mut reader, mut writer) = stream.into_split();
                let mut buf = [0u8; 256];
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        read = reader.read(&mut buf) => {
                            if !matches!(read, Ok(n) if n > 0) { return; }
                        }
                        msg = rx.recv() => match msg {
                            Some(data) => {
                                if writer.write_all(&data).await.is_err() { return; }
                            }
                            None => return,
                        },
                    }
                }
            });
        }
    }

    /// Bring the device up on whichever backend this host offers; `kernel` off keeps it on
    /// the interposer pool where the kernel's copy would be read back by the display server.
    pub async fn open(self: &Arc<Self>, kernel: bool) -> bool {
        let ok = if kernel && uinput_writable() {
            let me = self.clone();
            tokio::task::spawn_blocking(move || me.create_kernel())
                .await
                .unwrap_or(false)
        } else {
            self.create_interposed().await
        };
        if ok {
            let inner = self.inner.lock();
            info!(target: "gamepad", "{} available at {} ({}).", self.name,
                inner.node.as_deref().unwrap_or("?"), if inner.kernel.is_some() { "kernel" } else { "interposer" });
        }
        ok
    }

    /// Deliver one event and the `SYN_REPORT` that closes its report.
    pub fn emit(&self, ev_type: u16, code: u16, value: i32) {
        let data = pack_evdev_report(ev_type, code, value, LOCAL_ARCH_BITS);
        let mut inner = self.inner.lock();
        if let Some(fd) = inner.kernel.as_ref() {
            let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
            if n < 0 {
                debug!(target: "gamepad", "{}: kernel write failed ({}).", self.name, std::io::Error::last_os_error());
            }
            return;
        }
        let data = Bytes::from(data);
        inner.readers.retain(|tx| {
            !matches!(
                tx.try_send(data.clone()),
                Err(mpsc::error::TrySendError::Closed(_))
            )
        });
    }

    /// Retire the device and remove everything it published.
    pub async fn close(&self) {
        let (tasks, event_num) = {
            let mut inner = self.inner.lock();
            if let Some(fd) = inner.kernel.take() {
                let _ = ioctl_int(fd.as_raw_fd(), UI_DEV_DESTROY, 0);
            }
            inner.cancel.cancel();
            inner.readers.clear();
            inner.node = None;
            (std::mem::take(&mut inner.tasks), inner.event_num.take())
        };
        for task in tasks {
            let _ = task.await;
        }
        if let Some(num) = event_num {
            for suffix in [".sock", ".desc"] {
                let _ =
                    std::fs::remove_file(self.sock_dir.join(format!("selkies_event{num}{suffix}")));
            }
        }
    }
}
