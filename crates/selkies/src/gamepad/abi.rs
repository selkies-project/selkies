/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The kernel input ABI the gamepads speak: evdev and joydev codes, the uinput ioctl
//! encodings, and the byte layouts of every struct crossing a socket or a device node.
//!
//! Everything here is computed from the definitions in `linux/input.h`, `linux/uinput.h`,
//! and `linux/joystick.h` rather than read from a binding, so a test compares each value
//! against what the kernel headers of the build host say (`tests/tools/uinput_abi_truth`).
//! The ioctl numbers follow the asm-generic encoding, which every architecture Selkies
//! builds for (x86_64, aarch64) uses. `struct input_event` carries a `timeval` of two
//! `long`s, so its size depends on the reader's word size: the interposer tells the server
//! its own with one byte after the handshake, and the writer here packs for that width.
//! `struct ff_effect` holds a pointer in its union, so its size and the two uinput
//! force-feedback request structs built on it depend on this process's word size alone.

use std::mem::size_of;
use std::time::{SystemTime, UNIX_EPOCH};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_FF: u16 = 0x15;
pub const SYN_REPORT: u16 = 0;

/// The force-feedback types and bits a memless pad (xpad) reports: rumble, the periodic
/// waveforms it plays as rumble, and gain.
pub const FF_RUMBLE: u16 = 0x50;
pub const FF_PERIODIC: u16 = 0x51;
pub const FF_SQUARE: u16 = 0x58;
pub const FF_TRIANGLE: u16 = 0x59;
pub const FF_SINE: u16 = 0x5a;
pub const FF_GAIN: u16 = 0x60;
pub const FF_MEMLESS_BITS: [u16; 6] = [
    FF_RUMBLE,
    FF_PERIODIC,
    FF_SQUARE,
    FF_TRIANGLE,
    FF_SINE,
    FF_GAIN,
];
pub const FF_EFFECTS_MAX: u32 = 16;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;

/// Gamepad buttons: A/B/X/Y alias SOUTH/EAST/NORTH/WEST; TL/TR are the bumpers,
/// SELECT/START/MODE back, start, and guide, THUMBL/THUMBR the stick clicks.
pub const BTN_A: u16 = 0x130;
pub const BTN_B: u16 = 0x131;
pub const BTN_X: u16 = 0x133;
pub const BTN_Y: u16 = 0x134;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBL: u16 = 0x13d;
pub const BTN_THUMBR: u16 = 0x13e;

/// Absolute axes; Z and RZ carry the triggers, HAT0X/HAT0Y the D-pad.
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;

/// The first non-keyboard key code: everything below it is a keyboard key.
pub const BTN_MISC: u16 = 0x100;

pub const EV_MAX: usize = 31;
pub const KEY_MAX: usize = 767;
pub const REL_MAX: usize = 15;
pub const ABS_MAX: usize = 63;
pub const MSC_MAX: usize = 7;
pub const INPUT_PROP_MAX: usize = 31;
pub const ABS_CNT: usize = 64;

pub const BUS_USB: u16 = 0x03;
pub const BUS_VIRTUAL: u16 = 0x06;

/// JS event types from `linux/joystick.h`.
pub const JS_EVENT_BUTTON: u8 = 0x01;
pub const JS_EVENT_AXIS: u8 = 0x02;
pub const JS_EVENT_INIT: u8 = 0x80;

/// The asm-generic ioctl field layout.
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const IOC_TYPESHIFT: u64 = 8;
const IOC_SIZESHIFT: u64 = 16;
const IOC_DIRSHIFT: u64 = 30;

/// Encode a uinput (`'U'`) ioctl request number.
const fn uinput_ioc(direction: u64, number: u64, size: usize) -> u64 {
    (direction << IOC_DIRSHIFT)
        | ((b'U' as u64) << IOC_TYPESHIFT)
        | number
        | ((size as u64) << IOC_SIZESHIFT)
}

pub const UINPUT_PATH: &str = "/dev/uinput";
pub const UINPUT_SYSFS_BASE: &str = "/sys/devices/virtual/input";
pub const UINPUT_MAX_NAME_SIZE: usize = 80;
pub const UINPUT_SYSNAME_LEN: usize = 64;

/// `struct uinput_setup { struct input_id id; char name[80]; __u32 ff_effects_max; }`.
pub const UINPUT_SETUP_SIZE: usize = 8 + UINPUT_MAX_NAME_SIZE + 4;
/// `struct uinput_abs_setup { __u16 code; struct input_absinfo absinfo; }`, the absinfo
/// aligned to four bytes.
pub const UINPUT_ABS_SETUP_SIZE: usize = 4 + 6 * 4;
/// `struct ff_effect`: type, id, direction, trigger, replay (14 bytes), then the effect
/// union aligned to the pointer in `ff_periodic_effect`: 32 bytes on a 64-bit ABI, 28 on a
/// 32-bit one.
pub const FF_EFFECT_HEAD_SIZE: usize = 16;
pub const FF_EFFECT_SIZE: usize = FF_EFFECT_HEAD_SIZE + 24 + size_of::<usize>();
/// `struct uinput_ff_upload { __u32 request_id; __s32 retval; struct ff_effect effect, old; }`.
pub const UINPUT_FF_UPLOAD_SIZE: usize = 8 + 2 * FF_EFFECT_SIZE;
/// `struct uinput_ff_erase { __u32 request_id; __s32 retval; __u32 effect_id; }`.
pub const UINPUT_FF_ERASE_SIZE: usize = 12;
/// The uinput device's own events: an application uploaded or erased an effect.
pub const EV_UINPUT: u16 = 0x0101;
pub const UI_FF_UPLOAD: u16 = 1;
pub const UI_FF_ERASE: u16 = 2;

pub const UI_DEV_CREATE: u64 = uinput_ioc(0, 1, 0);
pub const UI_DEV_DESTROY: u64 = uinput_ioc(0, 2, 0);
pub const UI_DEV_SETUP: u64 = uinput_ioc(IOC_WRITE, 3, UINPUT_SETUP_SIZE);
pub const UI_ABS_SETUP: u64 = uinput_ioc(IOC_WRITE, 4, UINPUT_ABS_SETUP_SIZE);
pub const UI_SET_EVBIT: u64 = uinput_ioc(IOC_WRITE, 100, 4);
pub const UI_SET_KEYBIT: u64 = uinput_ioc(IOC_WRITE, 101, 4);
pub const UI_SET_RELBIT: u64 = uinput_ioc(IOC_WRITE, 102, 4);
pub const UI_SET_ABSBIT: u64 = uinput_ioc(IOC_WRITE, 103, 4);
pub const UI_SET_FFBIT: u64 = uinput_ioc(IOC_WRITE, 107, 4);
/// `_IOW('U', 108, char *)`: the argument is the string's address.
pub const UI_SET_PHYS: u64 = uinput_ioc(IOC_WRITE, 108, size_of::<usize>());
pub const UI_GET_SYSNAME: u64 = uinput_ioc(IOC_READ, 44, UINPUT_SYSNAME_LEN);
pub const UI_BEGIN_FF_UPLOAD: u64 = uinput_ioc(IOC_READ | IOC_WRITE, 200, UINPUT_FF_UPLOAD_SIZE);
pub const UI_END_FF_UPLOAD: u64 = uinput_ioc(IOC_WRITE, 201, UINPUT_FF_UPLOAD_SIZE);
pub const UI_BEGIN_FF_ERASE: u64 = uinput_ioc(IOC_READ | IOC_WRITE, 202, UINPUT_FF_ERASE_SIZE);
pub const UI_END_FF_ERASE: u64 = uinput_ioc(IOC_WRITE, 203, UINPUT_FF_ERASE_SIZE);

/// This process's word size, which fixes its own `struct input_event`.
pub const LOCAL_ARCH_BITS: u8 = usize::BITS as u8;

/// `struct js_event { __u32 time; __s16 value; __u8 type; __u8 number; }`.
pub const JS_EVENT_SIZE: usize = 8;

/// A `struct input_event` of the given word size: 24 bytes on 64-bit, 16 on 32-bit.
pub const fn input_event_size(arch_bits: u8) -> usize {
    if arch_bits == 64 { 24 } else { 16 }
}

/// A `struct input_event` in this process's own ABI.
pub const LOCAL_INPUT_EVENT_SIZE: usize = input_event_size(LOCAL_ARCH_BITS);

/// Pack a `struct js_event`.
///
/// `type` is `__u8`, so the `JS_EVENT_INIT` flag OR'd into it stays representable; the
/// millisecond timestamp is masked to fit the `__u32`.
pub fn pack_js_event(ev_type: u8, number: u8, value: i16) -> [u8; JS_EVENT_SIZE] {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let ts = (ms & 0xffff_ffff) as u32;
    let mut out = [0u8; JS_EVENT_SIZE];
    out[0..4].copy_from_slice(&ts.to_ne_bytes());
    out[4..6].copy_from_slice(&value.to_ne_bytes());
    out[6] = ev_type;
    out[7] = number;
    out
}

/// Unpack a `struct js_event` into `(type, number, value)`.
pub fn unpack_js_event(buf: &[u8]) -> Option<(u8, u8, i16)> {
    if buf.len() < JS_EVENT_SIZE {
        return None;
    }
    let value = i16::from_ne_bytes([buf[4], buf[5]]);
    Some((buf[6], buf[7], value))
}

/// Pack one `struct input_event` of the reader's word size, stamped `now`.
fn pack_input_event_at(
    out: &mut Vec<u8>,
    sec: i64,
    usec: i64,
    ev_type: u16,
    code: u16,
    value: i32,
    arch_bits: u8,
) {
    if arch_bits == 64 {
        out.extend_from_slice(&sec.to_ne_bytes());
        out.extend_from_slice(&usec.to_ne_bytes());
    } else {
        out.extend_from_slice(&(sec as i32).to_ne_bytes());
        out.extend_from_slice(&(usec as i32).to_ne_bytes());
    }
    out.extend_from_slice(&ev_type.to_ne_bytes());
    out.extend_from_slice(&code.to_ne_bytes());
    out.extend_from_slice(&value.to_ne_bytes());
}

/// Pack an `input_event` and the `SYN_REPORT` that closes its report, in the reader's word
/// size: `struct timeval`'s members are `long`, 8 bytes on a 64-bit reader and 4 on a 32-bit
/// one, which is what the interposer's architecture byte says.
pub fn pack_evdev_report(ev_type: u16, code: u16, value: i32, arch_bits: u8) -> Vec<u8> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (sec, usec) = (now.as_secs() as i64, now.subsec_micros() as i64);
    let mut out = Vec::with_capacity(2 * input_event_size(arch_bits));
    pack_input_event_at(&mut out, sec, usec, ev_type, code, value, arch_bits);
    pack_input_event_at(&mut out, sec, usec, EV_SYN, SYN_REPORT, 0, arch_bits);
    out
}

/// Unpack one `struct input_event` of this process's ABI into `(type, code, value)`.
pub fn unpack_local_input_event(buf: &[u8]) -> Option<(u16, u16, i32)> {
    if buf.len() < LOCAL_INPUT_EVENT_SIZE {
        return None;
    }
    let off = LOCAL_INPUT_EVENT_SIZE - 8;
    let ev_type = u16::from_ne_bytes([buf[off], buf[off + 1]]);
    let code = u16::from_ne_bytes([buf[off + 2], buf[off + 3]]);
    let value = i32::from_ne_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
    Some((ev_type, code, value))
}

/// Pack a `struct uinput_setup`: the bus and identity, the name truncated to fit its
/// NUL-terminated field, and the number of force-feedback effects the device holds.
pub fn pack_uinput_setup(
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: &str,
    ff_effects_max: u32,
) -> [u8; UINPUT_SETUP_SIZE] {
    let mut out = [0u8; UINPUT_SETUP_SIZE];
    out[0..2].copy_from_slice(&bustype.to_ne_bytes());
    out[2..4].copy_from_slice(&vendor.to_ne_bytes());
    out[4..6].copy_from_slice(&product.to_ne_bytes());
    out[6..8].copy_from_slice(&version.to_ne_bytes());
    let name = truncate_name(name, UINPUT_MAX_NAME_SIZE - 1);
    out[8..8 + name.len()].copy_from_slice(name);
    out[88..92].copy_from_slice(&ff_effects_max.to_ne_bytes());
    out
}

/// The leading bytes of `name` that fit `max` bytes without splitting a UTF-8 sequence.
pub fn truncate_name(name: &str, max: usize) -> &[u8] {
    let mut end = name.len().min(max);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name.as_bytes()[..end]
}

/// `struct input_absinfo` less its current value: the range, fuzz, flat, and resolution an
/// axis declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsInfo {
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// Pack a `struct uinput_abs_setup` for `code` at rest.
pub fn pack_uinput_abs_setup(code: u16, info: AbsInfo) -> [u8; UINPUT_ABS_SETUP_SIZE] {
    let mut out = [0u8; UINPUT_ABS_SETUP_SIZE];
    out[0..2].copy_from_slice(&code.to_ne_bytes());
    for (i, v) in [
        0,
        info.minimum,
        info.maximum,
        info.fuzz,
        info.flat,
        info.resolution,
    ]
    .iter()
    .enumerate()
    {
        let at = 4 + 4 * i;
        out[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    }
    out
}

/// The evdev capability bitmap `codes` set, sized as a descriptor declares it
/// (`maximum / 8 + 1` bytes).
pub fn evdev_bitmap(codes: impl IntoIterator<Item = u16>, maximum: usize) -> Vec<u8> {
    let mut buf = vec![0u8; maximum / 8 + 1];
    for code in codes {
        let code = code as usize;
        if code <= maximum {
            buf[code / 8] |= 1 << (code % 8);
        }
    }
    buf
}

/// Read a little-endian-agnostic native `u16`/`i16`/`u32`/`i32` out of a buffer.
pub fn read_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([buf[at], buf[at + 1]])
}

pub fn read_i16(buf: &[u8], at: usize) -> i16 {
    i16::from_ne_bytes([buf[at], buf[at + 1]])
}

pub fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

pub fn write_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

pub fn write_i32(buf: &mut [u8], at: usize, value: i32) {
    buf[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}
