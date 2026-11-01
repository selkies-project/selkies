/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The fixed Xbox 360 pad every slot presents, and the mapping of a browser Gamepad API
//! control onto it.
//!
//! Whatever the client's own controller is, applications see a 'Microsoft X-Box 360 pad'
//! with eleven buttons and eight axes, so one udev identity, one evdev capability set, and
//! one joydev layout serve every client, and the identity the Input Interposer and
//! fake-udev hard-code agrees with the kernel device. A client control change maps onto one
//! internal control: a button onto a key, the triggers (client buttons 6 and 7) and the
//! D-pad (buttons 12 to 15) onto axes. Each mapped change is packed both ways, as the
//! joydev event the js sockets carry and as the evdev event the evdev sockets and the
//! kernel device carry, since one client event feeds both streams.

use crate::gamepad::abi::*;

/// The pad's identity, the one the interposer and fake-udev publish.
pub const PAD_NAME: &str = "Microsoft X-Box 360 pad";
pub const PAD_VENDOR: u16 = 0x045e;
pub const PAD_PRODUCT: u16 = 0x028e;
pub const PAD_VERSION: u16 = 0x0114;

/// The evdev key code of each internal button index: 0 A, 1 B, 2 X, 3 Y, 4 LB, 5 RB,
/// 6 Back, 7 Start, 8 Guide, 9 left stick click, 10 right stick click.
pub const BTN_MAP: [u16; 11] = [
    BTN_A, BTN_B, BTN_X, BTN_Y, BTN_TL, BTN_TR, BTN_SELECT, BTN_START, BTN_MODE, BTN_THUMBL,
    BTN_THUMBR,
];

/// The evdev axis code of each internal axis index: 0 LS X, 1 LS Y, 2 LT, 3 RS X, 4 RS Y,
/// 5 RT, 6 D-pad X, 7 D-pad Y.
pub const AXES_MAP: [u16; 8] = [
    ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y,
];

/// Internal axis indices that are triggers, and those that are D-pad hats.
pub const TRIGGER_AXES: [u8; 2] = [2, 5];
pub const HAT_AXES: [u8; 2] = [6, 7];

pub const ABS_MIN_VAL: i32 = -32767;
pub const ABS_MAX_VAL: i32 = 32767;
pub const ABS_HAT_MIN_VAL: i32 = -1;
pub const ABS_HAT_MAX_VAL: i32 = 1;

/// The range each axis declares, matching the interposer's `EVIOCGABS` answer so an
/// application cannot tell the two backends apart.
pub const ABS_INFO_DEFAULT: AbsInfo = AbsInfo {
    minimum: ABS_MIN_VAL,
    maximum: ABS_MAX_VAL,
    fuzz: 16,
    flat: 128,
    resolution: 1,
};
pub const ABS_INFO_HAT: AbsInfo = AbsInfo {
    minimum: ABS_HAT_MIN_VAL,
    maximum: ABS_HAT_MAX_VAL,
    fuzz: 0,
    flat: 0,
    resolution: 0,
};

/// The range `code` declares to uinput.
pub fn abs_info(code: u16) -> AbsInfo {
    if code == ABS_HAT0X || code == ABS_HAT0Y {
        ABS_INFO_HAT
    } else {
        ABS_INFO_DEFAULT
    }
}

/// Client (browser Gamepad API) button index to internal button index: A B X Y LB RB are
/// 0 to 5, Back 8, Start 9, the stick presses 10 and 11, Guide 16.
fn button_index(client: u32) -> Option<u8> {
    Some(match client {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 5,
        8 => 6,
        9 => 7,
        10 => 9,
        11 => 10,
        16 => 8,
        _ => return None,
    })
}

/// Client axis to internal axis index: left stick 0/1, right stick 2/3.
fn axis_index(client: u32) -> Option<u8> {
    Some(match client {
        0 => 0,
        1 => 1,
        2 => 3,
        3 => 4,
        _ => return None,
    })
}

/// Client buttons 6 (LT) and 7 (RT) drive the trigger axes.
fn trigger_axis(client: u32) -> Option<u8> {
    match client {
        6 => Some(2),
        7 => Some(5),
        _ => None,
    }
}

/// Client D-pad buttons 12 to 15 (up, down, left, right) to `(hat axis, direction)`.
fn hat_of(client: u32) -> Option<(u8, i32)> {
    match client {
        12 => Some((7, -1)),
        13 => Some((7, 1)),
        14 => Some((6, -1)),
        15 => Some((6, 1)),
        _ => None,
    }
}

/// Normalize a client axis value into the evdev or joydev range.
///
/// Sticks run -1.0 to 1.0, triggers 0.0 to 1.0, hats -1, 0, or 1. Triggers map onto the
/// full stick range rather than 0 to 255: joydev and evdev consumers treat them as ordinary
/// analog axes. A hat is -1/0/1 on evdev and full scale on joydev (`for_js`). The
/// conversions truncate as the Python backend's `int()` did, and a hat rounds half to even
/// as its `round()` did.
pub fn normalize_axis_value(
    client_value: f64,
    is_trigger: bool,
    is_hat: bool,
    for_js: bool,
) -> i32 {
    if is_hat {
        let hat = (client_value.round_ties_even() as i32).clamp(ABS_HAT_MIN_VAL, ABS_HAT_MAX_VAL);
        return if for_js { hat * ABS_MAX_VAL } else { hat };
    }
    let span = (ABS_MAX_VAL - ABS_MIN_VAL) as f64;
    if is_trigger {
        return (ABS_MIN_VAL as f64 + client_value * span) as i32;
    }
    (ABS_MIN_VAL as f64 + ((client_value + 1.0) / 2.0) * span) as i32
}

/// One client control change mapped onto the pad: the joydev event and the evdev event it
/// becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedEvent {
    /// `JS_EVENT_BUTTON` or `JS_EVENT_AXIS`.
    pub js_type: u8,
    /// The internal button or axis index joydev numbers the control by.
    pub js_number: u8,
    pub js_value: i16,
    /// `EV_KEY` or `EV_ABS`.
    pub ev_type: u16,
    pub ev_code: u16,
    pub ev_value: i32,
}

/// Map one client control change onto the pad; `None` for a control the pad lacks.
///
/// `client_value` is the control's new value: 0 or 1 for a button, -1.0 to 1.0 for an axis,
/// 0.0 to 1.0 for a trigger button.
pub fn map_event(client_index: u32, client_value: f64, is_button: bool) -> Option<MappedEvent> {
    let (internal, ev_type, value, is_trigger, is_hat) = if is_button {
        if let Some((axis, direction)) = hat_of(client_index) {
            (
                axis,
                EV_ABS,
                (direction * client_value as i32) as f64,
                false,
                true,
            )
        } else if let Some(axis) = trigger_axis(client_index) {
            (
                axis,
                EV_ABS,
                client_value,
                TRIGGER_AXES.contains(&axis),
                false,
            )
        } else {
            (
                button_index(client_index)?,
                EV_KEY,
                (client_value as i32) as f64,
                false,
                false,
            )
        }
    } else {
        let axis = axis_index(client_index)?;
        (
            axis,
            EV_ABS,
            client_value,
            TRIGGER_AXES.contains(&axis),
            HAT_AXES.contains(&axis),
        )
    };
    let (ev_code, js_value, ev_value) = if ev_type == EV_KEY {
        let code = *BTN_MAP.get(internal as usize)?;
        (code, value as i32, value as i32)
    } else {
        let code = *AXES_MAP.get(internal as usize)?;
        (
            code,
            normalize_axis_value(value, is_trigger, is_hat, true),
            normalize_axis_value(value, is_trigger, is_hat, false),
        )
    };
    Some(MappedEvent {
        js_type: if ev_type == EV_KEY {
            JS_EVENT_BUTTON
        } else {
            JS_EVENT_AXIS
        },
        js_number: internal,
        js_value: js_value.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        ev_type,
        ev_code,
        ev_value,
    })
}

/// The joydev value an axis rests at: a trigger released at the bottom of its range, a
/// stick or hat at center.
pub fn js_axis_rest(internal: u8) -> i16 {
    normalize_axis_value(
        0.0,
        TRIGGER_AXES.contains(&internal),
        HAT_AXES.contains(&internal),
        true,
    ) as i16
}

/// The layout of the interposer's `js_config_t`, written byte for byte as the C reads it.
pub const INTERPOSER_MAX_BTNS: usize = 512;
pub const INTERPOSER_MAX_AXES: usize = 64;
pub const CONTROLLER_NAME_MAX_LEN: usize = 255;
pub const INTERPOSER_CONFIG_SIZE: usize = 1360;

/// The `js_config_t` the server sends first on every interposer connection: the name
/// (NUL-padded to 255 bytes, then one alignment byte), vendor, product, version, button
/// and axis counts, the button map (512 `u16`), the axis map (64 `u8`), and six bytes of
/// trailing alignment, 1360 bytes in all.
pub fn interposer_config() -> [u8; INTERPOSER_CONFIG_SIZE] {
    let mut out = [0u8; INTERPOSER_CONFIG_SIZE];
    let name = truncate_name(PAD_NAME, CONTROLLER_NAME_MAX_LEN - 1);
    out[..name.len()].copy_from_slice(name);
    let mut at = CONTROLLER_NAME_MAX_LEN + 1;
    for v in [
        PAD_VENDOR,
        PAD_PRODUCT,
        PAD_VERSION,
        BTN_MAP.len() as u16,
        AXES_MAP.len() as u16,
    ] {
        out[at..at + 2].copy_from_slice(&v.to_ne_bytes());
        at += 2;
    }
    for code in BTN_MAP {
        out[at..at + 2].copy_from_slice(&code.to_ne_bytes());
        at += 2;
    }
    at = CONTROLLER_NAME_MAX_LEN + 1 + 10 + 2 * INTERPOSER_MAX_BTNS;
    for (i, code) in AXES_MAP.iter().enumerate() {
        out[at + i] = *code as u8;
    }
    out
}
