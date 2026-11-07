//! Kernel input devices for host-capture injection, over `/dev/uinput`.
//!
//! A host session takes its input from the kernel, so a uinput keyboard and
//! pointer reach it whatever display server it runs: no compositor protocol to
//! advertise, no portal on the bus, and no consent dialog to answer. That puts
//! it on the host-capture input ladder below libei and the virtual keyboard and
//! pointer protocols, which a compositor may not offer, and above the portal's
//! own methods (a socket write to the compositor lands in about 4 us, a uinput
//! write in about 12 us, a portal call in about 2 ms); it is taken only where
//! `/dev/uinput` is writable — a bare-metal host rather than an unprivileged
//! container.
//!
//! The pointer carries a normalized absolute axis rather than the layout's own
//! pixels: a uinput device fixes its axis range when the kernel creates it,
//! while the union layout changes whenever a display is added or resized.

use std::ffi::c_ulong;
use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, OwnedFd};

const IOC_WRITE: c_ulong = 1;
const UINPUT_NAME_LEN: usize = 80;

/// `_IOW('U', nr, size)`, the encoding `<linux/uinput.h>` gives its setup ioctls.
const fn iow(nr: c_ulong, size: c_ulong) -> c_ulong {
    (IOC_WRITE << 30) | (size << 16) | (b'U' as c_ulong) << 8 | nr
}
/// `_IO('U', nr)`, for the ioctls that carry no argument.
const fn io(nr: c_ulong) -> c_ulong {
    ((b'U' as c_ulong) << 8) | nr
}

const UI_DEV_CREATE: c_ulong = io(1);
const UI_DEV_DESTROY: c_ulong = io(2);
const UI_DEV_SETUP: c_ulong = iow(3, std::mem::size_of::<UinputSetup>() as c_ulong);
const UI_ABS_SETUP: c_ulong = iow(4, std::mem::size_of::<UinputAbsSetup>() as c_ulong);
const UI_SET_EVBIT: c_ulong = iow(100, 4);
const UI_SET_KEYBIT: c_ulong = iow(101, 4);
const UI_SET_RELBIT: c_ulong = iow(102, 4);
const UI_SET_ABSBIT: c_ulong = iow(103, 4);

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const BTN_MISC: u16 = 0x100;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
const BUS_VIRTUAL: u16 = 0x06;

/// The span an absolute axis is declared over; positions are scaled onto it.
pub const ABS_RANGE: i32 = 65535;

#[repr(C)]
#[derive(Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; UINPUT_NAME_LEN],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Default)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: AbsInfo,
}

#[repr(C)]
struct InputEvent {
    sec: i64,
    usec: i64,
    kind: u16,
    code: u16,
    value: i32,
}

/// Whether this process can create kernel input devices.
pub fn available() -> bool {
    OpenOptions::new().write(true).open("/dev/uinput").is_ok()
}

unsafe fn set(fd: i32, request: c_ulong, value: c_ulong) -> Result<(), String> {
    if unsafe { libc::ioctl(fd, request as _, value) } < 0 {
        return Err(format!(
            "uinput ioctl {request:#x}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// One kernel device: the fd that created it carries its events until it drops.
pub struct Device {
    fd: OwnedFd,
}

impl Device {
    fn open(
        name: &str,
        product: u16,
        build: impl Fn(i32) -> Result<(), String>,
    ) -> Result<Self, String> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .map_err(|e| format!("open /dev/uinput: {e}"))?;
        let fd: OwnedFd = file.into();
        let raw = fd.as_raw_fd();
        unsafe { set(raw, UI_SET_EVBIT, EV_SYN as c_ulong)? };
        build(raw)?;
        let mut setup = UinputSetup {
            id: InputId {
                bustype: BUS_VIRTUAL,
                vendor: 0x1d6b,
                product,
                version: 1,
            },
            name: [0; UINPUT_NAME_LEN],
            ff_effects_max: 0,
        };
        let bytes = name.as_bytes();
        let n = bytes.len().min(UINPUT_NAME_LEN - 1);
        setup.name[..n].copy_from_slice(&bytes[..n]);
        if unsafe { libc::ioctl(raw, UI_DEV_SETUP as _, &setup as *const _) } < 0 {
            return Err(format!("UI_DEV_SETUP: {}", std::io::Error::last_os_error()));
        }
        if unsafe { libc::ioctl(raw, UI_DEV_CREATE as _) } < 0 {
            return Err(format!(
                "UI_DEV_CREATE: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { fd })
    }

    /// A keyboard carrying the whole `KEY_*` block; the `BTN_*` codes above it
    /// are left out, or the device reads as a pad rather than a keyboard.
    pub fn keyboard() -> Result<Self, String> {
        Self::open("Selkies Virtual Keyboard", 0x0001, |fd| unsafe {
            set(fd, UI_SET_EVBIT, EV_KEY as c_ulong)?;
            for code in 1..BTN_MISC {
                set(fd, UI_SET_KEYBIT, code as c_ulong)?;
            }
            Ok(())
        })
    }

    /// A pointer with both motion kinds: absolute for a placed position,
    /// relative for a locked pointer whose delta must stay a delta.
    pub fn pointer() -> Result<Self, String> {
        Self::open("Selkies Virtual Pointer", 0x0002, |fd| unsafe {
            set(fd, UI_SET_EVBIT, EV_KEY as c_ulong)?;
            set(fd, UI_SET_EVBIT, EV_REL as c_ulong)?;
            set(fd, UI_SET_EVBIT, EV_ABS as c_ulong)?;
            for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
                set(fd, UI_SET_KEYBIT, code as c_ulong)?;
            }
            for code in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                set(fd, UI_SET_RELBIT, code as c_ulong)?;
            }
            for code in [ABS_X, ABS_Y] {
                set(fd, UI_SET_ABSBIT, code as c_ulong)?;
                let abs = UinputAbsSetup {
                    code,
                    absinfo: AbsInfo {
                        maximum: ABS_RANGE,
                        ..AbsInfo::default()
                    },
                };
                if libc::ioctl(fd, UI_ABS_SETUP as _, &abs as *const _) < 0 {
                    return Err(format!(
                        "UI_ABS_SETUP {code}: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            Ok(())
        })
    }

    /// Write one event and the `SYN_REPORT` that closes its report.
    pub fn emit(&self, kind: u16, code: u16, value: i32) -> Result<(), String> {
        let events = [
            InputEvent {
                sec: 0,
                usec: 0,
                kind,
                code,
                value,
            },
            InputEvent {
                sec: 0,
                usec: 0,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ];
        let bytes = unsafe {
            std::slice::from_raw_parts(events.as_ptr() as *const u8, std::mem::size_of_val(&events))
        };
        let written = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            )
        };
        if written < 0 {
            return Err(format!("uinput write: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY as _) };
    }
}

/// The keyboard and pointer one host session injects through.
pub struct Pair {
    pub keyboard: Device,
    pub pointer: Device,
}

impl Pair {
    /// Both devices, or `None` where the kernel cannot serve them, which is
    /// what sends injection down to the libei and portal rungs.
    pub fn open() -> Option<Self> {
        if !available() {
            return None;
        }
        match (Device::keyboard(), Device::pointer()) {
            (Ok(keyboard), Ok(pointer)) => Some(Self { keyboard, pointer }),
            (keyboard, pointer) => {
                for err in [keyboard.err(), pointer.err()].into_iter().flatten() {
                    eprintln!("[HostCapture] uinput unavailable: {err}");
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The events a device emits reach its consumer byte for byte.
    ///
    /// Needs the `/dev/uinput` emulator preloaded, which stands in for a kernel
    /// this machine has no node for, so it is not part of the default run:
    /// `LD_PRELOAD=<selkies>/tests/tools/uinput_shim.so UINPUT_SHIM_STREAM=/tmp/ui.bin
    /// cargo test --lib uinput -- --ignored`
    ///
    /// The emulator's ioctl log is a buffered `FILE*` this process cannot flush,
    /// so the assertion is on the event stream, which it writes straight through.
    #[test]
    #[ignore]
    fn emitted_events_reach_the_device_byte_for_byte() {
        let path = std::env::var("UINPUT_SHIM_STREAM").expect("UINPUT_SHIM_STREAM");
        let keyboard = Device::keyboard().expect("the emulator serves /dev/uinput");
        keyboard.emit(EV_KEY, 30, 1).unwrap();
        keyboard.emit(EV_KEY, 30, 0).unwrap();
        drop(keyboard);
        let raw = std::fs::read(&path).expect("event stream");
        let size = std::mem::size_of::<InputEvent>();
        let mut events: Vec<(u16, u16, i32)> = Vec::new();
        let mut at = 0;
        while at + size <= raw.len() {
            let c = &raw[at..at + size];
            events.push((
                u16::from_ne_bytes([c[16], c[17]]),
                u16::from_ne_bytes([c[18], c[19]]),
                i32::from_ne_bytes([c[20], c[21], c[22], c[23]]),
            ));
            at += size;
        }
        assert_eq!(
            events,
            vec![
                (EV_KEY, 30, 1),
                (EV_SYN, SYN_REPORT, 0),
                (EV_KEY, 30, 0),
                (EV_SYN, SYN_REPORT, 0),
            ],
            "stream was {events:?}"
        );
    }

    #[test]
    fn ioctls_and_structs_match_the_kernel_abi() {
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_RELBIT, 0x4004_5566);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
        assert_eq!(std::mem::offset_of!(UinputAbsSetup, absinfo), 4);
        assert_eq!(std::mem::offset_of!(UinputSetup, name), 8);
        assert_eq!(std::mem::offset_of!(UinputSetup, ff_effects_max), 88);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_ABS_SETUP, 0x401c_5504);
    }
}
