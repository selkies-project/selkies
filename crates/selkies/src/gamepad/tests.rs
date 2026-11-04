/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The gamepad checks: the uinput ABI against the kernel headers, the interposer struct layouts
//! against the C compiler, the backend policy, the slot gate, the force-feedback mix and the kernel
//! requests, the unread handle, the sockets fake-udev enumerates (`test_fake_udev_enumeration`'s
//! server side), and the kernel path under the `/dev/uinput` emulator.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::abi::*;
use super::ff::*;
use super::mapper::*;
use super::pad::*;
use super::uinput::*;
use super::virtual_device::*;
use super::*;

/// The repository root, found from the crate directory.
fn repo_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("tests/tools/uinput_abi_truth.c").exists() {
            return dir;
        }
        assert!(
            dir.pop(),
            "the repository root was not found above {}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
}

/// A scratch directory of this process for one test.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("selkies-gamepad-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build one tool under `tests/tools` with its Makefile and return its path.
fn tool(name: &str) -> PathBuf {
    let tools = repo_root().join("tests/tools");
    let status = Command::new("make")
        .args(["-s", "-C"])
        .arg(&tools)
        .arg(name)
        .status()
        .expect("make runs");
    assert!(status.success(), "make -C tests/tools {name}");
    tools.join(name)
}

/// Compile a C program printing `name value` lines and return them.
fn c_truth(tag: &str, source: &str) -> HashMap<String, i64> {
    let dir = scratch(tag);
    let src = dir.join("truth.c");
    std::fs::write(&src, source).unwrap();
    let bin = dir.join("truth");
    let out = Command::new("gcc")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .output()
        .expect("gcc runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = Command::new(&bin).output().unwrap();
    parse_truth(&String::from_utf8_lossy(&out.stdout))
}

fn parse_truth(text: &str) -> HashMap<String, i64> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.parse().ok()?))
        })
        .collect()
}

#[test]
fn uinput_abi_matches_kernel_headers() {
    let truth = Command::new(tool("uinput_abi_truth")).output().unwrap();
    let truth = parse_truth(&String::from_utf8_lossy(&truth.stdout));
    let want = |name: &str| {
        *truth
            .get(name)
            .unwrap_or_else(|| panic!("{name} in the truth"))
    };
    let checks: [(&str, i64); 32] = [
        ("UI_DEV_CREATE", UI_DEV_CREATE as i64),
        ("UI_DEV_DESTROY", UI_DEV_DESTROY as i64),
        ("UI_DEV_SETUP", UI_DEV_SETUP as i64),
        ("UI_ABS_SETUP", UI_ABS_SETUP as i64),
        ("UI_SET_EVBIT", UI_SET_EVBIT as i64),
        ("UI_SET_KEYBIT", UI_SET_KEYBIT as i64),
        ("UI_SET_ABSBIT", UI_SET_ABSBIT as i64),
        ("UI_GET_SYSNAME", UI_GET_SYSNAME as i64),
        ("UINPUT_MAX_NAME_SIZE", UINPUT_MAX_NAME_SIZE as i64),
        ("BUS_USB", BUS_USB as i64),
        ("sizeof_uinput_setup", UINPUT_SETUP_SIZE as i64),
        ("off_setup_name", 8),
        ("off_setup_ff", 88),
        ("sizeof_uinput_abs_setup", UINPUT_ABS_SETUP_SIZE as i64),
        ("off_abs_setup_absinfo", 4),
        (
            "sizeof_input_event",
            (pack_evdev_report(EV_KEY, BTN_A, 1, 64).len() / 2) as i64,
        ),
        (
            "sizeof_js_event",
            pack_js_event(JS_EVENT_BUTTON, 0, 0).len() as i64,
        ),
        ("off_js_value", 4),
        ("off_js_type", 6),
        ("off_js_number", 7),
        ("JS_EVENT_BUTTON", JS_EVENT_BUTTON as i64),
        ("JS_EVENT_AXIS", JS_EVENT_AXIS as i64),
        ("JS_EVENT_INIT", JS_EVENT_INIT as i64),
        ("UI_SET_FFBIT", UI_SET_FFBIT as i64),
        ("UI_SET_PHYS", UI_SET_PHYS as i64),
        ("UI_BEGIN_FF_UPLOAD", UI_BEGIN_FF_UPLOAD as i64),
        ("UI_END_FF_UPLOAD", UI_END_FF_UPLOAD as i64),
        ("UI_BEGIN_FF_ERASE", UI_BEGIN_FF_ERASE as i64),
        ("UI_END_FF_ERASE", UI_END_FF_ERASE as i64),
        ("EV_UINPUT", EV_UINPUT as i64),
        ("UI_FF_UPLOAD", UI_FF_UPLOAD as i64),
        ("UI_FF_ERASE", UI_FF_ERASE as i64),
    ];
    for (name, got) in checks {
        assert_eq!(got, want(name), "{name}");
    }
    for (name, got) in [
        ("EV_FF", EV_FF),
        ("FF_RUMBLE", FF_RUMBLE),
        ("FF_PERIODIC", FF_PERIODIC),
        ("FF_SQUARE", FF_SQUARE),
        ("FF_TRIANGLE", FF_TRIANGLE),
        ("FF_SINE", FF_SINE),
        ("FF_GAIN", FF_GAIN),
    ] {
        assert_eq!(got as i64, want(name), "{name}");
    }
    assert_eq!(FF_EFFECT_SIZE as i64, want("sizeof_ff_effect"));
    assert_eq!(10, want("off_ff_replay"));
    assert_eq!(FF_EFFECT_HEAD_SIZE as i64, want("off_ff_u"));
    assert_eq!(4, want("off_periodic_magnitude"));
    assert_eq!(
        UINPUT_FF_UPLOAD_SIZE as i64,
        want("sizeof_uinput_ff_upload")
    );
    assert_eq!(8, want("off_ff_upload_effect"));
    assert_eq!(UINPUT_FF_ERASE_SIZE as i64, want("sizeof_uinput_ff_erase"));
    // The synthetic events that open a joydev stream carry JS_EVENT_INIT in the type byte.
    let (ev_type, number, value) =
        unpack_js_event(&pack_js_event(JS_EVENT_BUTTON | JS_EVENT_INIT, 5, 1)).unwrap();
    assert_eq!(
        ev_type as i64,
        want("JS_EVENT_BUTTON") | want("JS_EVENT_INIT")
    );
    assert_eq!((number, value), (5, 1));
}

#[test]
fn axis_values_land_on_the_declared_range() {
    assert_eq!(normalize_axis_value(-1.0, false, false, false), ABS_MIN_VAL);
    assert_eq!(normalize_axis_value(0.0, false, false, false), 0);
    assert_eq!(normalize_axis_value(1.0, false, false, false), ABS_MAX_VAL);
    assert_eq!(normalize_axis_value(0.0, true, false, false), ABS_MIN_VAL);
    assert_eq!(normalize_axis_value(1.0, true, false, false), ABS_MAX_VAL);
    assert_eq!(
        normalize_axis_value(-1.0, false, true, false),
        ABS_HAT_MIN_VAL
    );
    assert_eq!(normalize_axis_value(0.0, false, true, false), 0);
    assert_eq!(
        normalize_axis_value(1.0, false, true, false),
        ABS_HAT_MAX_VAL
    );
    assert_eq!(
        normalize_axis_value(7.0, false, true, false),
        ABS_HAT_MAX_VAL,
        "a hat past its end is clamped"
    );
    assert_eq!(normalize_axis_value(-1.0, false, true, true), -ABS_MAX_VAL);
    assert_eq!(normalize_axis_value(1.0, false, true, true), ABS_MAX_VAL);
    for client in [-1.0, 1.0] {
        let want = normalize_axis_value(client, false, false, false);
        let (_, _, value) = unpack_js_event(&pack_js_event(JS_EVENT_AXIS, 0, want as i16)).unwrap();
        assert_eq!(
            value as i32, want,
            "the axis value survives the joydev event"
        );
    }
}

/// A client control change: `(index, value, is button)`.
type ClientChange = (u32, f64, bool);

#[test]
fn mapper_event_semantics() {
    let script: [(ClientChange, (u16, u16, i32)); 9] = [
        ((0, 1.0, true), (EV_KEY, BTN_A, 1)),
        ((0, 0.0, true), (EV_KEY, BTN_A, 0)),
        ((6, 0.5, true), (EV_ABS, ABS_Z, 0)),
        ((12, 1.0, true), (EV_ABS, ABS_HAT0Y, -1)),
        ((12, 0.0, true), (EV_ABS, ABS_HAT0Y, 0)),
        ((0, -1.0, false), (EV_ABS, ABS_X, -32767)),
        ((1, 0.25, false), (EV_ABS, ABS_Y, 8191)),
        ((16, 1.0, true), (EV_KEY, BTN_MODE, 1)),
        ((16, 0.0, true), (EV_KEY, BTN_MODE, 0)),
    ];
    for ((index, value, is_button), want) in script {
        let ev = map_event(index, value, is_button).unwrap();
        assert_eq!(
            (ev.ev_type, ev.ev_code, ev.ev_value),
            want,
            "client {index} = {value}"
        );
    }
    assert!(map_event(7, 1.0, false).is_none(), "an axis the pad lacks");
    assert!(map_event(17, 1.0, true).is_none(), "a button the pad lacks");
    let hat = map_event(14, 1.0, true).unwrap();
    assert_eq!(
        (hat.js_type, hat.js_number, hat.js_value),
        (JS_EVENT_AXIS, 6, -32767),
        "a hat is full scale on joydev"
    );
}

#[test]
fn interposer_structs_match_the_c_layout() {
    let truth = c_truth(
        "cstructs",
        r#"
#include <stdint.h>
#include <stdio.h>
#include <stddef.h>
#include <linux/input.h>
#include <linux/uinput.h>
#define CONTROLLER_NAME_MAX_LEN 255
#define INTERPOSER_MAX_BTNS 512
#define INTERPOSER_MAX_AXES 64
typedef struct { char name[CONTROLLER_NAME_MAX_LEN]; uint16_t vendor; uint16_t product; uint16_t version;
  uint16_t num_btns; uint16_t num_axes; uint16_t btn_map[INTERPOSER_MAX_BTNS]; uint8_t axes_map[INTERPOSER_MAX_AXES];
  uint8_t final_alignment_padding[6]; } js_config_t;
typedef struct { uint8_t kind; uint8_t reserved; int16_t id; uint16_t strong; uint16_t weak; uint16_t length_ms;
  uint16_t delay_ms; uint16_t count; uint16_t reserved2; } sji_ff_record_t;
#define UDYN_BYTES(max) (((max) / 8) + 1)
typedef struct { uint32_t magic; uint32_t version; uint16_t bustype; uint16_t vendor; uint16_t product; uint16_t version_id;
  char name[UINPUT_MAX_NAME_SIZE]; uint8_t evbits[UDYN_BYTES(EV_MAX)]; uint8_t keybits[UDYN_BYTES(KEY_MAX)];
  uint8_t relbits[UDYN_BYTES(REL_MAX)]; uint8_t absbits[UDYN_BYTES(ABS_MAX)]; uint8_t mscbits[UDYN_BYTES(MSC_MAX)];
  uint8_t propbits[UDYN_BYTES(INPUT_PROP_MAX)]; struct input_absinfo absinfo[ABS_CNT]; uint32_t event_size; } udyn_desc_t;
int main(void) {
  printf("sizeof_js_config %zu\n", sizeof(js_config_t));
  printf("off_vendor %zu\n", offsetof(js_config_t, vendor));
  printf("off_num_btns %zu\n", offsetof(js_config_t, num_btns));
  printf("off_btn_map %zu\n", offsetof(js_config_t, btn_map));
  printf("off_axes_map %zu\n", offsetof(js_config_t, axes_map));
  printf("sizeof_ff_record %zu\n", sizeof(sji_ff_record_t));
  printf("off_ff_id %zu\n", offsetof(sji_ff_record_t, id));
  printf("off_ff_count %zu\n", offsetof(sji_ff_record_t, count));
  printf("sizeof_udyn_desc %zu\n", sizeof(udyn_desc_t));
  printf("off_udyn_name %zu\n", offsetof(udyn_desc_t, name));
  printf("off_udyn_keybits %zu\n", offsetof(udyn_desc_t, keybits));
  printf("off_udyn_propbits %zu\n", offsetof(udyn_desc_t, propbits));
  printf("off_udyn_absinfo %zu\n", offsetof(udyn_desc_t, absinfo));
  printf("off_udyn_event_size %zu\n", offsetof(udyn_desc_t, event_size));
  return 0; }
"#,
    );
    let want = |name: &str| *truth.get(name).unwrap_or_else(|| panic!("{name}")) as usize;
    let config = interposer_config();
    assert_eq!(config.len(), want("sizeof_js_config"));
    assert_eq!(INTERPOSER_CONFIG_SIZE, want("sizeof_js_config"));
    let off_vendor = want("off_vendor");
    assert_eq!(&config[..PAD_NAME.len()], PAD_NAME.as_bytes());
    assert!(
        config[PAD_NAME.len()..off_vendor].iter().all(|b| *b == 0),
        "the name is NUL padded"
    );
    assert_eq!(read_u16(&config, off_vendor), PAD_VENDOR);
    assert_eq!(read_u16(&config, off_vendor + 2), PAD_PRODUCT);
    assert_eq!(read_u16(&config, off_vendor + 4), PAD_VERSION);
    assert_eq!(
        read_u16(&config, want("off_num_btns")),
        BTN_MAP.len() as u16
    );
    assert_eq!(
        read_u16(&config, want("off_num_btns") + 2),
        AXES_MAP.len() as u16
    );
    let btn_map = want("off_btn_map");
    for (i, code) in BTN_MAP.iter().enumerate() {
        assert_eq!(read_u16(&config, btn_map + 2 * i), *code, "button {i}");
    }
    assert_eq!(
        read_u16(&config, btn_map + 2 * BTN_MAP.len()),
        0,
        "the rest of the button map is zero"
    );
    let axes_map = want("off_axes_map");
    for (i, code) in AXES_MAP.iter().enumerate() {
        assert_eq!(config[axes_map + i] as u16, *code, "axis {i}");
    }
    assert_eq!(FF_RECORD_SIZE, want("sizeof_ff_record"));
    let mut record = [0u8; FF_RECORD_SIZE];
    record[0] = FF_RECORD_PLAY;
    record[want("off_ff_id")..want("off_ff_id") + 2].copy_from_slice(&(-3i16).to_ne_bytes());
    record[want("off_ff_count")..want("off_ff_count") + 2].copy_from_slice(&7u16.to_ne_bytes());
    let parsed = FfRecord::parse(&record).unwrap();
    assert_eq!(
        (parsed.kind, parsed.effect_id, parsed.count),
        (FF_RECORD_PLAY, -3, 7)
    );
    let caps = Capabilities {
        evbits: vec![EV_KEY],
        keybits: vec![30],
        ..Capabilities::default()
    };
    let desc = pack_udyn_desc("Selkies Virtual Keyboard", 0x1d6b, 1, 1, &caps);
    assert_eq!(desc.len(), want("sizeof_udyn_desc"));
    assert_eq!(UDYN_DESC_SIZE, want("sizeof_udyn_desc"));
    assert_eq!(read_u32(&desc, 0), UDYN_DESC_MAGIC);
    assert_eq!(read_u32(&desc, 4), UDYN_DESC_VERSION);
    assert_eq!(
        &desc[want("off_udyn_name")..want("off_udyn_name") + 24],
        b"Selkies Virtual Keyboard"
    );
    let evbits = want("off_udyn_name") + UINPUT_MAX_NAME_SIZE;
    assert_eq!(
        desc[evbits],
        (1 << EV_SYN) | (1 << EV_KEY),
        "EV_SYN is always advertised"
    );
    assert_eq!(
        desc[want("off_udyn_keybits") + 30 / 8],
        1 << (30 % 8),
        "KEY_A set"
    );
    assert_eq!(
        want("off_udyn_propbits") + 4 + 1,
        want("off_udyn_absinfo"),
        "one alignment byte before absinfo"
    );
    assert_eq!(
        read_u32(&desc, want("off_udyn_event_size")),
        LOCAL_INPUT_EVENT_SIZE as u32
    );
}

#[test]
fn uinput_probe_answers_for_each_way_the_node_can_behave() {
    let dir = scratch("probe");
    let node = dir.join("node");
    std::fs::write(&node, b"").unwrap();
    assert!(
        uinput_writable_at(&node),
        "a node this process can open for writing"
    );
    assert!(!uinput_writable_at(&dir.join("absent")), "no node at all");
    let refused = dir.join("refused");
    std::fs::write(&refused, b"").unwrap();
    std::fs::set_permissions(
        &refused,
        std::os::unix::fs::PermissionsExt::from_mode(0o444),
    )
    .unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert!(
            !uinput_writable_at(&refused),
            "a node that refuses the open"
        );
    }
    let fifo = dir.join("reader-less");
    let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(uinput_writable_at(&fifo)));
    let got = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the probe returns");
    assert!(!got, "a node a blocking open would never return from");
}

#[test]
fn uinput_policy_matrix() {
    const INTERPOSER: &str = "/usr/$LIB/selkies_input_interposer.so";
    const PRELOADED: &str = "/usr/lib/x86_64-linux-gnu/selkies_input_interposer.so";
    for has_uinput in [true, false] {
        let case = |mode: &str, interposer: Option<&str>, preload: Option<&str>| {
            decide_uinput(
                mode,
                || interposer_configured_by(interposer, preload),
                || has_uinput,
            )
        };
        assert!(
            !case("auto", Some(INTERPOSER), None),
            "auto yields to SELKIES_INTERPOSER"
        );
        assert!(
            !case("auto", None, Some(PRELOADED)),
            "auto yields to a preloaded interposer"
        );
        assert_eq!(
            case("auto", None, Some("/usr/lib/libfakeroot.so")),
            has_uinput,
            "an unrelated preload"
        );
        assert_eq!(
            case("auto", None, Some("")),
            has_uinput,
            "auto uses uinput when nothing preloads"
        );
        assert!(!case("false", None, None), "false never uses uinput");
        assert_eq!(
            case("true", Some(INTERPOSER), None),
            has_uinput,
            "true ignores the interposer"
        );
        assert_eq!(
            case("banana", None, None),
            has_uinput,
            "unknown value falls back to auto"
        );
        assert_eq!(
            case("", None, None),
            has_uinput,
            "empty value falls back to auto"
        );
    }
}

#[test]
fn publish_policy() {
    assert!(!publish_wanted(false, ""), "off by default");
    assert!(publish_wanted(true, ""), "on, both devices");
    assert!(
        !publish_wanted(true, "wayland-0"),
        "on, but host capture owns the session"
    );
    let names: Vec<&str> = published_device_specs().iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        ["Selkies Virtual Keyboard", "Selkies Virtual Pointer"]
    );
}

#[test]
#[ignore = "needs an X display (DISPLAY), an Xvfb that lists no device nodes"]
fn xvfb_does_not_read_kernel_input() {
    let (conn, _) = x11rb::connect(None).expect("an X display");
    assert!(!display_reads_kernel_input(&conn));
}

#[test]
fn slot_authority_matrix() {
    let connect = |i: usize| format!("js,c,{i},UFJPQkU=,6,17");
    let button = |i: usize| format!("js,b,{i},0,1");
    let disconnect = |i: usize| format!("js,d,{i}");
    let all = |i: usize| [connect(i), button(i), disconnect(i)];
    let (viewer, controller) = (Role::Viewer, Role::Controller);
    assert!(
        (0..4).all(|i| all(i).iter().all(|m| slot_denied(m, viewer, None, false))),
        "a shared viewer holding no slot drives no gamepad"
    );
    let two = json!(2);
    assert!(
        all(1)
            .iter()
            .all(|m| !slot_denied(m, viewer, Some(&two), false)),
        "a viewer holding slot 2 drives index 1"
    );
    assert!(
        [0, 2, 3]
            .iter()
            .all(|i| slot_denied(&button(*i), viewer, Some(&two), false)),
        "and no other index"
    );
    assert!(
        slot_denied(&connect(0), viewer, Some(&two), false),
        "a viewer's connect cannot claim another slot"
    );
    assert!(
        slot_denied(&disconnect(0), viewer, Some(&two), false),
        "a viewer's disconnect cannot reset another slot's pad"
    );
    assert!(
        (0..4).all(|i| !slot_denied(&button(i), controller, None, false)),
        "a legacy controller is unrestricted"
    );
    assert!(
        slot_denied(&button(0), controller, None, true)
            && slot_denied(&button(0), viewer, None, true),
        "a token with no slot drives no gamepad whatever its role"
    );
    let one = json!(1);
    assert!(
        slot_denied(&button(1), controller, Some(&one), true)
            && !slot_denied(&button(0), controller, Some(&one), true),
        "a token's slot governs a controller too"
    );
    assert!(
        !slot_denied("kd,65", viewer, None, true),
        "a non-gamepad message is not this gate's business"
    );
    assert!(
        slot_denied("js,b,x,0,1", viewer, Some(&two), false)
            && slot_denied("js,b", viewer, Some(&two), false),
        "a malformed or absent index is refused"
    );
    assert!(
        !slot_denied("js,b, 01 ,0,1", viewer, Some(&two), false),
        "an index spelled oddly is compared by value"
    );
    let three_four = json!([3, 4]);
    assert!(
        [2, 3].iter().all(|i| all(*i).iter().all(|m| !slot_denied(
            m,
            controller,
            Some(&three_four),
            true
        ))),
        "a token holding slots 3 and 4 drives indexes 2 and 3"
    );
    assert!(
        [0, 1]
            .iter()
            .all(|i| slot_denied(&button(*i), controller, Some(&three_four), true)),
        "and no other"
    );
    assert!(
        slot_denied(&button(0), viewer, Some(&json!(-1)), false),
        "a strict viewer's claim of -1 drives none"
    );
    assert!(
        slot_denied(&button(0), viewer, Some(&json!(null)), false),
        "a null slot is no slot"
    );
}

#[test]
fn token_slots_and_the_table_form() {
    assert_eq!(token_slots(&json!(2)), [2]);
    assert_eq!(
        token_slots(&json!("3")),
        [3],
        "a decimal string names its slot"
    );
    assert_eq!(token_slots(&json!([4, 3])), [4, 3], "a list in its order");
    assert!(
        token_slots(&json!(null)).is_empty()
            && token_slots(&json!(0)).is_empty()
            && token_slots(&json!([])).is_empty()
    );
    assert_eq!(
        token_slots(&json!([5, true, "x", 2, 2])),
        [2],
        "no slot names none, and a slot named twice counts once"
    );
    assert!(token_slots(&json!({"slot": 1})).is_empty());
    assert_eq!(stored_slot(&json!(2)), json!(2));
    assert_eq!(stored_slot(&json!([2])), json!(2));
    assert_eq!(stored_slot(&json!("2")), json!(2));
    assert_eq!(stored_slot(&json!([3, 4])), json!([3, 4]));
    assert_eq!(stored_slot(&json!([])), json!(null));
    assert_eq!(stored_slot(&json!(0)), json!(null));
}

fn levels(r: Option<Rumble>) -> Option<(f64, f64, u32)> {
    r.map(|r| {
        (
            (r.strong * 1000.0).round() / 1000.0,
            (r.weak * 1000.0).round() / 1000.0,
            r.hold_ms,
        )
    })
}

#[test]
fn force_feedback_mix() {
    let (a, b) = (FfSource::Conn(1), FfSource::Conn(2));
    let ms = Duration::from_millis;
    let mut mixer = FfMixer::default();
    let t0 = Instant::now();
    assert_eq!(
        levels(mixer.record(a, FfRecord::play(0, 0x8000, 0x4000, 100, 0, 1), t0)),
        Some((0.5, 0.25, 100)),
        "a played effect reaches the motors at once, for its length"
    );
    assert_eq!(mixer.next_wake(), Some(t0 + ms(100)));
    assert_eq!(
        levels(mixer.update(t0 + ms(100))),
        Some((0.0, 0.0, 0)),
        "and stops when its length has run"
    );
    assert_eq!(mixer.next_wake(), None);

    let t = t0 + ms(300);
    mixer.record(a, FfRecord::play(0, 0xC000, 0, 200, 0, 1), t);
    assert_eq!(
        levels(mixer.record(b, FfRecord::play(3, 0x8000, 0x2000, 200, 0, 1), t)),
        Some((1.0, 0.125, 200)),
        "effects playing together add up, capped at full"
    );
    assert_eq!(
        levels(mixer.record(b, FfRecord::stop(3), t)),
        Some((0.75, 0.0, 200)),
        "stopping one leaves the other"
    );
    assert_eq!(
        levels(mixer.record(a, FfRecord::play(0, 0x4000, 0x4000, 200, 0, 1), t)),
        Some((0.25, 0.25, 200)),
        "a replay of the same effect replaces it"
    );
    assert_eq!(
        levels(mixer.record(a, FfRecord::gain(0x8000), t)),
        Some((0.125, 0.125, 200)),
        "the gain scales the mix"
    );
    mixer.record(a, FfRecord::gain(0xffff), t);
    assert_eq!(
        levels(mixer.forget(a, t)),
        Some((0.0, 0.0, 0)),
        "a closed handle's effects stop"
    );

    let t = t0 + ms(600);
    assert_eq!(
        mixer.record(a, FfRecord::play(1, 0x8000, 0x8000, 60, 80, 1), t),
        None,
        "a delayed effect waits out its delay"
    );
    assert_eq!(mixer.next_wake(), Some(t + ms(80)));
    assert_eq!(
        levels(mixer.update(t + ms(80))),
        Some((0.5, 0.5, 60)),
        "then plays for its length"
    );
    assert_eq!(
        levels(mixer.update(t + ms(140))),
        Some((0.0, 0.0, 0)),
        "and stops"
    );

    let t = t0 + ms(900);
    assert_eq!(
        levels(mixer.record(a, FfRecord::play(2, 0x8000, 0, 50, 0, 3), t)),
        Some((0.5, 0.0, 150)),
        "a repeat count plays the length that many times"
    );
    assert_eq!(
        levels(mixer.update(t + ms(150))),
        Some((0.0, 0.0, 0)),
        "and then stops"
    );

    let t = t0 + ms(1200);
    let lease = FF_LEASE_MS;
    assert_eq!(
        levels(mixer.record(a, FfRecord::play(4, 0xffff, 0xffff, 0, 0, 1), t)),
        Some((1.0, 1.0, lease)),
        "an effect without an end is held for a lease"
    );
    assert_eq!(
        mixer.next_wake(),
        Some(t + ms(lease as u64 / 2)),
        "renewed at half the lease"
    );
    assert_eq!(
        mixer.update(t + ms(500)),
        None,
        "an unchanged mix is not sent again before its renewal"
    );
    assert_eq!(
        levels(mixer.update(t + ms(lease as u64 / 2))),
        Some((1.0, 1.0, lease)),
        "renewed while it plays"
    );
    assert_eq!(
        levels(mixer.replay(t + ms(1100))),
        Some((1.0, 1.0, lease)),
        "a client taking the slot is handed the mix at once"
    );
    assert_eq!(
        levels(mixer.record(a, FfRecord::stop(4), t + ms(1200))),
        Some((0.0, 0.0, 0)),
        "until stopped"
    );
    assert_eq!(
        mixer.replay(t + ms(1300)),
        None,
        "and a quiet pad hands it nothing"
    );
    assert_eq!(
        levels(mixer.record(a, FfRecord::play(5, 0x8000, 0, 65535, 0, 1), t + ms(1400))),
        Some((0.5, 0.0, lease)),
        "one longer than the lease is held for a lease too"
    );
    assert_eq!(
        levels(mixer.record(a, FfRecord::stop(5), t + ms(1500))),
        Some((0.0, 0.0, 0))
    );
    assert_eq!(
        mixer.next_wake(),
        None,
        "nothing is sent once everything is quiet"
    );
}

/// The uinput ioctls the kernel backend answers requests with, answered as the kernel does:
/// a BEGIN fills in the request's effect (or effect id), an END records what the owner
/// answered.
#[derive(Default)]
struct FakeKernel {
    /// request id to `(type, effect id, length, delay, union bytes)`
    effects: HashMap<u32, (u16, i16, u16, u16, Vec<u8>)>,
    erase_ids: HashMap<u32, u32>,
    answers: Vec<(&'static str, u32, i32)>,
}

impl FakeKernel {
    fn ioctl(&mut self, request: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if request == UI_BEGIN_FF_UPLOAD {
            let (etype, eid, length, delay, union) = self.effects[&read_u32(buf, 0)].clone();
            buf[8..10].copy_from_slice(&etype.to_ne_bytes());
            buf[10..12].copy_from_slice(&eid.to_ne_bytes());
            buf[18..20].copy_from_slice(&length.to_ne_bytes());
            buf[20..22].copy_from_slice(&delay.to_ne_bytes());
            buf[24..24 + union.len()].copy_from_slice(&union);
        } else if request == UI_END_FF_UPLOAD {
            self.answers
                .push(("upload", read_u32(buf, 0), read_u32(buf, 4) as i32));
        } else if request == UI_BEGIN_FF_ERASE {
            let id = self.erase_ids[&read_u32(buf, 0)];
            write_u32(buf, 8, id);
        } else if request == UI_END_FF_ERASE {
            self.answers
                .push(("erase", read_u32(buf, 0), read_u32(buf, 4) as i32));
        }
        Ok(())
    }
}

fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
        0
    );
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn local_event(ev_type: u16, code: u16, value: i32) -> Vec<u8> {
    let mut out = vec![0u8; LOCAL_INPUT_EVENT_SIZE - 8];
    out.extend_from_slice(&ev_type.to_ne_bytes());
    out.extend_from_slice(&code.to_ne_bytes());
    out.extend_from_slice(&value.to_ne_bytes());
    out
}

#[test]
fn kernel_force_feedback_requests_are_answered() {
    use std::os::fd::AsRawFd;
    let kernel = Arc::new(Mutex::new(FakeKernel::default()));
    let (ours, app) = socketpair();
    assert_eq!(
        unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        0
    );
    let app = std::os::unix::net::UnixStream::from(app);
    let k = kernel.clone();
    let mut dev = UInputGamepad::new("rumble-test", "").with_fd(
        ours,
        Box::new(move |_, req, buf| k.lock().unwrap().ioctl(req, buf)),
    );
    let events = |evs: &[(u16, u16, i32)]| {
        use std::io::Write;
        let mut w = &app;
        for (t, c, v) in evs {
            w.write_all(&local_event(*t, *c, *v)).unwrap();
        }
    };
    let mut rumble = Vec::new();
    rumble.extend_from_slice(&0x8000u16.to_ne_bytes());
    rumble.extend_from_slice(&0x4000u16.to_ne_bytes());
    kernel
        .lock()
        .unwrap()
        .effects
        .insert(7, (FF_RUMBLE, 0, 500, 20, rumble));
    events(&[(EV_UINPUT, UI_FF_UPLOAD, 7)]);
    let out = dev.service_ff().unwrap();
    assert_eq!(
        kernel.lock().unwrap().answers,
        [("upload", 7, 0)],
        "an upload is answered with its request id and success"
    );
    assert!(out.is_empty());
    assert_eq!(
        dev.effects().get(&0),
        Some(&(0x8000, 0x4000, 500, 20)),
        "and the effect kept"
    );

    let mut periodic = Vec::new();
    periodic.extend_from_slice(&FF_SINE.to_ne_bytes());
    periodic.extend_from_slice(&50u16.to_ne_bytes());
    periodic.extend_from_slice(&0x3000i16.to_ne_bytes());
    kernel
        .lock()
        .unwrap()
        .effects
        .insert(8, (FF_PERIODIC, 1, 300, 0, periodic));
    kernel
        .lock()
        .unwrap()
        .effects
        .insert(9, (0x52, 2, 300, 0, 0x3000i16.to_ne_bytes().to_vec()));
    events(&[(EV_UINPUT, UI_FF_UPLOAD, 8), (EV_UINPUT, UI_FF_UPLOAD, 9)]);
    dev.service_ff().unwrap();
    assert_eq!(
        dev.effects().get(&1),
        Some(&(0x6000, 0x6000, 300, 0)),
        "a periodic effect is kept at twice its magnitude on both motors"
    );
    assert!(
        kernel
            .lock()
            .unwrap()
            .answers
            .contains(&("upload", 9, -libc::EINVAL))
            && !dev.effects().contains_key(&2),
        "a constant-force effect is refused"
    );

    events(&[
        (EV_FF, 0, 1),
        (EV_FF, 1, 2),
        (EV_FF, 0, 0),
        (EV_FF, FF_GAIN, 0x4000),
        (EV_FF, 5, 1),
    ]);
    let out = dev.service_ff().unwrap();
    assert_eq!(
        out,
        [
            FfRecord::play(0, 0x8000, 0x4000, 500, 20, 1),
            FfRecord::play(1, 0x6000, 0x6000, 300, 0, 2),
            FfRecord::stop(0),
            FfRecord::gain(0x4000)
        ],
        "plays, stops, and the gain come back as records, an unknown effect's play dropped"
    );

    kernel.lock().unwrap().erase_ids.insert(10, 1);
    events(&[(EV_UINPUT, UI_FF_ERASE, 10)]);
    let out = dev.service_ff().unwrap();
    assert_eq!(
        kernel.lock().unwrap().answers.last(),
        Some(&("erase", 10, 0))
    );
    assert_eq!(
        out,
        [FfRecord::stop(1)],
        "an erase is answered, and stops the effect"
    );
    assert!(!dev.effects().contains_key(&1));
    assert!(
        dev.service_ff().unwrap().is_empty(),
        "nothing left to read is no record"
    );
    dev.forget_fd();
}

/// One application handle on a pad node: the handshake, then a parse of what it reads
/// into the last value of every control.
struct Handle {
    stream: UnixStream,
    is_evdev: bool,
    state: HashMap<(u16, u16), i32>,
    records: usize,
    eof: bool,
    pending: Vec<u8>,
}

impl Handle {
    async fn open(path: &Path, is_evdev: bool) -> Handle {
        let mut stream = UnixStream::connect(path).await.unwrap();
        let mut config = vec![0u8; INTERPOSER_CONFIG_SIZE];
        stream.read_exact(&mut config).await.unwrap();
        stream.write_all(&[8]).await.unwrap();
        Handle {
            stream,
            is_evdev,
            state: HashMap::new(),
            records: 0,
            eof: false,
            pending: Vec::new(),
        }
    }

    fn take(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        let size = if self.is_evdev { 24 } else { JS_EVENT_SIZE };
        let whole = self.pending.len() - self.pending.len() % size;
        for chunk in self.pending[..whole].chunks_exact(size) {
            self.records += 1;
            if self.is_evdev {
                let (t, c, v) = unpack_local_input_event(chunk).unwrap();
                if t != EV_SYN {
                    self.state.insert((t, c), v);
                }
            } else {
                let (t, n, v) = unpack_js_event(chunk).unwrap();
                self.state
                    .insert(((t & !JS_EVENT_INIT) as u16, n as u16), v as i32);
            }
        }
        self.pending.drain(..whole);
    }

    /// Read until nothing more arrives for `quiet` or the connection ends.
    async fn catch_up(&mut self, quiet: Duration) {
        let mut buf = vec![0u8; 65536];
        loop {
            match tokio::time::timeout(quiet, self.stream.read(&mut buf)).await {
                Err(_) => return,
                Ok(Ok(0)) | Ok(Err(_)) => {
                    self.eof = true;
                    return;
                }
                Ok(Ok(n)) => {
                    let data = buf[..n].to_vec();
                    self.take(&data);
                }
            }
        }
    }
}

#[tokio::test]
async fn an_unread_handle_is_shed_and_resynced_but_never_closed() {
    let dir = scratch("slow-reader");
    let pad = Pad::new(0, &dir, false);
    assert!(pad.start().await);
    let mut js_read = Handle::open(pad.js_path(), false).await;
    let mut js_idle = Handle::open(pad.js_path(), false).await;
    let mut ev_read = Handle::open(pad.evdev_path(), true).await;
    let mut ev_idle = Handle::open(pad.evdev_path(), true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The read handles are followed live, each arrival stamped.
    let follow = |mut h: Handle| {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let mut arrivals = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(2), h.stream.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => {
                        arrivals.push(Instant::now());
                        let data = buf[..n].to_vec();
                        h.take(&data);
                    }
                    _ => return (h, arrivals),
                }
            }
        })
    };
    let (js_follow, ev_follow) = (follow(js_read), follow(ev_read));
    let start = Instant::now();
    for n in 0..20000u32 {
        pad.send_event(0, if n % 2 == 1 { 1.0 } else { -1.0 }, false);
        pad.send_event(0, (n % 2) as f64, true);
        if n % 64 == 0 {
            tokio::task::yield_now().await;
        }
    }
    pad.send_event(0, 0.25, false);
    let took = start.elapsed();
    let (js_read_h, js_arrivals) = js_follow.await.unwrap();
    let (ev_read_h, ev_arrivals) = ev_follow.await.unwrap();
    js_read = js_read_h;
    ev_read = ev_read_h;
    for (name, arrivals) in [("js", &js_arrivals), ("evdev", &ev_arrivals)] {
        let worst = arrivals
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or_default();
        assert!(
            worst < Duration::from_millis(500),
            "the {name} handle being read is never paused for the unread ones: longest gap {worst:?} over {took:?}"
        );
    }
    for (name, idle, read) in [
        ("js", &mut js_idle, &js_read),
        ("evdev", &mut ev_idle, &ev_read),
    ] {
        idle.catch_up(Duration::from_millis(300)).await;
        assert!(!idle.eof, "an unread {name} handle stays open");
        assert!(
            idle.records < read.records,
            "and is shed events while unread: {} of {} records",
            idle.records,
            read.records
        );
        assert!(
            !idle.state.is_empty() && idle.state == read.state,
            "and once read it ends at the pad's state ({name})"
        );
    }
    pad.send_event(1, 0.5, false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut js_read = js_read;
    let mut ev_read = ev_read;
    js_read.catch_up(Duration::from_millis(200)).await;
    ev_read.catch_up(Duration::from_millis(200)).await;
    for (name, idle, read) in [
        ("js", &mut js_idle, &js_read),
        ("evdev", &mut ev_idle, &ev_read),
    ] {
        idle.catch_up(Duration::from_millis(200)).await;
        assert!(
            !idle.eof && idle.state == read.state,
            "and then follows the {name} stream live"
        );
    }
    tokio::time::timeout(Duration::from_secs(5), pad.close())
        .await
        .expect("the pad closes");
}

#[tokio::test]
async fn servers_close_while_applications_hold_the_pad_open() {
    let dir = scratch("closing");
    let pad = Pad::new(0, &dir, false);
    assert!(pad.start().await);
    let held = [
        Handle::open(pad.js_path(), false).await,
        Handle::open(pad.evdev_path(), true).await,
    ];
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(Duration::from_secs(3), pad.close())
        .await
        .expect("the servers close while applications still hold the pad open");
    drop(held);
}

#[tokio::test]
async fn a_js_handle_opens_on_the_pads_state_and_an_evdev_handle_writes_its_rumble() {
    let dir = scratch("handshake");
    let pad = Pad::new(0, &dir, false);
    assert!(pad.start().await);
    let sent: Arc<Mutex<Vec<(f64, f64, u32)>>> = Arc::default();
    let record = sent.clone();
    pad.set_on_rumble(Some(Arc::new(move |s, w, ms| {
        record.lock().unwrap().push((s, w, ms))
    })));
    pad.send_event(0, 1.0, true);
    pad.send_event(6, 1.0, true);
    let mut js = Handle::open(pad.js_path(), false).await;
    js.catch_up(Duration::from_millis(200)).await;
    assert_eq!(
        js.records,
        BTN_MAP.len() + AXES_MAP.len(),
        "the init replay covers every control"
    );
    assert_eq!(
        js.state.get(&(JS_EVENT_BUTTON as u16, 0)),
        Some(&1),
        "a held button opens held"
    );
    assert_eq!(
        js.state.get(&(JS_EVENT_AXIS as u16, 2)),
        Some(&32767),
        "the pressed trigger"
    );
    assert_eq!(
        js.state.get(&(JS_EVENT_AXIS as u16, 5)),
        Some(&-32767),
        "a trigger never moved rests released"
    );
    assert_eq!(
        js.state.get(&(JS_EVENT_AXIS as u16, 6)),
        Some(&0),
        "a hat never moved rests centered"
    );
    let mut evdev = Handle::open(pad.evdev_path(), true).await;
    let mut record = [0u8; FF_RECORD_SIZE];
    record[0] = FF_RECORD_PLAY;
    record[4..6].copy_from_slice(&0x8000u16.to_ne_bytes());
    record[6..8].copy_from_slice(&0x4000u16.to_ne_bytes());
    record[8..10].copy_from_slice(&100u16.to_ne_bytes());
    record[12..14].copy_from_slice(&1u16.to_ne_bytes());
    evdev.stream.write_all(&record).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        levels(sent.lock().unwrap().first().map(|r| Rumble {
            strong: r.0,
            weak: r.1,
            hold_ms: r.2
        })),
        Some((0.5, 0.25, 100))
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sent.lock().unwrap().len(),
        2,
        "the timer stops it when its length has run"
    );
    assert_eq!(sent.lock().unwrap()[1], (0.0, 0.0, 0));
    pad.reset_state();
    evdev.catch_up(Duration::from_millis(100)).await;
    assert_eq!(
        evdev.state.get(&(EV_KEY, BTN_A)),
        Some(&0),
        "reset releases the held button"
    );
    assert_eq!(
        evdev.state.get(&(EV_ABS, ABS_Z)),
        Some(&-32767),
        "and the trigger"
    );
    assert_eq!(pad.held_count(), 0);
    // An endless effect from the evdev handle is held for a lease and stops with the handle.
    record[8..10].copy_from_slice(&0u16.to_ne_bytes());
    evdev.stream.write_all(&record).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(sent.lock().unwrap().last().map(|r| r.2), Some(FF_LEASE_MS));
    drop(evdev);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        sent.lock().unwrap().last(),
        Some(&(0.0, 0.0, 0)),
        "a closed handle's effects stop"
    );
    pad.close().await;
}

#[tokio::test]
async fn the_sockets_fake_udev_enumerates_exist_only_while_served() {
    use std::os::unix::fs::FileTypeExt;
    let dir = scratch("enumeration");
    std::fs::write(dir.join("selkies_js1.sock"), b"stale").unwrap();
    let slots = GamepadSlots::new(&dir, false);
    slots.start().await;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut want: Vec<String> = (0..4)
        .flat_map(|i| [js_socket_name(i), evdev_socket_name(i)])
        .collect();
    want.sort();
    assert_eq!(
        names, want,
        "exactly the js and event sockets of the four slots, a stale file replaced"
    );
    for name in &names {
        assert!(
            std::fs::symlink_metadata(dir.join(name))
                .unwrap()
                .file_type()
                .is_socket(),
            "{name} is a socket"
        );
    }
    slots.close().await;
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "nothing is left once the pads close"
    );
}

#[derive(Default)]
struct RecordingHost {
    sent: Mutex<Vec<(ConnId, String)>>,
    authority: Mutex<HashMap<ConnId, SlotAuthority>>,
}

impl GamepadHost for RecordingHost {
    fn send_system_action(&self, conn: ConnId, action: &str) {
        self.sent.lock().unwrap().push((conn, action.to_string()));
    }

    fn slot_authority(&self, conn: ConnId) -> SlotAuthority {
        self.authority
            .lock()
            .unwrap()
            .get(&conn)
            .cloned()
            .unwrap_or(SlotAuthority {
                role: Role::Viewer,
                slot: None,
                secure: false,
            })
    }
}

fn install_default_settings() {
    crate::settings::install(crate::settings::Settings::parse(&[], &|_| None));
}

#[tokio::test]
async fn js_verbs_drive_the_slots_their_sender_holds() {
    install_default_settings();
    let dir = scratch("verbs");
    let slots = GamepadSlots::new(&dir, false);
    slots.start().await;
    let host = Arc::new(RecordingHost::default());
    host.authority.lock().unwrap().insert(
        7,
        SlotAuthority {
            role: Role::Controller,
            slot: None,
            secure: false,
        },
    );
    host.authority.lock().unwrap().insert(
        8,
        SlotAuthority {
            role: Role::Viewer,
            slot: Some(json!(2)),
            secure: false,
        },
    );
    slots.set_host(Some(host.clone()));
    assert!(
        !slots.handle_message(7, "kd,65"),
        "a key is not this handler's"
    );
    assert!(slots.handle_message(7, "js,c,0,UFJPQkU=,6,17"));
    let assoc = slots.associations();
    assert_eq!(
        (
            assoc[&0].client_name.as_str(),
            assoc[&0].client_num_btns,
            assoc[&0].client_num_axes,
            assoc[&0].conn
        ),
        ("PROBE", 17, 6, 7)
    );
    slots.handle_message(7, "js,b,0,0,1");
    assert_eq!(slots.pad(0).unwrap().held_count(), 1);
    slots.handle_message(7, "js,h,0");
    assert!(slots.heartbeat_live(0));
    slots.pad(0).unwrap().ff_record(
        FfSource::Conn(99),
        FfRecord::play(0, 0x8000, 0x4000, 100, 0, 1),
    );
    assert_eq!(
        host.sent.lock().unwrap().as_slice(),
        [(7, "rumble,0,0.500,0.250,100".to_string())],
        "rumble goes to the client driving the slot"
    );
    slots.handle_message(7, "js,d,0");
    assert!(
        slots.associations().is_empty() && slots.pad(0).unwrap().held_count() == 0,
        "a disconnect releases what was held"
    );
    assert!(!slots.heartbeat_live(0));

    slots.handle_message(8, "js,c,0,UFJPQkU=,6,17");
    assert!(
        slots.associations().is_empty(),
        "a viewer holding slot 2 cannot take slot 0"
    );
    slots.handle_message(8, "js,c,1,UFJPQkU=,6,17");
    assert_eq!(slots.associations()[&1].conn, 8, "but drives index 1");
    slots.handle_message(8, "js,b,1,2,1");
    slots.handle_message(8, "js,h,1");
    assert_eq!(slots.pad(1).unwrap().held_count(), 1);
    slots.sweep_stale(Instant::now() + Duration::from_secs(3));
    assert_eq!(
        slots.pad(1).unwrap().held_count(),
        0,
        "a lapsed heartbeat neutralizes the pad"
    );
    assert_eq!(
        slots.associations()[&1].conn,
        8,
        "but keeps the association"
    );
    slots.handle_message(8, "js,b,1,2,1");
    slots.release_for_conn(8);
    assert!(
        slots.associations().is_empty() && slots.pad(1).unwrap().held_count() == 0,
        "a vanished connection releases its slots"
    );

    slots.handle_message(7, "js,c,2,UFJPQkU=,6,17");
    slots.release_slots_for_conn(9, &[3]);
    assert_eq!(
        slots.associations().len(),
        1,
        "another connection's release leaves the slot alone"
    );
    slots.release_slots_for_conn(7, &[3]);
    assert!(
        slots.associations().is_empty(),
        "a slot taken from a connection is released"
    );
    slots.handle_message(7, "js,b,9,0,1");
    slots.handle_message(7, "js,b,x,0,1");
    slots.handle_message(7, "js,z,0");
    slots.close().await;
}

#[tokio::test]
async fn a_published_device_on_the_interposer_pool() {
    let dir = scratch("udyn");
    let spec = &published_device_specs()[1];
    let device = VirtualInputDevice::from_spec(spec, &dir);
    assert!(device.open(false).await);
    assert_eq!(device.node().as_deref(), Some("/dev/input/event3000"));
    assert!(!device.is_kernel());
    let desc = std::fs::read(dir.join("selkies_event3000.desc")).unwrap();
    assert_eq!(desc.len(), UDYN_DESC_SIZE);
    assert_eq!(
        read_u32(&desc, 4),
        2,
        "a version 2 descriptor names the event size"
    );
    let second = VirtualInputDevice::from_spec(&published_device_specs()[0], &dir);
    assert!(second.open(false).await);
    assert_eq!(
        second.node().as_deref(),
        Some("/dev/input/event3001"),
        "a live node is skipped"
    );
    let mut reader = UnixStream::connect(dir.join("selkies_event3000.sock"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    device.emit(EV_REL, REL_X, 5);
    let mut buf = vec![0u8; 2 * LOCAL_INPUT_EVENT_SIZE];
    tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unpack_local_input_event(&buf), Some((EV_REL, REL_X, 5)));
    assert_eq!(
        unpack_local_input_event(&buf[LOCAL_INPUT_EVENT_SIZE..]),
        Some((EV_SYN, SYN_REPORT, 0))
    );
    device.close().await;
    second.close().await;
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "socket and descriptor are removed"
    );
}

/// What `shim_child` plays on the pad: `(client index, value, is button)`.
const SHIM_SCRIPT: [ClientChange; 9] = [
    (0, 1.0, true),
    (0, 0.0, true),
    (6, 0.5, true),
    (12, 1.0, true),
    (12, 0.0, true),
    (0, -1.0, false),
    (1, 0.25, false),
    (16, 1.0, true),
    (16, 0.0, true),
];

/// Run the ignored test `name` in a child of this process under the `/dev/uinput` emulator,
/// which records the setup ioctls in `log` and the event writes in `stream`.
fn under_shim(shim: &Path, name: &str, log: &Path, stream: &Path, env: &[(&str, PathBuf)]) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        &format!("gamepad::tests::{name}"),
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env("LD_PRELOAD", shim)
    .env("UINPUT_SHIM_LOG", log)
    .env("UINPUT_SHIM_STREAM", stream)
    .env("UINPUT_SHIM_SYSNAME", "input999");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{name} ran: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The kernel backend driven in-process against the `/dev/uinput` emulator
/// (`tests/tools/uinput_shim.so`), which has to be preloaded before this process opens the
/// node: `shim_child` runs in a child of this process under the preload, and the recording
/// it leaves is checked here.
#[test]
fn the_kernel_device_is_set_up_as_the_interposer_pad_and_fed_the_same_stream() {
    let shim = tool("uinput_shim.so");
    let dir = scratch("shim");
    let (log, stream, collected) = (
        dir.join("shim.log"),
        dir.join("stream.bin"),
        dir.join("socket.bin"),
    );
    under_shim(
        &shim,
        "shim_child",
        &log,
        &stream,
        &[
            ("SELKIES_SHIM_SOCKETS", dir.join("sockets")),
            ("SELKIES_SHIM_COLLECTED", collected.clone()),
        ],
    );
    let log: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let codes = |prefix: &str| -> Vec<u16> {
        log.iter()
            .filter(|l| l.starts_with(prefix))
            .map(|l| {
                u16::from_str_radix(
                    l.split_whitespace()
                        .nth(1)
                        .unwrap()
                        .trim_start_matches("0x"),
                    16,
                )
                .unwrap()
            })
            .collect()
    };
    let mut evbits = codes("SET_EVBIT");
    evbits.sort();
    assert_eq!(
        evbits,
        [EV_KEY, EV_ABS, EV_FF],
        "EV_KEY, EV_ABS and EV_FF advertised"
    );
    let setup = log
        .iter()
        .find(|l| l.starts_with("DEV_SETUP"))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        codes("SET_FFBIT"),
        FF_MEMLESS_BITS,
        "a memless pad's force feedback advertised"
    );
    assert!(
        setup.contains(&format!("ff={FF_EFFECTS_MAX} ")),
        "16 effects at once: {setup}"
    );
    assert_eq!(codes("SET_KEYBIT"), BTN_MAP, "every button advertised");
    assert_eq!(codes("SET_ABSBIT"), AXES_MAP, "every axis advertised");
    assert!(
        setup.contains(&format!("vendor=0x{PAD_VENDOR:04x}"))
            && setup.contains(&format!("product=0x{PAD_PRODUCT:04x}"))
            && setup.contains(&format!("version=0x{PAD_VERSION:04x}"))
            && setup.ends_with(PAD_NAME)
            && setup.contains("bus=0x03"),
        "device identity matches the interposer's: {setup}"
    );
    let phys: Vec<&str> = log
        .iter()
        .filter(|l| l.starts_with("SET_PHYS "))
        .map(|l| l.split_once(' ').unwrap().1)
        .collect();
    assert_eq!(
        phys,
        ["virtual/input/selkies_ev0/phys"],
        "its physical path is the interposer's for the slot"
    );
    let mut abs_setup: HashMap<u16, (i32, i32, i32, i32, i32)> = HashMap::new();
    for line in log.iter().filter(|l| l.starts_with("ABS_SETUP")) {
        let f: HashMap<&str, &str> = line
            .split_whitespace()
            .skip(1)
            .map(|kv| kv.split_once('=').unwrap())
            .collect();
        let num = |k: &str| f[k].parse::<i32>().unwrap();
        abs_setup.insert(
            u16::from_str_radix(f["code"].trim_start_matches("0x"), 16).unwrap(),
            (num("min"), num("max"), num("fuzz"), num("flat"), num("res")),
        );
    }
    let expected: HashMap<u16, (i32, i32, i32, i32, i32)> = AXES_MAP
        .iter()
        .map(|c| {
            let i = abs_info(*c);
            (*c, (i.minimum, i.maximum, i.fuzz, i.flat, i.resolution))
        })
        .collect();
    assert_eq!(
        abs_setup, expected,
        "axis ranges match the interposer's EVIOCGABS"
    );
    let create = log.iter().position(|l| l == "DEV_CREATE").expect("created");
    assert_eq!(
        log.iter().filter(|l| *l == "DEV_CREATE").count(),
        1,
        "device created once"
    );
    let last_setup = log
        .iter()
        .rposition(|l| {
            l.starts_with("SET_") || l.starts_with("ABS_SETUP") || l.starts_with("DEV_SETUP")
        })
        .unwrap();
    assert!(create > last_setup, "created after setup");
    assert_eq!(
        log.iter().filter(|l| *l == "DEV_DESTROY").count(),
        1,
        "device destroyed on close"
    );
    assert!(
        log.iter().any(|l| l.starts_with("GET_SYSNAME")),
        "sysname queried"
    );
    let unpack = |blob: &[u8]| -> Vec<(u16, u16, i32)> {
        blob.as_chunks::<24>()
            .0
            .iter()
            .map(|c| unpack_local_input_event(c).unwrap())
            .collect()
    };
    let uinput_events = unpack(&std::fs::read(&stream).unwrap());
    let socket_events = unpack(&std::fs::read(&collected).unwrap());
    assert_eq!(
        uinput_events, socket_events,
        "kernel stream equals the evdev interposer stream"
    );
    let expected: Vec<(u16, u16, i32)> = [
        (EV_KEY, BTN_A, 1),
        (EV_KEY, BTN_A, 0),
        (EV_ABS, ABS_Z, 0),
        (EV_ABS, ABS_HAT0Y, -1),
        (EV_ABS, ABS_HAT0Y, 0),
        (EV_ABS, ABS_X, -32767),
        (EV_ABS, ABS_Y, 8191),
        (EV_KEY, BTN_MODE, 1),
        (EV_KEY, BTN_MODE, 0),
    ]
    .into_iter()
    .flat_map(|e| [e, (EV_SYN, 0, 0)])
    .collect();
    assert_eq!(uinput_events, expected, "event semantics");
}

/// The half of the shim check that runs under the preload; `the_kernel_device_is_set_up...`
/// spawns it, so it is ignored on its own.
#[test]
#[ignore = "runs under tests/tools/uinput_shim.so as a child of the shim test"]
fn shim_child() {
    let sockets =
        PathBuf::from(std::env::var("SELKIES_SHIM_SOCKETS").expect("spawned by the shim test"));
    let collected = PathBuf::from(std::env::var("SELKIES_SHIM_COLLECTED").unwrap());
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let pad = Pad::new(0, &sockets, true);
        assert!(pad.start().await);
        let mut handle = Handle::open(pad.evdev_path(), true).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        pad.ensure_uinput();
        assert!(pad.uinput_enabled(), "the emulated device was created");
        for (index, value, is_button) in SHIM_SCRIPT {
            pad.send_event(index, value, is_button);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut raw = Vec::new();
        let mut buf = vec![0u8; 4096];
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(300), handle.stream.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        std::fs::write(&collected, &raw).unwrap();
        pad.close().await;
    });
}

/// The kernel nodes of a device are resolved from sysfs, the emulator answering the sysname.
#[test]
fn kernel_nodes_are_resolved_from_sysfs() {
    let shim = tool("uinput_shim.so");
    let dir = scratch("shim-sysfs");
    let sysfs = dir.join("sysfs");
    std::fs::create_dir_all(sysfs.join("input999/event42")).unwrap();
    std::fs::create_dir_all(sysfs.join("input999/js3")).unwrap();
    std::fs::create_dir_all(sysfs.join("input999/capabilities")).unwrap();
    under_shim(
        &shim,
        "shim_sysfs_child",
        &dir.join("shim.log"),
        &dir.join("stream.bin"),
        &[("SELKIES_SHIM_SYSFS", sysfs)],
    );
}

#[test]
#[ignore = "runs under tests/tools/uinput_shim.so as a child of the sysfs test"]
fn shim_sysfs_child() {
    let sysfs =
        PathBuf::from(std::env::var("SELKIES_SHIM_SYSFS").expect("spawned by the sysfs test"));
    let mut dev = UInputGamepad::new("selkies_js0.sock", "").with_sysfs_base(&sysfs);
    let nodes = dev.create().unwrap();
    dev.destroy();
    assert_eq!(
        nodes,
        ["/dev/input/event42", "/dev/input/js3"],
        "kernel nodes resolved from sysfs"
    );
}
