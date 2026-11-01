/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The kernel backend: a slot's gamepad as a uinput device, and the policy deciding whether
//! a session gets one.
//!
//! `uinput_gamepad` resolves to a decision once per process (`uinput_gamepads_enabled`):
//! `auto` makes kernel gamepads the fallback for hosts that do not preload the Input
//! Interposer, staying off wherever the interposer is set up (`SELKIES_INTERPOSER`, or the
//! library in `LD_PRELOAD`) so no application sees the same pad through both backends at
//! once, and off where `/dev/uinput` cannot be opened for writing; `true` attempts it
//! regardless and `false` never does. The probe opens the node `O_WRONLY | O_NONBLOCK`, so a
//! node a blocking open would never return from reads as unavailable rather than hanging
//! startup.
//!
//! A [`UInputGamepad`] is the same Xbox pad the interposer presents, force feedback
//! included: the kernel hands an application's effect uploads and erasures to the device's
//! owner to answer (`UI_BEGIN_FF_UPLOAD` and its siblings), and its plays arrive as `EV_FF`
//! events; `service_ff` answers each request as it is read, keeping a rumble effect's two
//! magnitudes and driving both motors at a periodic effect's own, scaled from 0x7fff to
//! 0xffff as a memless pad plays them, and refusing every other type as such a pad does.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use log::{debug, error, info, warn};

use crate::gamepad::abi::*;
use crate::gamepad::ff::FfRecord;
use crate::gamepad::mapper::{
    AXES_MAP, BTN_MAP, PAD_NAME, PAD_PRODUCT, PAD_VENDOR, PAD_VERSION, abs_info,
};

/// Whether this process can create kernel input devices through `path`.
pub fn uinput_writable_at(path: &Path) -> bool {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .is_ok()
}

/// Whether this process can create kernel input devices.
pub fn uinput_writable() -> bool {
    uinput_writable_at(Path::new(UINPUT_PATH))
}

/// Whether this session preloads the Input Interposer into applications, which already
/// delivers gamepad events without any kernel device.
pub fn interposer_configured() -> bool {
    interposer_configured_by(
        std::env::var("SELKIES_INTERPOSER").ok().as_deref(),
        std::env::var("LD_PRELOAD").ok().as_deref(),
    )
}

/// `interposer_configured` over the two variables it reads: `SELKIES_INTERPOSER` set to
/// anything, or the library's name in `LD_PRELOAD`.
pub fn interposer_configured_by(
    selkies_interposer: Option<&str>,
    ld_preload: Option<&str>,
) -> bool {
    selkies_interposer.is_some_and(|v| !v.is_empty())
        || ld_preload.is_some_and(|v| v.contains("selkies_input_interposer"))
}

/// Resolve the `uinput_gamepad` setting against the host, logging why.
pub fn uinput_gamepads_enabled(mode: &str) -> bool {
    decide_uinput(mode, interposer_configured, uinput_writable)
}

/// The decision behind `uinput_gamepads_enabled`, over the two probes it needs.
pub fn decide_uinput(
    mode: &str,
    interposer: impl FnOnce() -> bool,
    writable: impl FnOnce() -> bool,
) -> bool {
    let mode = mode.trim().to_ascii_lowercase();
    let mode = if mode.is_empty() {
        "auto".to_string()
    } else {
        mode
    };
    if matches!(mode.as_str(), "false" | "0" | "no" | "off") {
        return false;
    }
    let forced = matches!(mode.as_str(), "true" | "1" | "yes" | "on");
    if !forced && mode != "auto" {
        warn!(target: "gamepad", "Unrecognized uinput_gamepad value '{mode}'; using 'auto'.");
    }
    if !forced && interposer() {
        debug!(target: "gamepad", "Input Interposer is configured for this session; kernel gamepads stay off.");
        return false;
    }
    if !writable() {
        let text = format!(
            "{UINPUT_PATH} is missing or not writable, so gamepads reach applications only through the Input \
             Interposer. Load the uinput module and grant this user write access to enable kernel gamepads."
        );
        if forced {
            error!(target: "gamepad", "{text}");
        } else {
            info!(target: "gamepad", "{text}");
        }
        return false;
    }
    true
}

/// The physical path a slot's pad reports, the interposer's `EVIOCGPHYS` answer on either
/// backend. With the virtual keyboard and pointer's `selkies/virtinput/...`, it is what a
/// host's udev rule keys on to keep kernel devices a container registers off the host's own
/// seat.
pub fn pad_phys(slot: usize) -> String {
    format!("virtual/input/selkies_ev{slot}/phys")
}

/// Issue one ioctl on `fd` with an integer argument.
pub(crate) fn ioctl_int(fd: RawFd, request: u64, value: i64) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, value as libc::c_long) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// An ioctl whose argument is a buffer the kernel reads or fills.
pub type BufIoctl = Box<dyn FnMut(RawFd, u64, &mut [u8]) -> io::Result<()> + Send>;

/// Issue one ioctl on `fd` whose argument is a buffer the kernel reads or fills.
pub(crate) fn ioctl_buf(fd: RawFd, request: u64, buf: &mut [u8]) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, buf.as_mut_ptr()) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Name a uinput device's physical path, before `UI_DEV_CREATE`.
pub(crate) fn set_uinput_phys(fd: RawFd, phys: &str) -> io::Result<()> {
    let bytes: Vec<u8> = phys.bytes().filter(|b| *b != 0).take(1023).collect();
    let text = CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let rc = unsafe { libc::ioctl(fd, UI_SET_PHYS as _, text.as_ptr()) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The sysfs name the kernel gave a uinput device, once created.
pub(crate) fn uinput_sysname(fd: RawFd) -> Option<String> {
    let mut buf = [0u8; UINPUT_SYSNAME_LEN];
    ioctl_buf(fd, UI_GET_SYSNAME, &mut buf).ok()?;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    (!name.is_empty()).then_some(name)
}

/// The `/dev/input` nodes registered for the device named `sysname` under `sysfs_base`:
/// `event*` always, `js*` as well when joydev is loaded; sorted.
pub(crate) fn device_nodes(sysfs_base: &Path, sysname: &str, with_js: bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sysfs_base.join(sysname)) else {
        return Vec::new();
    };
    let mut nodes: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("event") || (with_js && n.starts_with("js")))
        .map(|n| format!("/dev/input/{n}"))
        .collect();
    nodes.sort();
    nodes
}

/// One slot's kernel gamepad, created through `/dev/uinput`.
pub struct UInputGamepad {
    label: String,
    phys: String,
    sysfs_base: std::path::PathBuf,
    /// Shared with the force-feedback watch, which registers it with the reactor.
    fd: Option<Arc<OwnedFd>>,
    /// How the force-feedback requests are answered: the kernel's ioctls, or a stand-in a
    /// test answers them with.
    ff_ioctl: BufIoctl,
    /// The nodes the kernel registered, resolved at creation.
    pub device_nodes: Vec<String>,
    /// Effect id to `(strong, weak, length ms, delay ms)`, the magnitudes as the interposer
    /// resolves them.
    effects: HashMap<i16, (u16, u16, u16, u16)>,
}

impl UInputGamepad {
    /// A device not yet created; `label` names it in the log and `phys` is what it reports
    /// as its physical path.
    pub fn new(label: &str, phys: &str) -> UInputGamepad {
        UInputGamepad {
            label: label.to_string(),
            phys: phys.to_string(),
            sysfs_base: Path::new(UINPUT_SYSFS_BASE).to_path_buf(),
            fd: None,
            ff_ioctl: Box::new(ioctl_buf),
            device_nodes: Vec::new(),
            effects: HashMap::new(),
        }
    }

    /// Resolve the device's nodes under another sysfs root (tests).
    pub fn with_sysfs_base(mut self, base: &Path) -> UInputGamepad {
        self.sysfs_base = base.to_path_buf();
        self
    }

    /// Adopt an already open descriptor as the device, its requests answered by `ff_ioctl`
    /// (tests drive one end of a socketpair and answer as the kernel would).
    pub fn with_fd(mut self, fd: OwnedFd, ff_ioctl: BufIoctl) -> UInputGamepad {
        self.fd = Some(Arc::new(fd));
        self.ff_ioctl = ff_ioctl;
        self
    }

    /// The device's descriptor, once created.
    pub fn fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(|f| f.as_raw_fd())
    }

    /// The descriptor shared for watching; it stays open while any holder lives.
    pub fn fd_handle(&self) -> Option<Arc<OwnedFd>> {
        self.fd.clone()
    }

    /// The effects applications have uploaded, by id.
    pub fn effects(&self) -> &HashMap<i16, (u16, u16, u16, u16)> {
        &self.effects
    }

    /// Register the kernel device and resolve its `/dev/input` node paths.
    ///
    /// The setup ioctls declare `EV_KEY`, `EV_ABS`, and `EV_FF`, the memless force-feedback
    /// bits, every button and axis of the pad with the ranges the interposer answers, the
    /// physical path, and the identity, then create the device. A failure closes the
    /// descriptor and returns the error.
    pub fn create(&mut self) -> io::Result<Vec<String>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UINPUT_PATH)?;
        let fd: OwnedFd = file.into();
        let raw = fd.as_raw_fd();
        let setup = || -> io::Result<()> {
            for ev in [EV_KEY, EV_ABS, EV_FF] {
                ioctl_int(raw, UI_SET_EVBIT, ev as i64)?;
            }
            for code in FF_MEMLESS_BITS {
                ioctl_int(raw, UI_SET_FFBIT, code as i64)?;
            }
            for code in BTN_MAP {
                ioctl_int(raw, UI_SET_KEYBIT, code as i64)?;
            }
            for code in AXES_MAP {
                ioctl_int(raw, UI_SET_ABSBIT, code as i64)?;
                let mut buf = pack_uinput_abs_setup(code, abs_info(code));
                ioctl_buf(raw, UI_ABS_SETUP, &mut buf)?;
            }
            if !self.phys.is_empty() {
                set_uinput_phys(raw, &self.phys)?;
            }
            let mut buf = pack_uinput_setup(
                BUS_USB,
                PAD_VENDOR,
                PAD_PRODUCT,
                PAD_VERSION,
                PAD_NAME,
                FF_EFFECTS_MAX,
            );
            ioctl_buf(raw, UI_DEV_SETUP, &mut buf)?;
            ioctl_int(raw, UI_DEV_CREATE, 0)
        };
        setup()?;
        self.device_nodes = uinput_sysname(raw)
            .map(|s| device_nodes(&self.sysfs_base, &s, true))
            .unwrap_or_default();
        self.fd = Some(Arc::new(fd));
        Ok(self.device_nodes.clone())
    }

    /// Write one event and its `SYN_REPORT` to the device.
    pub fn emit(&self, ev_type: u16, code: u16, value: i32) -> io::Result<()> {
        let Some(fd) = self.fd() else {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        };
        let data = pack_evdev_report(ev_type, code, value, LOCAL_ARCH_BITS);
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Answer the effect uploads and erasures queued on the device, and return what
    /// applications played, stopped, and set the gain to since.
    ///
    /// The kernel holds an application's `EVIOCSFF` or `EVIOCRMFF` until this device's owner
    /// ends the request, so every one is answered as it is read.
    pub fn service_ff(&mut self) -> io::Result<Vec<FfRecord>> {
        let mut out = Vec::new();
        let Some(fd) = self.fd() else {
            return Ok(out);
        };
        let mut buf = vec![0u8; LOCAL_INPUT_EVENT_SIZE * 64];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => break,
                    _ => return Err(err),
                }
            }
            if n == 0 {
                break;
            }
            for chunk in buf[..n as usize].as_chunks::<LOCAL_INPUT_EVENT_SIZE>().0 {
                let Some((etype, code, value)) = unpack_local_input_event(chunk) else {
                    continue;
                };
                if etype == EV_UINPUT && code == UI_FF_UPLOAD {
                    self.answer_upload(value as u32)?;
                } else if etype == EV_UINPUT && code == UI_FF_ERASE {
                    if let Some(effect_id) = self.answer_erase(value as u32)? {
                        out.push(FfRecord::stop(effect_id));
                    }
                } else if etype == EV_FF && code == FF_GAIN {
                    out.push(FfRecord::gain(value.clamp(0, 0xffff) as u16));
                } else if etype == EV_FF {
                    let effect_id = code as i16;
                    if let Some(&(strong, weak, length, delay)) = self.effects.get(&effect_id) {
                        if value > 0 {
                            out.push(FfRecord::play(
                                effect_id,
                                strong,
                                weak,
                                length,
                                delay,
                                value.min(0xffff) as u16,
                            ));
                        } else {
                            out.push(FfRecord::stop(effect_id));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// End one upload request: keep a rumble or periodic effect, refuse anything else.
    fn answer_upload(&mut self, request_id: u32) -> io::Result<()> {
        let fd = self.fd().ok_or(io::ErrorKind::NotConnected)?;
        let mut buf = vec![0u8; UINPUT_FF_UPLOAD_SIZE];
        write_u32(&mut buf, 0, request_id);
        (self.ff_ioctl)(fd, UI_BEGIN_FF_UPLOAD, &mut buf)?;
        let effect = 8;
        let etype = read_u16(&buf, effect);
        let effect_id = read_i16(&buf, effect + 2);
        let length = read_u16(&buf, effect + 10);
        let delay = read_u16(&buf, effect + 12);
        let union = effect + FF_EFFECT_HEAD_SIZE;
        let kept = if etype == FF_RUMBLE {
            Some((read_u16(&buf, union), read_u16(&buf, union + 2)))
        } else if etype == FF_PERIODIC {
            let magnitude = (read_i16(&buf, union + 4) as i32).unsigned_abs() * 2;
            let level = magnitude.min(0xffff) as u16;
            Some((level, level))
        } else {
            None
        };
        let retval = match kept {
            Some((strong, weak)) => {
                self.effects
                    .insert(effect_id, (strong, weak, length, delay));
                0
            }
            None => -libc::EINVAL,
        };
        write_i32(&mut buf, 4, retval);
        (self.ff_ioctl)(fd, UI_END_FF_UPLOAD, &mut buf)
    }

    /// End one erase request; the id of the effect it forgets, if the device held it.
    fn answer_erase(&mut self, request_id: u32) -> io::Result<Option<i16>> {
        let fd = self.fd().ok_or(io::ErrorKind::NotConnected)?;
        let mut buf = vec![0u8; UINPUT_FF_ERASE_SIZE];
        write_u32(&mut buf, 0, request_id);
        (self.ff_ioctl)(fd, UI_BEGIN_FF_ERASE, &mut buf)?;
        let effect_id = read_u32(&buf, 8) as i16;
        let known = self.effects.remove(&effect_id).is_some();
        write_i32(&mut buf, 4, 0);
        (self.ff_ioctl)(fd, UI_END_FF_ERASE, &mut buf)?;
        Ok(known.then_some(effect_id))
    }

    /// Unregister the kernel device and drop the descriptor; it closes once the watch has
    /// let go of it too.
    pub fn destroy(&mut self) {
        let Some(fd) = self.fd.take() else {
            return;
        };
        if let Err(e) = ioctl_int(fd.as_raw_fd(), UI_DEV_DESTROY, 0) {
            warn!(target: "gamepad", "Gamepad {}: could not destroy the kernel device: {e}", self.label);
        }
        drop(fd);
        self.device_nodes.clear();
    }

    /// Forget the descriptor without unregistering anything (a stand-in device in tests).
    pub fn forget_fd(&mut self) {
        self.fd = None;
    }
}

impl Drop for UInputGamepad {
    fn drop(&mut self) {
        self.destroy();
    }
}
