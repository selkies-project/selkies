/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The handler end to end: wire messages through the real dispatcher and injectors, over a
//! recording seat and a fake core keymap, and against a real X server where a check needs one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pixelflux::Error;

use super::command::{AppSession, live_x_displays};
use super::dispatch::{FINGER_SCROLL_MAX_PX, POINTER_ECHO_MIN, POINTER_ECHO_POLL, PointerReader};
use super::handler::{InputConfig, KeyItem, detect_app_compositor_in, xdotool_type_args};
use super::pointer::Rect;
use super::wayland_keymap::testing::{FakeSeat, keymap_text};
use super::x11_keyboard::testing::FakeLink;
use super::x11_link::XLink;
use super::x11_worker::XWorker;
use super::*;
use crate::clipboard::Session;
use crate::core::Role;
use crate::session_screens::testing::{
    Owned, PrivateLabwc, Scratch, isolated, private_x_server, run_isolated,
};
use crate::session_screens::{AppCompositor, SessionScreens};

const A: Option<ConnId> = Some(1);
const B: Option<ConnId> = Some(2);
const SHIFT_L: u32 = 0xFFE1;
const CONTROL_L: u32 = 0xFFE3;
const ALTGR: u32 = 0xFE03;

/// The transport: records what the handler told it.
#[derive(Default)]
struct Rec {
    sent: Mutex<Vec<(Option<ConnId>, String)>>,
    echoes: Mutex<Vec<(ConnId, String)>>,
    calls: Mutex<Vec<String>>,
    layouts: Mutex<Layouts>,
    sender: Mutex<Sender>,
}

impl Rec {
    fn echoes_to(&self, conn: Option<ConnId>) -> Vec<String> {
        let conn = conn.unwrap();
        self.echoes
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, _)| *c == conn)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn call(&self, text: String) {
        self.calls.lock().unwrap().push(text);
    }
}

#[async_trait]
impl InputHost for Rec {
    fn sender(&self, _conn: Option<ConnId>) -> Sender {
        self.sender.lock().unwrap().clone()
    }
    fn send_system_action(&self, conn: Option<ConnId>, action: &str) {
        self.sent.lock().unwrap().push((conn, action.to_string()));
    }
    fn send_cursor(&self, _cursor: &CursorMessage) {}
    fn display_layouts(&self) -> Layouts {
        self.layouts.lock().unwrap().clone()
    }
    async fn resize(&self, resolution: &str, display_id: &DisplayId) {
        self.call(format!("resize {resolution} {display_id}"));
    }
    async fn scaling_ratio(&self, scale: f64, display_id: &DisplayId) {
        self.call(format!("scale {scale} {display_id}"));
    }
    async fn update_settings(&self, settings: serde_json::Value, display_id: &DisplayId) {
        self.call(format!("settings {settings} {display_id}"));
    }
    async fn video_bitrate(&self, kbps: i64, display_id: &DisplayId) {
        self.call(format!("vb {kbps} {display_id}"));
    }
    async fn audio_bitrate(&self, kbps: i64) {
        self.call(format!("ab {kbps}"));
    }
    async fn set_fps(&self, fps: f64, display_id: &DisplayId) {
        self.call(format!("fps {fps} {display_id}"));
    }
    async fn request_keyframe(&self, display_id: &DisplayId) {
        self.call(format!("keyframe {display_id}"));
    }
    async fn pointer_visible(&self, visible: bool) {
        self.call(format!("visible {visible}"));
    }
    async fn rate_control_mode(&self, mode: &str, display_id: &DisplayId) {
        self.call(format!("rc {mode} {display_id}"));
    }
    async fn crf(&self, value: i64, display_id: &DisplayId) {
        self.call(format!("crf {value} {display_id}"));
    }
    fn client_fps(&self, fps: i64) {
        self.call(format!("client_fps {fps}"));
    }
    fn client_latency(&self, ms: i64) {
        self.call(format!("client_latency {ms}"));
    }
    async fn client_webrtc_stats(&self, kind: &str, csv: &str) {
        self.call(format!("{kind} {csv}"));
    }
    fn ping_response(&self, _latency_ms: f64) {
        self.call("pong".into());
    }
    async fn set_client_stream_box(
        &self,
        display_id: &DisplayId,
        ox: f64,
        oy: f64,
        sx: f64,
        sy: f64,
    ) {
        self.call(format!("box {display_id} {ox} {oy} {sx} {sy}"));
    }
    fn app_compositor_adopted(&self, display: &str) {
        self.call(format!("adopted {display}"));
    }
    fn session_screens_changed(&self) {
        self.call("screens".into());
    }
}

impl PointerSink for Rec {
    fn pointer_echo(&self, conn: ConnId, action: &str) {
        self.echoes.lock().unwrap().push((conn, action.to_string()));
    }
}

/// The gamepads: records the messages handed over.
#[derive(Default)]
struct Pads(Mutex<Vec<String>>);

impl GamepadSink for Pads {
    fn handle(&self, _conn: ConnId, msg: &str) {
        self.0.lock().unwrap().push(msg.to_string());
    }
    fn release_conn(&self, _conn: ConnId) {}
    fn release_slots(&self, _conn: ConnId, _slots: &[u8]) {}
}

fn layouts(entries: &[(&str, i32, i32, i32, i32)]) -> Layouts {
    entries
        .iter()
        .map(|(id, x, y, w, h)| {
            (
                id.to_string(),
                Rect {
                    x: *x,
                    y: *y,
                    w: *w,
                    h: *h,
                },
            )
        })
        .collect()
}

fn dispatcher(cfg: InputConfig, seat: Option<Arc<dyn WaylandSeat>>) -> (Arc<Dispatcher>, Arc<Rec>) {
    let rec = Arc::new(Rec::default());
    *rec.sender.lock().unwrap() = Sender {
        role: Some(Role::Controller),
        input_authorized: true,
        ..Default::default()
    };
    let h = Dispatcher::new(
        cfg,
        rec.clone(),
        Arc::new(NullSink),
        rec.clone(),
        seat,
        None,
    );
    (h, rec)
}

/// An X11 handler over a fake core keymap.
fn x11(link: Arc<FakeLink>) -> (Arc<Dispatcher>, Arc<Rec>) {
    let (h, rec) = dispatcher(InputConfig::default(), None);
    h.install_x(XWorker::spawn(link as Arc<dyn XLink>).expect("the X thread starts"));
    (h, rec)
}

/// A Wayland handler whose apps run on the capture compositor, or on a nested one.
fn wayland(seat: Arc<dyn WaylandSeat>, separate: bool) -> (Arc<Dispatcher>, Arc<Rec>) {
    let cfg = InputConfig {
        is_wayland: true,
        ..Default::default()
    };
    let (h, rec) = dispatcher(cfg, Some(seat));
    set_topology(&h, separate);
    (h, rec)
}

/// Report the app compositor as resolved, nested or not, with no socket to find.
fn set_topology(h: &Dispatcher, separate: bool) {
    let mut st = h.state.lock();
    st.app_wl_cached = Some(
        if separate {
            "wayland-nested"
        } else {
            "wayland-capture"
        }
        .to_string(),
    );
    st.app_wl_is_separate = separate;
}

/// Deliver wire messages as the transport does, then let everything queued land.
async fn send(h: &Arc<Dispatcher>, messages: &[&str], conn: Option<ConnId>) {
    for msg in messages {
        h.on_message(msg, "primary", conn).await;
    }
    settle(h).await;
}

/// Wait for the Wayland worker's queue and the X thread's jobs to drain.
async fn settle(h: &Arc<Dispatcher>) {
    loop {
        let idle = {
            let st = h.state.lock();
            st.wl_queue.is_empty() && !st.keyboard_busy
        };
        if idle {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    if let Some(x) = h.x() {
        x.drain().await;
    }
}

/// A us core keymap slice of the navigation and editing keys.
const NAVIGATION: [(&str, u32); 12] = [
    ("Left", 0xFF51),
    ("Right", 0xFF53),
    ("Up", 0xFF52),
    ("Down", 0xFF54),
    ("Home", 0xFF50),
    ("End", 0xFF57),
    ("Page_Up", 0xFF55),
    ("Page_Down", 0xFF56),
    ("Tab", 0xFF09),
    ("Insert", 0xFF63),
    ("Delete", 0xFFFF),
    ("F3", 0xFFC0),
];

fn us_core_keymap() -> HashMap<u8, Vec<u32>> {
    let mut m: HashMap<u8, Vec<u32>> = HashMap::new();
    for (kc, syms) in [
        (37u8, vec![CONTROL_L]),
        (64, vec![0xFFE9]),
        (50, vec![SHIFT_L]),
        (62, vec![0xFFE2]),
        (108, vec![ALTGR]),
        (38, vec![0x61, 0x41]),
        (54, vec![0x63, 0x43]),
        (10, vec![0x31, 0x21]),
        (23, vec![0xFF09]),
        (110, vec![0xFF50]),
        (111, vec![0xFF52]),
        (112, vec![0xFF55]),
        (113, vec![0xFF51]),
        (114, vec![0xFF53]),
        (115, vec![0xFF57]),
        (116, vec![0xFF54]),
        (117, vec![0xFF56]),
        (118, vec![0xFF63]),
        (119, vec![0xFFFF]),
        (69, vec![0xFFC0]),
    ] {
        m.insert(kc, syms);
    }
    m
}

/// Whether Shift went up at any point before the key's own press.
fn shift_lifted(events: &[(u32, u32)], shift_kc: u32, key_kc: u32) -> bool {
    for (kc, state) in events {
        if (*kc, *state) == (key_kc, 1) {
            return false;
        }
        if *kc == shift_kc && *state == 0 {
            return true;
        }
    }
    false
}

/// The three checks on one backend: navigation keeps Shift, a glyph does not, a chord passes.
async fn shift_navigation_on<I, C, K>(
    name: &str,
    h: &Arc<Dispatcher>,
    injected: I,
    clear: C,
    shift_kc: u32,
    ctrl_kc: u32,
    keycode_of: K,
) where
    I: Fn() -> Vec<(u32, u32)>,
    C: Fn(),
    K: Fn(u32) -> u32,
{
    send(h, &[&format!("kd,{SHIFT_L}")], None).await;
    assert!(
        injected().contains(&(shift_kc, 1)),
        "{name}: Shift goes down"
    );
    for (label, keysym) in NAVIGATION {
        clear();
        send(h, &[&format!("kd,{keysym}"), &format!("ku,{keysym}")], None).await;
        let kc = keycode_of(keysym);
        let got = injected();
        assert!(
            got.contains(&(kc, 1)) && !shift_lifted(&got, shift_kc, kc),
            "{name}: Shift+{label} keeps Shift down: {got:?}"
        );
    }
    clear();
    send(h, &["kd,97", "ku,97"], None).await;
    assert!(
        shift_lifted(&injected(), shift_kc, keycode_of(0x61)),
        "{name}: a glyph under a held Shift is still neutralized: {:?}",
        injected()
    );
    send(h, &[&format!("kd,{CONTROL_L}")], None).await;
    clear();
    let left = 0xFF51;
    send(h, &[&format!("kd,{left}"), &format!("ku,{left}")], None).await;
    let got = injected();
    assert!(
        got.contains(&(keycode_of(left), 1))
            && !got.contains(&(shift_kc, 0))
            && !got.contains(&(ctrl_kc, 0)),
        "{name}: Ctrl+Shift+Left passes through untouched: {got:?}"
    );
    send(
        h,
        &[&format!("ku,{CONTROL_L}"), &format!("ku,{SHIFT_L}")],
        None,
    )
    .await;
    let got = injected();
    assert_eq!(
        got.iter().filter(|e| **e == (shift_kc, 0)).count(),
        1,
        "{name}: releasing the chord lifts Shift once: {got:?}"
    );
}

#[tokio::test]
async fn shift_navigation_x11() {
    let link = Arc::new(FakeLink::new(us_core_keymap(), vec![50, 62, 37, 108]));
    let (h, _rec) = x11(link.clone());
    h.state.lock().key_repeat_enabled = false;
    let l = link.clone();
    let c = link.clone();
    let k = link.clone();
    shift_navigation_on(
        "x11",
        &h,
        move || {
            l.presses()
                .iter()
                .map(|(kc, d)| (*kc as u32, *d as u32))
                .collect()
        },
        move || c.clear(),
        50,
        37,
        move |ks| k.keysym_to_keycode(ks) as u32,
    )
    .await;
}

#[tokio::test]
async fn shift_navigation_wayland() {
    let us = keymap_text("us");
    assert!(!us.is_empty(), "the xkb data files are in the sandbox");
    let seat = Arc::new(FakeSeat::new(&us));
    let (h, _rec) = wayland(seat.clone(), false);
    assert!(
        h.ensure_wayland_keymap_owner().await,
        "wayland: owner built from the us base"
    );
    h.start_tasks();
    let kc = |ks: u32| {
        h.state
            .lock()
            .wl_keymap_owner
            .as_ref()
            .unwrap()
            .base_keycode(ks)
            .unwrap()
    };
    let (shift, ctrl) = (kc(SHIFT_L), kc(CONTROL_L));
    let map: HashMap<u32, u32> = NAVIGATION
        .iter()
        .map(|(_, ks)| (*ks, kc(*ks)))
        .chain([(0x61, kc(0x61))])
        .collect();
    let (s1, s2) = (seat.clone(), seat.clone());
    shift_navigation_on(
        "wayland",
        &h,
        move || s1.injected(),
        move || s2.clear(),
        shift,
        ctrl,
        move |ks| map[&ks],
    )
    .await;
    h.disconnect().await;
}

/// Held keys survive a Wayland base-layout change.
#[tokio::test]
async fn held_keys_survive_a_base_layout_change() {
    let (de, us) = (keymap_text("de"), keymap_text("us"));
    assert!(
        !de.is_empty() && !us.is_empty(),
        "the xkb data files are in the sandbox"
    );
    const Z: u32 = 0x7A;
    let seat = Arc::new(FakeSeat::new(&de));
    let (h, _rec) = wayland(seat.clone(), false);
    let owner_kc = |h: &Dispatcher, ks: u32| {
        h.state
            .lock()
            .wl_keymap_owner
            .as_ref()
            .unwrap()
            .base_keycode(ks)
            .unwrap()
    };
    assert!(
        h.ensure_wayland_keymap_owner().await,
        "owner built from the de base"
    );
    let kc_de = owner_kc(&h, Z);
    h.send_keypress(Z, true, None).await;
    h.state.lock().pressed_keys.insert(Z, Instant::now());
    assert_eq!(
        seat.injected().last(),
        Some(&(kc_de, 1)),
        "z pressed at its de keycode"
    );

    seat.set_text(&us);
    h.invalidate_wayland_keymap_owner();
    assert!(h.ensure_wayland_keymap_owner().await);
    let kc_us = owner_kc(&h, Z);
    assert_ne!(
        kc_us, kc_de,
        "the two layouts place z on different keycodes (the test means something)"
    );
    {
        let st = h.state.lock();
        let rebuilt = st.wl_keymap_owner.as_ref().unwrap();
        assert_eq!(
            rebuilt.held_keycode(Z),
            Some(kc_de),
            "rebuilt owner carries the held key"
        );
        assert!(rebuilt.is_down(kc_de));
    }
    h.send_keypress(Z, false, None).await;
    assert_eq!(
        seat.injected().last(),
        Some(&(kc_de, 0)),
        "release lifts the keycode that was pressed"
    );
    assert!(
        seat.injected().iter().all(|(kc, _)| *kc != kc_us),
        "nothing injected at the new keycode"
    );
    {
        let st = h.state.lock();
        let rebuilt = st.wl_keymap_owner.as_ref().unwrap();
        assert!(
            rebuilt.held_keycode(Z).is_none() && !rebuilt.is_down(kc_de),
            "the release untracks the key"
        );
    }

    // A shifted press synthesizes Shift; the refcount rides along.
    seat.set_text(&de);
    h.invalidate_wayland_keymap_owner();
    assert!(h.ensure_wayland_keymap_owner().await);
    seat.clear();
    h.send_keypress(0x5A, true, None).await;
    let key = owner_kc(&h, 0x5A);
    let synth: Vec<u32> = seat
        .injected()
        .iter()
        .filter(|(kc, st)| *st == 1 && *kc != key)
        .map(|(kc, _)| *kc)
        .collect();
    assert_eq!(synth.len(), 1, "shifted press synthesizes a modifier");
    seat.set_text(&us);
    h.invalidate_wayland_keymap_owner();
    assert!(h.ensure_wayland_keymap_owner().await);
    seat.clear();
    h.send_keypress(0x5A, false, None).await;
    assert!(
        seat.injected().contains(&(synth[0], 0)),
        "release after the rebuild lifts the synthesized modifier as well"
    );

    // A reset after the change releases the held key through the rebuilt owner.
    seat.set_text(&de);
    h.invalidate_wayland_keymap_owner();
    assert!(h.ensure_wayland_keymap_owner().await);
    h.send_keypress(Z, true, None).await;
    h.state.lock().pressed_keys.insert(Z, Instant::now());
    seat.set_text(&us);
    h.invalidate_wayland_keymap_owner();
    seat.clear();
    h.reset_keyboard().await;
    assert!(
        seat.injected().contains(&(kc_de, 0)) && h.state.lock().pressed_keys.is_empty(),
        "reset after a layout change lifts the held keycode"
    );
    assert!(
        seat.pressed().is_empty(),
        "the seat has nothing left pressed"
    );

    // Overlay binds are not carried: the compositor rebuilt its keymap without them.
    seat.set_text(&de);
    h.invalidate_wayland_keymap_owner();
    assert!(h.ensure_wayland_keymap_owner().await);
    h.state
        .lock()
        .wl_keymap_owner
        .as_mut()
        .unwrap()
        .overlay_bind(0x0100_0000 | 0x1F600)
        .unwrap();
    seat.set_text(&us);
    h.invalidate_wayland_keymap_owner();
    assert!(h.ensure_wayland_keymap_owner().await);
    assert_eq!(
        h.state
            .lock()
            .wl_keymap_owner
            .as_ref()
            .unwrap()
            .overlay_len(),
        0,
        "overlay binds start empty on the rebuilt owner"
    );
}

/// Where the client `keyboardLayout` hint lands.
#[tokio::test]
async fn layout_hint_lands_on_the_seat_only_without_a_session_compositor() {
    let pushed = |seat: &FakeSeat| seat.layouts.lock().unwrap().clone();
    let tuple = |l: &str, v: &str| {
        (
            l.to_string(),
            v.to_string(),
            String::new(),
            String::new(),
            String::new(),
        )
    };

    let seat = Arc::new(FakeSeat::new(""));
    let (h, _) = wayland(seat.clone(), true);
    h.apply_client_keyboard_layout("de").await;
    h.apply_client_keyboard_layout("de").await;
    assert!(
        pushed(&seat).is_empty(),
        "nested session: hint not pushed onto the seat"
    );
    assert_eq!(
        h.client_keyboard_layout().as_deref(),
        Some("de"),
        "nested session: hint noted"
    );
    assert!(
        h.state.lock().wl_seat_client_layout.is_none(),
        "nested session: seat records no client layout"
    );

    set_topology(&h, false);
    h.apply_client_keyboard_layout("de").await;
    assert_eq!(
        pushed(&seat),
        vec![tuple("de", "")],
        "direct topology later: the noted hint is applied"
    );
    assert_eq!(
        h.state.lock().wl_seat_client_layout.as_deref(),
        Some("de"),
        "seat records the client layout"
    );
    assert!(
        h.state.lock().wl_keymap_stale,
        "keymap owner marked stale for the new base"
    );

    let seat = Arc::new(FakeSeat::new(""));
    let (h, _) = wayland(seat.clone(), false);
    h.apply_client_keyboard_layout("ch(fr)").await;
    h.apply_client_keyboard_layout("ch(fr)").await;
    assert_eq!(
        pushed(&seat),
        vec![tuple("ch", "fr")],
        "direct: layout and variant pushed once"
    );
    h.apply_client_keyboard_layout("us").await;
    assert_eq!(pushed(&seat).len(), 2, "direct: a new value is pushed");
    assert_eq!(pushed(&seat)[1].0, "us");

    let (hx, _) = dispatcher(InputConfig::default(), Some(Arc::new(FakeSeat::new(""))));
    hx.apply_client_keyboard_layout("de").await;
    assert_eq!(
        hx.client_keyboard_layout().as_deref(),
        Some("de"),
        "x11: hint noted, nothing pushed"
    );

    let seat = Arc::new(FakeSeat::new(""));
    let (h, _) = wayland(seat.clone(), false);
    h.apply_client_keyboard_layout("de;rm -rf").await;
    assert!(
        pushed(&seat).is_empty() && h.client_keyboard_layout().is_none(),
        "malformed hint rejected"
    );

    // The deployment layout, as the process env names it.
    let with_env = |layout: &str, seat: Arc<FakeSeat>| {
        let mut cfg = InputConfig {
            is_wayland: true,
            ..Default::default()
        };
        cfg.xkb_defaults[0] = layout.to_string();
        let (h, _) = dispatcher(cfg, Some(seat));
        set_topology(&h, false);
        h
    };
    let seat = Arc::new(FakeSeat::new(""));
    let h = with_env("fr", seat.clone());
    h.apply_client_keyboard_layout("de").await;
    set_topology(&h, true);
    h.schedule_seat_layout_restore();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        pushed(&seat).last().unwrap().0,
        "fr",
        "late session compositor: seat restored to the deployment layout"
    );
    assert!(h.state.lock().wl_seat_client_layout.is_none());
    h.apply_client_keyboard_layout("de").await;
    assert_eq!(
        pushed(&seat).last().unwrap().0,
        "fr",
        "the re-asserted hint stays informational under the session"
    );

    let seat = Arc::new(FakeSeat::new(""));
    let h = with_env("", seat.clone());
    h.apply_client_keyboard_layout("de").await;
    set_topology(&h, true);
    h.schedule_seat_layout_restore();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        pushed(&seat).last(),
        Some(&tuple("", "")),
        "no env layout: restore pushes the xkbcommon default"
    );
    assert!(h.state.lock().wl_seat_client_layout.is_none());

    let seat = Arc::new(FakeSeat::new(""));
    let h = with_env("", seat.clone());
    set_topology(&h, true);
    h.schedule_seat_layout_restore();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        pushed(&seat).is_empty(),
        "nothing to restore when the seat never took a client layout"
    );

    let seat = Arc::new(FakeSeat::new(""));
    let h = with_env("", seat.clone());
    h.push_wayland_base_layout(false).await;
    assert!(
        pushed(&seat).is_empty(),
        "session start without an env layout pushes nothing"
    );
}

/// Deliver `steps`, `(seconds, message)`, on the clock, plus a rest for what they set off.
async fn on_the_clock(h: &Arc<Dispatcher>, steps: &[(f64, String)]) {
    let start = tokio::time::Instant::now();
    for (at, msg) in steps {
        tokio::time::sleep_until(start + Duration::from_secs_f64(*at)).await;
        h.on_message(msg, "primary", A).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    settle(h).await;
}

/// The stall scenarios, `(seconds since the start, down)` read back per backend.
async fn stall_scenarios<I, C>(name: &str, h: &Arc<Dispatcher>, injected: I, reset: C)
where
    I: Fn() -> Vec<(f64, bool)>,
    C: Fn(),
{
    const FIRST_REPEAT_S: f64 = 0.6;
    let (kd, ku, kh) = (
        "kd,101".to_string(),
        "ku,101".to_string(),
        "kh,101".to_string(),
    );
    let downs = |got: &[(f64, bool)]| got.iter().map(|(_, d)| *d).collect::<Vec<_>>();

    reset();
    on_the_clock(h, &[(0.0, kd.clone()), (1.0, ku.clone())]).await;
    let got = injected();
    assert_eq!(
        downs(&got),
        [true, false],
        "{name}: a release a stall holds back is let go before the first repeat: {got:?}"
    );
    assert!(
        got[1].0 < FIRST_REPEAT_S,
        "{name}: before the first repeat: {got:?}"
    );

    reset();
    let mut steps = vec![(0.0, kd.clone())];
    steps.extend((1..=10).map(|i| (0.1 * i as f64, kh.clone())));
    steps.push((1.05, ku.clone()));
    steps.sort_by(|a, b| a.0.total_cmp(&b.0));
    on_the_clock(h, &steps).await;
    let got = injected();
    assert_eq!(
        downs(&got),
        [true, false],
        "{name}: a key whose heartbeats keep coming is let go by its release alone: {got:?}"
    );
    assert!(got[1].0 > 1.0, "{name}: by its release: {got:?}");

    reset();
    on_the_clock(
        h,
        &[
            (0.0, kd.clone()),
            (1.0, kh.clone()),
            (1.0, kh.clone()),
            (1.0, ku.clone()),
        ],
    )
    .await;
    let got = injected();
    assert_eq!(
        downs(&got),
        [true, false],
        "{name}: a heartbeat arriving with the held-back release presses nothing again: {got:?}"
    );
    assert!(got[1].0 < FIRST_REPEAT_S);

    reset();
    on_the_clock(
        h,
        &[
            (0.0, kd.clone()),
            (1.0, kh.clone()),
            (1.1, kh.clone()),
            (1.2, kh.clone()),
            (1.3, ku.clone()),
        ],
    )
    .await;
    let got = injected();
    assert_eq!(
        downs(&got),
        [true, false, true, false],
        "{name}: a heartbeat after a stall presses the held key again until its release: {got:?}"
    );

    reset();
    on_the_clock(
        h,
        &[
            (0.0, kd.clone()),
            (1.0, kh.clone()),
            (2.0, kh.clone()),
            (3.0, kh.clone()),
            (3.5, ku.clone()),
        ],
    )
    .await;
    let got = injected();
    assert_eq!(
        downs(&got),
        [true, false],
        "{name}: a hidden tab's heartbeat, one a second, leaves the key up: {got:?}"
    );
}

/// A stalled release, X11.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_release_types_once_x11() {
    let link = Arc::new(FakeLink::new(HashMap::from([(26u8, vec![0x65])]), vec![]));
    let (h, _) = x11(link.clone());
    h.state.lock().key_repeat_enabled = false;
    h.start_tasks();
    let (l, c) = (link.clone(), link.clone());
    stall_scenarios(
        "x11",
        &h,
        move || {
            l.recorded
                .lock()
                .unwrap()
                .keys
                .iter()
                .map(|(_, d, at)| (*at, *d))
                .collect()
        },
        move || c.clear(),
    )
    .await;
    h.disconnect().await;
}

/// A stalled release, Wayland.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_release_types_once_wayland() {
    let us = keymap_text("us");
    assert!(!us.is_empty(), "the xkb data files are in the sandbox");
    let seat = Arc::new(FakeSeat::new(&us));
    let (h, _) = wayland(seat.clone(), false);
    h.start_tasks();
    let start = Arc::new(Mutex::new(Instant::now()));
    let (s1, s2, t1, t2) = (seat.clone(), seat.clone(), start.clone(), start.clone());
    stall_scenarios(
        "wayland",
        &h,
        move || {
            let at = *t1.lock().unwrap();
            s1.injected()
                .iter()
                .zip(s1.stamps.lock().unwrap().iter())
                .map(|((_, st), when)| (when.duration_since(at).as_secs_f64(), *st == 1))
                .collect()
        },
        move || {
            s2.clear();
            *t2.lock().unwrap() = Instant::now();
        },
    )
    .await;
    h.disconnect().await;
}

/// Numbered pointer messages over a channel that keeps no order.
#[tokio::test]
async fn numbered_pointer_messages() {
    let link = Arc::new(FakeLink::new(HashMap::new(), vec![]));
    let (h, _) = x11(link.clone());
    let motions = |l: &FakeLink| l.recorded.lock().unwrap().motions.clone();
    for (msg, conn) in [
        ("m,10,10,0,0,1", A),
        ("m,30,30,0,0,3", A),
        ("m,20,20,0,0,2", A),
        ("m2,5,5,0,0,2", A),
        ("m,40,40,1,0,4", A),
        ("m,7,7,0,0", A),
        ("m,50,50,0,0,1", B),
        ("m,60,60,0,0,x", A),
    ] {
        send(&h, &[msg], conn).await;
    }
    let m = motions(&link);
    assert_eq!(
        m[..2],
        [(10, 10, false), (30, 30, false)],
        "positions apply in order"
    );
    assert!(
        !m.contains(&(20, 20, false)),
        "a position numbered below the last applied is dropped"
    );
    assert!(
        m.contains(&(5, 5, true)),
        "a delta numbered below the last applied still counts"
    );
    assert!(m.contains(&(40, 40, false)), "a later click is applied");
    assert_eq!(
        link.recorded.lock().unwrap().buttons[..1],
        [(1, true)],
        "and its button goes down"
    );
    assert!(
        m.contains(&(7, 7, false)),
        "an unnumbered message applies as before"
    );
    assert!(
        m.contains(&(50, 50, false)),
        "another connection counts on its own"
    );
    assert!(
        !m.contains(&(60, 60, false)),
        "an unreadable number drops the message"
    );
    assert_eq!(
        h.state.lock().pointer_seq.get(&1),
        Some(&4),
        "the count is the last number each connection sent"
    );

    send(&h, &["m,1,1,0,0,9"], Some(3)).await;
    assert_eq!(
        h.state.lock().pointer_seq.get(&3),
        Some(&9),
        "a connection's count is kept until it goes"
    );
    h.release_conn(3);
    assert!(
        !h.state.lock().pointer_seq.contains_key(&3),
        "and forgotten when it does"
    );

    // A delta that lands after the release that followed it still moves the pointer, but the
    // button it carried is not pressed again.
    let link = Arc::new(FakeLink::new(HashMap::new(), vec![]));
    let (h, _) = x11(link.clone());
    send(
        &h,
        &[
            "m2,0,0,1,0,20",
            "m2,4,0,1,0,22",
            "m2,0,0,0,0,23",
            "m2,3,0,1,0,21",
        ],
        Some(4),
    )
    .await;
    assert!(
        motions(&link).contains(&(3, 0, true)),
        "a late delta still moves the pointer"
    );
    assert_eq!(
        link.recorded.lock().unwrap().buttons,
        [(1, true), (1, false)],
        "but does not press again the button released after it"
    );

    // A wheel pulse overtaken by later motion is a click, not a state: its scroll bit still
    // rises, over the buttons held now rather than those it carried.
    link.recorded.lock().unwrap().buttons.clear();
    send(
        &h,
        &[
            "m2,0,0,1,0,30",
            "m2,5,0,0,0,33",
            "m2,0,0,1,1,31",
            "m2,0,0,9,1,32",
        ],
        Some(5),
    )
    .await;
    let buttons = link.recorded.lock().unwrap().buttons.clone();
    assert_eq!(
        buttons,
        [(1, true), (1, false), (5, true), (5, false)],
        "a late wheel pulse still scrolls, without pressing again the button held when it was sent"
    );
}

/// What each backend injects as the pointer is tracked; the tracking itself is in `pointer`.
#[tokio::test]
async fn pointer_tracking_on_each_backend() {
    let single = layouts(&[("primary", 0, 0, 1920, 1080)]);
    let seat = Arc::new(FakeSeat::new(""));
    let (h, rec) = wayland(seat.clone(), false);
    *rec.layouts.lock().unwrap() = single.clone();
    send(&h, &["m,900,500,0,0"], None).await;
    send(&h, &["m2,4000,0,0,0"], None).await;
    assert_eq!(
        seat.moves.lock().unwrap().last(),
        Some(&("relative", 4000.0, 0.0)),
        "a backend with relative injection gets the delta verbatim"
    );
    assert_eq!(
        h.state.lock().pointer.last_x,
        1919,
        "the tracked position still stops at the edge"
    );
    let before = seat.moves.lock().unwrap().len();
    send(&h, &["m2,0,0,1,0"], None).await;
    assert_eq!(
        seat.moves.lock().unwrap().len(),
        before,
        "wayland: a zero delta injects no motion"
    );
    assert_eq!(
        *seat.buttons.lock().unwrap(),
        [(272, 1)],
        "wayland: the button still goes down"
    );

    let link = Arc::new(FakeLink::new(HashMap::new(), vec![]));
    let (h, rec) = x11(link.clone());
    *rec.layouts.lock().unwrap() = single;
    let motions = |l: &FakeLink| l.recorded.lock().unwrap().motions.clone();
    send(&h, &["m,900,500,0,0"], None).await;
    let before = motions(&link).len();
    send(&h, &["m2,0,0,1,0"], None).await;
    assert!(
        motions(&link)[before..].iter().all(|m| !m.2),
        "x11: a zero delta injects no motion"
    );
    send(&h, &["m2,0,0,0,0", "m2,300,0,0,0", "m2,-300,0,0,0"], None).await;
    let before = motions(&link).len();
    send(&h, &["m,900,500,0,0"], None).await;
    assert_eq!(
        motions(&link)[before..],
        [(900, 500, false)],
        "an absolute position after deltas warps even where it was tracked"
    );
    let before = motions(&link).len();
    send(&h, &["m,900,500,0,0"], None).await;
    assert_eq!(
        motions(&link).len(),
        before,
        "a repeat of it does not warp again"
    );
    send(&h, &["m2,99999,0,0,0"], None).await;
    assert_eq!(
        motions(&link).last(),
        Some(&(32767, 0, true)),
        "a delta too large for the request is saturated, not dropped"
    );
}

/// One pointer message, one warp, one flush; `--` before typed text.
#[tokio::test]
async fn pointer_message_is_one_request_and_one_flush() {
    let link = Arc::new(FakeLink::new(HashMap::new(), vec![]));
    let (h, _) = x11(link.clone());
    settle(&h).await;
    link.recorded.lock().unwrap().flushes = 0;
    send(&h, &["m,100,200,0,0"], None).await;
    let r = link.recorded.lock().unwrap();
    assert_eq!(
        r.motions,
        [(100, 200, false)],
        "an absolute move queues one warp"
    );
    assert_eq!(r.flushes, 1, "and flushes once");
    assert_eq!(
        xdotool_type_args("--delay 5"),
        ["type", "--", "--delay 5"],
        "xdotool type gets '--' before the text"
    );
}

/// Stands in for pixelflux: moves are numbered as they are injected and applied only when
/// `apply` says, as the compositor thread would, a relative one by `gain` (a compositor's own
/// acceleration) times the output's scale, held inside the 1920x1080 primary.
struct Compositor {
    state: Mutex<CompositorState>,
}

struct CompositorState {
    x: f64,
    y: f64,
    scale: f64,
    gain: f64,
    numbered: u64,
    applied: u64,
    queue: Vec<(u64, bool, f64, f64)>,
    unknown: bool,
}

impl Compositor {
    fn new(scale: f64, gain: f64) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CompositorState {
                x: 100.0,
                y: 100.0,
                scale,
                gain,
                numbered: 0,
                applied: 0,
                queue: Vec::new(),
                unknown: false,
            }),
        })
    }

    fn apply(&self) {
        let mut s = self.state.lock().unwrap();
        for (number, relative, a, b) in std::mem::take(&mut s.queue) {
            if relative {
                let k = s.gain * s.scale;
                s.x = (s.x + a * k).clamp(0.0, 1919.0);
                s.y = (s.y + b * k).clamp(0.0, 1079.0);
            } else {
                s.x = a;
                s.y = b;
            }
            s.applied = number;
        }
    }

    fn at(&self) -> (f64, f64) {
        let s = self.state.lock().unwrap();
        (s.x, s.y)
    }

    fn warp(&self, x: f64, y: f64) {
        let mut s = self.state.lock().unwrap();
        s.x = x;
        s.y = y;
    }

    fn number(&self, relative: bool, a: f64, b: f64) -> Result<Option<u64>, Error> {
        let mut s = self.state.lock().unwrap();
        s.numbered += 1;
        let n = s.numbered;
        s.queue.push((n, relative, a, b));
        Ok(Some(n))
    }
}

impl WaylandSeat for Compositor {
    fn inject_key(&self, _kc: u32, _state: u32) -> Result<(), Error> {
        Ok(())
    }
    fn inject_keys(&self, _events: Vec<(u32, u32)>) -> Result<(), Error> {
        Ok(())
    }
    fn set_keymap_overlay(&self, _binds: Vec<(u32, u32)>) -> Result<(), Error> {
        Ok(())
    }
    fn get_xkb_keymap_string(&self) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_xkb_layout(
        &self,
        _l: String,
        _v: String,
        _o: String,
        _m: String,
        _r: String,
    ) -> Result<bool, Error> {
        Ok(true)
    }
    fn get_keyboard_state(&self) -> Result<(Vec<u32>, u32), Error> {
        Ok((Vec::new(), 0))
    }
    fn type_keysyms_wayland(&self, _display: String, _keysyms: Vec<u32>) -> Result<(), Error> {
        Ok(())
    }
    fn inject_mouse_move(&self, x: f64, y: f64) -> Result<Option<u64>, Error> {
        self.number(false, x, y)
    }
    fn inject_relative_mouse_move(&self, dx: f64, dy: f64) -> Result<Option<u64>, Error> {
        self.number(true, dx, dy)
    }
    fn inject_mouse_button(&self, _button: u32, _state: u32) -> Result<(), Error> {
        Ok(())
    }
    fn inject_mouse_scroll(&self, _x: f64, _y: f64) -> Result<(), Error> {
        Ok(())
    }
    fn inject_finger_scroll(&self, _dx: f64, _dy: f64) -> Result<(), Error> {
        Ok(())
    }
    fn inject_finger_scroll_end(&self) -> Result<(), Error> {
        Ok(())
    }
    fn pointer_location(&self) -> Option<(f64, f64, f64, u64)> {
        let s = self.state.lock().unwrap();
        (!s.unknown).then_some((s.x, s.y, s.scale, s.applied))
    }
    fn set_app_wayland_display(&self, _display: String) {}
    fn clipboard_unwatch_app(&self, _display: String) {}
}

fn echo_layouts() -> Layouts {
    layouts(&[
        ("primary", 0, 0, 1920, 1080),
        ("display2", 1920, 0, 1280, 720),
    ])
}

fn rest() -> Duration {
    POINTER_ECHO_POLL * 3
}

/// The echo a trackpad page draws its cursor from, on Wayland.
#[tokio::test]
async fn pointer_echo_wayland() {
    let comp = Compositor::new(1.0, 1.0);
    let (h, rec) = wayland(comp.clone(), false);
    *rec.layouts.lock().unwrap() = echo_layouts();
    send(&h, &["_pointer_echo,1"], A).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,primary,100.0,100.0,1,0"],
        "a subscriber is sent the position at once"
    );
    assert!(
        h.state.lock().echo_poll_running,
        "and the pointer is read again while it is watched"
    );

    for seq in 1..=3 {
        send(&h, &[&format!("m2,10,5,0,0,{seq}")], A).await;
        tokio::time::sleep(POINTER_ECHO_MIN * 2).await;
        comp.apply();
    }
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,primary,100.0,100.0,1,0"],
        "moves the page draws ahead of the echo, applied as it draws them, send nothing"
    );
    assert_eq!(comp.at(), (130.0, 115.0));

    comp.warp(2000.0, 50.0);
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A).last().unwrap(),
        "pointer,display2,80.0,50.0,1,3",
        "an application's warp reaches the page with no motion sent, with the messages the position includes"
    );
    let count = rec.echoes_to(A).len();
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A).len(),
        count,
        "an unchanged pointer is not echoed again"
    );

    send(&h, &["m,50,60,0,0,4"], B).await;
    tokio::time::sleep(POINTER_ECHO_MIN * 2).await;
    comp.apply();
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A).last().unwrap(),
        "pointer,primary,50.0,60.0,1,3",
        "another connection's move is echoed, and the subscriber keeps its own count"
    );
    assert!(
        rec.echoes_to(B).is_empty(),
        "and a connection that did not ask is sent nothing"
    );

    h.release_conn(1);
    assert!(
        h.state.lock().pointer_echoes.is_empty(),
        "an echo stops with its connection"
    );
    tokio::time::sleep(rest()).await;
    assert!(!h.state.lock().echo_poll_running, "and so does the reading");
}

/// The echo: moves the page cannot predict, and ones it can at a display's edge.
#[tokio::test]
async fn pointer_echo_surprises() {
    let comp = Compositor::new(1.0, 1.5);
    let (h, rec) = wayland(comp.clone(), false);
    *rec.layouts.lock().unwrap() = echo_layouts();
    send(&h, &["_pointer_echo,1", "m2,10,0,0,0,1"], A).await;
    comp.apply();
    send(&h, &["m2,10,0,0,0,2"], A).await;
    assert_eq!(
        rec.echoes_to(A).len(),
        1,
        "a move inside the interval is not echoed at once"
    );
    tokio::time::sleep(POINTER_ECHO_MIN * 3).await;
    assert_eq!(
        rec.echoes_to(A).last().unwrap(),
        "pointer,primary,115.0,100.0,1,1",
        "but when it ends, where the compositor's gain left the pointer elsewhere"
    );
    comp.apply();
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A).last().unwrap(),
        "pointer,primary,130.0,100.0,1,2",
        "naming the last message the applied moves include"
    );
    h.release_conn(1);

    let edge = Compositor::new(1.0, 1.0);
    edge.warp(1915.0, 100.0);
    let (h, rec) = wayland(edge.clone(), false);
    *rec.layouts.lock().unwrap() = echo_layouts();
    send(&h, &["_pointer_echo,1", "m2,10,0,0,0,1"], A).await;
    edge.apply();
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,primary,1915.0,100.0,1,0"],
        "a move the display's edge stops, which the page stops too, sends nothing"
    );
    assert_eq!(edge.at().0, 1919.0);
    send(&h, &["m2,-6,0,0,0,2"], A).await;
    edge.apply();
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,primary,1915.0,100.0,1,0"],
        "nor does one back from it, which the page draws from the edge as well"
    );
    assert_eq!(edge.at().0, 1913.0);
    h.release_conn(1);
}

/// The echo: a scaled output, and a session that cannot say.
#[tokio::test]
async fn pointer_echo_scale_and_unknown() {
    let comp = Compositor::new(2.0, 1.0);
    let (h, rec) = wayland(comp.clone(), false);
    *rec.layouts.lock().unwrap() = echo_layouts();
    send(&h, &["_pointer_echo,1"], A).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,primary,100.0,100.0,2,0"],
        "the echo names how far a relative pixel moves the pointer"
    );
    send(&h, &["m2,10,0,0,0,1"], A).await;
    comp.apply();
    tokio::time::sleep(rest()).await;
    assert_eq!(
        rec.echoes_to(A).len(),
        1,
        "and a move scaled by it is the page's to draw"
    );
    assert_eq!(comp.at().0, 120.0);
    send(&h, &["_pointer_echo,0"], A).await;
    assert!(
        !h.state.lock().pointer_echoes.contains_key(&1),
        "a page that stops asking is dropped"
    );

    let comp = Compositor::new(1.0, 1.0);
    comp.state.lock().unwrap().unknown = true;
    let (h, rec) = wayland(comp, false);
    send(&h, &["_pointer_echo,1"], A).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,none"],
        "a session that cannot say where its pointer is answers none"
    );
    assert!(h.state.lock().pointer_echoes.is_empty());
}

/// The X server's pointer, counting how often it was asked and from which thread.
struct XServer {
    at: Mutex<(i16, i16)>,
    queries: Mutex<u32>,
    threads: Mutex<Vec<ThreadId>>,
}

impl PointerReader for XServer {
    fn read(&self) -> Option<(i16, i16)> {
        *self.queries.lock().unwrap() += 1;
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        Some(*self.at.lock().unwrap())
    }
    fn close(&self) {}
}

/// The echo on X11: it compares the position it tracks while the pointer moves and asks the server
/// once it rests, from a worker thread.
#[tokio::test]
async fn pointer_echo_x11() {
    let link = Arc::new(FakeLink::new(HashMap::new(), vec![]));
    let (h, rec) = x11(link);
    *rec.layouts.lock().unwrap() = echo_layouts();
    let server = Arc::new(XServer {
        at: Mutex::new((300, 200)),
        queries: Mutex::new(0),
        threads: Mutex::new(Vec::new()),
    });
    h.state.lock().pointer_reader = Some(server.clone());
    let queries = |s: &XServer| *s.queries.lock().unwrap();

    send(&h, &["_pointer_echo,1"], A).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,primary,300.0,200.0,1,0"],
        "X11 asks the server where the pointer is on subscribing"
    );
    assert_eq!(queries(&server), 1);
    {
        let st = h.state.lock();
        assert_eq!(
            (st.pointer.last_x, st.pointer.last_y),
            (300, 200),
            "and tracks from its answer"
        );
    }
    for seq in 1..=5 {
        send(&h, &[&format!("m2,4,2,0,0,{seq}")], A).await;
        tokio::time::sleep(POINTER_ECHO_MIN + POINTER_ECHO_MIN / 2).await;
    }
    assert_eq!(
        queries(&server),
        1,
        "a moving pointer is compared from the tracking, with no round trip"
    );
    assert_eq!(rec.echoes_to(A).len(), 1, "and the page drew it");
    *server.at.lock().unwrap() = (330, 214);
    tokio::time::sleep(rest()).await;
    assert!(queries(&server) >= 2);
    assert_eq!(
        rec.echoes_to(A).last().unwrap(),
        "pointer,primary,330.0,214.0,1,5",
        "a resting pointer is read from the server, and echoed where it is not where the page drew it"
    );
    send(&h, &["m2,1,1,0,0,6"], A).await;
    {
        let st = h.state.lock();
        assert_eq!(
            (st.pointer.last_x, st.pointer.last_y),
            (331, 215),
            "and the tracking continues from that answer"
        );
    }
    *server.at.lock().unwrap() = (3500, 900);
    tokio::time::sleep(rest()).await;
    let runtime = std::thread::current().id();
    assert!(
        server.threads.lock().unwrap().iter().all(|t| *t != runtime),
        "the server is only ever asked from a worker thread, never the runtime"
    );
    let last = rec.echoes_to(A).last().unwrap().clone();
    assert_eq!(
        last.split(',').collect::<Vec<_>>()[1..4],
        ["display2", "1279.0", "719.0"],
        "a point outside every display is placed on the nearest, clamped into it"
    );
    h.release_conn(1);
}

/// A touchpad's scroll as a finger's.
#[tokio::test]
async fn finger_scroll() {
    let seat = Arc::new(FakeSeat::new(""));
    let (h, _) = wayland(seat.clone(), false);
    let finger = |s: &FakeSeat| s.finger.lock().unwrap().clone();
    assert!(
        h.finger_scroll_available(),
        "a Wayland session with the injection takes a finger's scroll"
    );
    send(&h, &["sf,1.5,-2.25", "sfe"], A).await;
    assert_eq!(
        finger(&seat),
        [Some((1.5, -2.25)), None],
        "the travel and the lift reach the seat"
    );
    seat.finger.lock().unwrap().clear();
    send(&h, &["sf,1e9,-1e9"], A).await;
    assert_eq!(
        finger(&seat),
        [Some((FINGER_SCROLL_MAX_PX, -FINGER_SCROLL_MAX_PX))],
        "a travel past the bound is held to it"
    );
    seat.finger.lock().unwrap().clear();
    send(&h, &["sf,nan,1", "sf,inf,1", "sf,x,1", "sf,1"], A).await;
    assert!(
        finger(&seat).is_empty(),
        "a travel that is not two finite numbers is dropped"
    );

    h.state.lock().keyboard_busy = true;
    h.on_message("sf,4,0", "primary", A).await;
    h.on_message("sfe", "primary", A).await;
    let queued: Vec<Option<(f64, f64)>> = h
        .state
        .lock()
        .wl_queue
        .iter()
        .map(|item| match item {
            KeyItem::FingerScroll(delta) => *delta,
            _ => panic!("only the scroll is queued"),
        })
        .collect();
    assert_eq!(
        queued,
        [Some((4.0, 0.0)), None],
        "behind a key the keyboard worker has yet to type, both wait their turn there"
    );
    assert!(finger(&seat).is_empty());
    h.state.lock().keyboard_busy = false;
    h.start_tasks();
    settle(&h).await;
    assert_eq!(
        finger(&seat),
        [Some((4.0, 0.0)), None],
        "and the worker gives them to the seat in order"
    );
    h.disconnect().await;

    let (x, _) = dispatcher(InputConfig::default(), Some(seat.clone()));
    seat.finger.lock().unwrap().clear();
    send(&x, &["sf,1,1", "sfe"], A).await;
    assert!(
        !x.finger_scroll_available() && finger(&seat).is_empty(),
        "X11 takes no finger's scroll"
    );
}

/// Three unbound letters cost the runtime nothing but the binds.
#[tokio::test]
async fn ime_binds_keep_the_runtime_free() {
    let modmap = vec![50, 62, 66, 37, 105, 64, 108, 77, 133, 134, 92];
    let bound: HashMap<u8, Vec<u32>> = (38u8..64)
        .map(|kc| (kc, vec![0x61 + kc as u32 - 38]))
        .collect();
    let link = Arc::new(FakeLink::new(bound, modmap));
    let (h, _) = x11(link.clone());
    const HANGUL: [u32; 3] = [0x100D55C, 0x100AD6D, 0x100C5B4];
    let ticks = Arc::new(Mutex::new(Vec::new()));
    let t = ticks.clone();
    let ticker = tokio::spawn(async move {
        loop {
            t.lock().unwrap().push(Instant::now());
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let start = Instant::now();
    for ks in HANGUL {
        h.send_keypress(ks, true, None).await;
        h.send_keypress(ks, false, None).await;
    }
    let took = start.elapsed();
    ticker.abort();
    let end = start + took;
    let mut points = vec![start];
    points.extend(
        ticks
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|t| *t >= start),
    );
    points.push(end);
    let gap = points
        .windows(2)
        .map(|w| w[1].saturating_duration_since(w[0]))
        .max()
        .unwrap();
    let typed = link.typed();
    assert_eq!(typed, HANGUL, "three unbound letters are typed whole");
    assert!(
        took >= super::x11_keyboard::BIND_SETTLE.mul_f64(2.7),
        "each waits out its bind's settle: {took:?}"
    );
    assert!(
        gap < Duration::from_millis(15),
        "with the runtime free throughout: longest gap {gap:?}"
    );
}

/// The Cyrillic chord translation, atomic keys, and the Alt+Arrow mouse buttons on X11.
#[tokio::test]
async fn x11_key_classification() {
    let link = Arc::new(FakeLink::new(us_core_keymap(), vec![50, 62, 37, 108]));
    let (h, _) = x11(link.clone());
    h.state.lock().key_repeat_enabled = false;
    send(
        &h,
        &[
            &format!("kd,{CONTROL_L}"),
            "kd,1747",
            "ku,1747",
            &format!("ku,{CONTROL_L}"),
        ],
        A,
    )
    .await;
    assert_eq!(
        link.presses(),
        [(37, true), (54, true), (54, false), (37, false)],
        "Ctrl+Cyrillic_es reaches the application as Ctrl+c"
    );

    link.clear();
    send(&h, &["kd,49"], A).await;
    assert_eq!(
        link.presses(),
        [(10, true), (10, false)],
        "a digit is typed whole"
    );
    assert!(h.state.lock().atomically_typed_keys.contains(&0x31));
    send(&h, &["ku,49"], A).await;
    assert_eq!(link.presses().len(), 2, "and its release injects nothing");
    assert!(!h.state.lock().atomically_typed_keys.contains(&0x31));

    link.clear();
    send(&h, &["m,1,1,16,0"], A).await;
    assert_eq!(
        link.presses(),
        [(64, true), (114, true), (114, false), (64, false)],
        "the forward button is Alt+Right, whole"
    );
}

/// The gates in front of the handler, and what the handler hands on.
#[tokio::test]
async fn receive_gates_and_verbs() {
    let pads = Arc::new(Pads::default());
    let rec = Arc::new(Rec::default());
    let seat = Arc::new(FakeSeat::new(""));
    let cfg = InputConfig {
        is_wayland: true,
        command_enabled: false,
        ..Default::default()
    };
    let h = Dispatcher::new(
        cfg,
        rec.clone(),
        pads.clone(),
        rec.clone(),
        Some(seat),
        None,
    );
    set_topology(&h, false);

    *rec.sender.lock().unwrap() = Sender {
        role: Some(Role::Viewer),
        ..Default::default()
    };
    assert_eq!(
        h.receive("kd,97", "primary", A).await,
        Admission::UnauthorizedViewer
    );
    assert!(
        h.state.lock().pressed_keys.is_empty(),
        "a refused key is not tracked"
    );
    assert_eq!(
        h.receive("kr", "primary", A).await,
        Admission::DroppedSilently
    );
    assert_eq!(
        h.receive("SETTINGS,{}", "primary", A).await,
        Admission::Allowed
    );
    assert!(
        rec.calls().is_empty(),
        "a viewer's settings never reach the display"
    );
    assert_eq!(
        h.receive("js,b,0,1,1", "primary", A).await,
        Admission::GamepadSlot,
        "a viewer without a slot drives none"
    );
    *rec.sender.lock().unwrap() = Sender {
        role: Some(Role::Viewer),
        slot: Some(serde_json::json!(1)),
        ..Default::default()
    };
    assert_eq!(
        h.receive("js,b,0,1,1", "primary", A).await,
        Admission::Allowed
    );
    assert_eq!(
        *pads.0.lock().unwrap(),
        ["js,b,0,1,1"],
        "the slots get the message whole"
    );

    *rec.sender.lock().unwrap() = Sender {
        role: Some(Role::Controller),
        input_authorized: true,
        ..Default::default()
    };
    for msg in [
        "r,1920x1080",
        "r,bad",
        "s,1.5",
        "s,1.5.0",
        "vb,4000",
        "vb,-1",
        "ab,128",
        "_arg_fps,60",
        "_arg_fps,inf",
        "_f,59",
        "_l,12",
        "_rc,crf",
        "_rc,nope",
        "_crf,25",
        "_crf,99",
        "SETTINGS,{\"a\":1,\"b\":2}",
        "SET_NATIVE_CURSOR_RENDERING,true",
        "p,0",
        "REQUEST_KEYFRAME",
        "vp,1,2,1.5,1.5",
        "_stats_video,a,b",
        "pong",
        "cmd,true",
    ] {
        h.receive(msg, "primary", A).await;
    }
    assert_eq!(
        rec.calls(),
        [
            "resize 1920x1080 primary",
            "scale 1.5 primary",
            "vb 4000 primary",
            "ab 128",
            "fps 60 primary",
            "client_fps 59",
            "client_latency 12",
            "rc crf primary",
            "crf 25 primary",
            "settings {\"a\":1,\"b\":2} primary",
            "visible true",
            "visible false",
            "keyframe primary",
            "box primary 1 2 1.5 1.5",
            "_stats_video a,b",
        ],
        "every verb reaches its callback, and a value out of bounds does not"
    );
    assert!(
        rec.sent.lock().unwrap().is_empty(),
        "cmd stays off while command_enabled is off"
    );

    for extreme in [
        "m,99999999999,-99999999999,0,0",
        "m2,99999999999,-99999999999,0,0",
        "kd,-1",
        "kd,99999999999",
        "kd,4294967295",
        "ku,4294967295",
    ] {
        h.on_message(extreme, "primary", A).await;
    }
    for malformed in [
        "kd", "kd,x", "ku,", "m", "m,1", "p", "_f", "_crf,x", "co", "co,", "vp,1", "s", "r", "",
        ",",
    ] {
        h.on_message(malformed, "primary", A).await;
    }
}

/// A key the seat cannot place, under a nested compositor, rides the virtual keyboard's batch;
/// a compositor without the protocol is left alone on a cooldown.
#[tokio::test]
async fn nested_compositor_routes_text_to_the_virtual_keyboard() {
    let us = keymap_text("us");
    assert!(!us.is_empty(), "the xkb data files are in the sandbox");
    let seat = Arc::new(FakeSeat::new(&us));
    let (h, _) = wayland(seat.clone(), true);
    h.start_tasks();
    let hangul = 0x0100_D55Cu32;
    send(
        &h,
        &[
            &format!("kd,{hangul}"),
            &format!("ku,{hangul}"),
            "co,end,ok",
        ],
        A,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        *seat.typed.lock().unwrap(),
        [vec![hangul, 0x6F, 0x6B]],
        "one batch, in order, for the two commits"
    );
    assert!(
        seat.injected().is_empty(),
        "and nothing on the seat, whose overlay the session never sees"
    );

    send(&h, &["kd,97", "ku,97"], A).await;
    assert_eq!(
        seat.injected().len(),
        2,
        "a key the base layout carries stays on the seat"
    );

    *seat.typer_unavailable.lock().unwrap() = true;
    send(&h, &["co,end,x"], A).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        h.state.lock().wl_typer_retry_at.is_some(),
        "a compositor without the protocol is re-probed on a cooldown"
    );
    h.disconnect().await;
}

/// The Wayland reset releases what is held, on the worker, behind the keys queued before it.
#[tokio::test]
async fn wayland_reset_rides_the_worker() {
    let us = keymap_text("us");
    assert!(!us.is_empty(), "the xkb data files are in the sandbox");
    let seat = Arc::new(FakeSeat::new(&us));
    let (h, _) = wayland(seat.clone(), false);
    h.start_tasks();
    h.on_message("kd,97", "primary", A).await;
    h.on_message(&format!("kd,{SHIFT_L}"), "primary", A).await;
    h.reset_keyboard().await;
    assert!(
        seat.pressed().is_empty(),
        "the seat has nothing left pressed: {:?}",
        seat.pressed()
    );
    {
        let st = h.state.lock();
        assert!(
            st.pressed_keys.is_empty() && st.active_modifiers.is_empty(),
            "and the sweep has nothing to release"
        );
    }
    h.disconnect().await;
}

/// A chord the mouse's back button synthesizes is one queue entry on Wayland.
#[tokio::test]
async fn back_button_is_one_chord_on_the_queue() {
    let seat = Arc::new(FakeSeat::new(""));
    let (h, _) = wayland(seat.clone(), false);
    h.on_message("m,1,1,8,0", "primary", A).await;
    assert_eq!(h.state.lock().wl_queue.len(), 1);
    assert!(
        matches!(h.state.lock().wl_queue.front(), Some(KeyItem::Chord(keys)) if keys.len() == 4)
    );
    h.on_message("m,1,1,8,3", "primary", A).await;
    h.on_message("m,1,1,0,3", "primary", A).await;
    h.on_message("m,1,1,16,3", "primary", A).await;
    assert_eq!(
        *seat.scrolls.lock().unwrap(),
        Vec::<(f64, f64)>::new(),
        "wheel clicks wait behind the queued chord"
    );
    assert_eq!(h.state.lock().wl_queue.len(), 2);
    h.on_message("m,1,1,32,0", "primary", A).await;
    assert_eq!(
        *seat.buttons.lock().unwrap(),
        [(272, 1)],
        "the eraser clicks like the pen tip"
    );
}

/// Against a real X server: XTEST reaches the server, an overlay bind lands, and the pointer and
/// the keymap read back. Mutates the keymap, so it wants a server of its own.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a private X server on DISPLAY: it rebinds keys and switches layouts"]
async fn real_x_server_round_trip() {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    let (h, rec) = dispatcher(InputConfig::default(), None);
    h.connect().await;
    assert!(h.x().is_some(), "the handler connects");
    *rec.layouts.lock().unwrap() = layouts(&[("primary", 0, 0, 1024, 768)]);

    // A second client sees what the injection did.
    let (probe, screen) = x11rb::rust_connection::RustConnection::connect(None).unwrap();
    let root = probe.setup().roots[screen].root;
    let pointer = || {
        let p = probe.query_pointer(root).unwrap().reply().unwrap();
        (p.root_x, p.root_y)
    };
    let key_down = |kc: u8| {
        let keys = probe.query_keymap().unwrap().reply().unwrap().keys;
        keys[kc as usize / 8] & (1 << (kc % 8)) != 0
    };

    send(&h, &["m,123,234,0,0"], A).await;
    assert_eq!(
        pointer(),
        (123, 234),
        "an absolute move warps the server's pointer"
    );
    send(&h, &["m2,7,-4,0,0"], A).await;
    assert_eq!(pointer(), (130, 230), "a delta moves it by the delta");

    send(&h, &["kd,97"], A).await;
    let a = h
        .x()
        .unwrap()
        .call(|s| s.link().keysym_to_keycode(0x61))
        .await
        .unwrap();
    assert!(a != 0 && key_down(a), "a key goes down on the server");
    send(&h, &["ku,97"], A).await;
    assert!(!key_down(a), "and up");

    let hangul = 0x0100_D55Cu32;
    send(&h, &[&format!("kd,{hangul}")], A).await;
    let (kc, value) = h
        .x()
        .unwrap()
        .call(move |s| {
            let kc = s.keyboard.overlay_binding(hangul).unwrap_or(0);
            let rows = s.link().keyboard_mapping(kc, 1).unwrap();
            (kc, rows[0][0])
        })
        .await
        .unwrap();
    assert!(
        kc != 0 && value == hangul,
        "an overlay bind lands in the server's map: {kc} {value:#x}"
    );
    assert!(key_down(kc), "and its key goes down");
    send(&h, &[&format!("ku,{hangul}")], A).await;
    assert!(!key_down(kc));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let bound = h
        .x()
        .unwrap()
        .call(move |s| s.keyboard.overlay_binding(hangul))
        .await
        .unwrap();
    assert_eq!(
        bound,
        Some(kc),
        "the shim's own notify leaves the overlay alone"
    );

    send(&h, &["_pointer_echo,1"], A).await;
    assert_eq!(
        rec.echoes_to(A),
        ["pointer,primary,130.0,230.0,1,0"],
        "the pointer reader answers from the server"
    );
    h.release_conn(1);

    // A layout with a second group: the group is locked around the key and put back.
    let set = std::process::Command::new("setxkbmap")
        .args(["us,ru"])
        .status()
        .unwrap();
    assert!(set.success());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cyrillic_ef = 0x06C6u32;
    send(&h, &[&format!("kd,{cyrillic_ef}")], A).await;
    let (locked, carried, placed) = h
        .x()
        .unwrap()
        .call(move |s| {
            s.link().sync().unwrap();
            let placed = s.keyboard.resolve(cyrillic_ef).unwrap();
            (
                s.link().xkb_locked_group().unwrap(),
                s.keyboard.layout_carries(cyrillic_ef),
                placed,
            )
        })
        .await
        .unwrap();
    assert!(
        carried,
        "the new layout's keysyms are placed after its notify"
    );
    assert_eq!(placed.2, Some(1), "the keysym sits in the second group");
    assert_eq!(locked, 1, "its group is locked around the key");
    assert_eq!(
        placed.0, a,
        "on the same physical key as the first group's 'a'"
    );
    assert!(key_down(a));
    send(&h, &[&format!("ku,{cyrillic_ef}")], A).await;
    assert!(!key_down(a));
    tokio::time::sleep(super::x11_keyboard::GROUP_LINGER + Duration::from_millis(200)).await;
    let locked = h
        .x()
        .unwrap()
        .call(|s| s.link().xkb_locked_group().unwrap())
        .await
        .unwrap();
    assert_eq!(locked, 0, "the lock goes back once its linger ends");
    let set = std::process::Command::new("setxkbmap")
        .args(["us"])
        .status()
        .unwrap();
    assert!(set.success());

    assert!(
        h.current_cursor().await.is_some(),
        "the current cursor is fetched through XFixes"
    );
    assert!(
        !h.display_reads_kernel_input().await,
        "Xvfb lists no device nodes"
    );

    // A broken connection is noticed and replaced.
    let old = h.x().unwrap();
    old.abandon();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(old.broken(), "the shut socket reads as broken");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        h.x().is_some_and(|x| !x.broken()),
        "and the health tick reconnects"
    );
    send(&h, &["m,5,6,0,0"], A).await;
    assert_eq!(pointer(), (5, 6), "input flows on the new connection");
    h.disconnect().await;
}

// --- The session the applications run in ---------------------------------------------------------

/// Lay `names` out as live sockets in a fresh directory beside their lock files.
fn live_sockets(names: &[&str]) -> (Scratch, Vec<std::os::unix::net::UnixListener>) {
    let runtime = Scratch::new("appsock");
    let listeners = names
        .iter()
        .map(|name| {
            std::fs::write(runtime.path(&format!("{name}.lock")), b"").expect("lock file");
            std::os::unix::net::UnixListener::bind(runtime.path(name)).expect("bind")
        })
        .collect();
    (runtime, listeners)
}

#[test]
fn which_socket_input_and_clipboard_are_aimed_at() {
    let detect = |names: &[&str]| {
        let (runtime, _listeners) = live_sockets(names);
        detect_app_compositor_in(&runtime.text(), "wayland-1")
    };
    assert_eq!(
        detect(&["wayland-1"]),
        None,
        "a plain session keeps input on the capture compositor"
    );
    assert_eq!(
        detect(&["wayland-1", "wayland-0"]).as_deref(),
        Some("wayland-0"),
        "a nested compositor is adopted"
    );
    assert_eq!(
        detect(&["wayland-1", "wayland-0", "wayland-wm"]).as_deref(),
        Some("wayland-0"),
        "a relay socket beside a nested compositor does not hide it"
    );
    assert_eq!(
        detect(&["wayland-1", "wayland-0", "wayland-2"]),
        None,
        "two nested compositors stay ambiguous"
    );

    let (runtime, listeners) = live_sockets(&["wayland-1", "wayland-0"]);
    drop(listeners);
    assert!(runtime.path("wayland-0").exists());
    assert_eq!(
        detect_app_compositor_in(&runtime.text(), "wayland-1"),
        None,
        "a stale socket file is not adopted: a compositor that died must not take input with it"
    );
}

#[tokio::test]
async fn the_configured_socket_wins_and_the_session_hears() {
    let seat = Arc::new(FakeSeat::new(&keymap_text("us")));
    let cfg = InputConfig {
        is_wayland: true,
        app_wayland_display: "wayland-2".into(),
        wayland_socket_index: 1,
        ..Default::default()
    };
    let (h, rec) = dispatcher(cfg, Some(seat as Arc<dyn WaylandSeat>));
    assert_eq!(h.app_wayland_display(), "wayland-2");
    assert!(h.has_separate_app_compositor());
    assert!(
        rec.calls().contains(&"adopted wayland-2".to_string()),
        "the transport hears: {:?}",
        rec.calls()
    );
}

#[test]
fn a_synced_dpi_scales_the_cursor_cap() {
    let cfg = InputConfig {
        max_cursor_size: 32,
        ..Default::default()
    };
    let (h, _) = dispatcher(cfg, None);
    assert_eq!(h.cursor_metrics(), (96.0, 32));
    h.set_system_dpi(192.0);
    assert_eq!(h.cursor_metrics(), (192.0, 64));
    assert_eq!(AppCompositor::system_dpi(&*h), 192.0);
    h.set_system_dpi(96.0);
    assert_eq!(h.cursor_metrics(), (96.0, 32), "and comes back");
}

#[test]
fn display_names_resolve_to_sockets() {
    use crate::clipboard::x11::display_live;
    use crate::input::x11_link::x_display_socket;
    for (name, want) in [
        (":0", "/tmp/.X11-unix/X0"),
        (":12.1", "/tmp/.X11-unix/X12"),
        ("unix:3", "/tmp/.X11-unix/X3"),
        ("unix:3.0", "/tmp/.X11-unix/X3"),
        (" :4 ", "/tmp/.X11-unix/X4"),
    ] {
        assert_eq!(x_display_socket(name).as_deref(), Some(want), "{name:?}");
    }
    for name in [
        "host:0",
        "192.168.0.2:0",
        "localhost:10.0",
        "tcp/host:0",
        "::1:0",
        "",
        ":",
        ":abc",
        ":0.",
    ] {
        assert_eq!(x_display_socket(name), None, "{name:?}");
    }
    assert!(
        !display_live("host:0"),
        "a remote display is reported dead rather than connected to"
    );
}

#[test]
fn where_the_applications_run() {
    let (h, _) = dispatcher(InputConfig::default(), None);
    assert_eq!(
        h.app_session_on(Some(":7".into())),
        AppSession {
            x11_display: Some(":7".into()),
            wayland_display: None,
            kind: "x11"
        },
        "x11 backend: apps on the server's DISPLAY"
    );

    let (h, _) = wayland(Arc::new(FakeSeat::new(&keymap_text("us"))), false);
    assert_eq!(
        h.app_session_on(None),
        AppSession {
            x11_display: None,
            wayland_display: Some("wayland-capture".into()),
            kind: "wayland"
        },
        "plain Wayland session: capture compositor socket"
    );

    let (h, _) = wayland(Arc::new(FakeSeat::new(&keymap_text("us"))), true);
    let session = h.app_session_on(Some(":selkies-none".into()));
    assert_eq!(session.wayland_display.as_deref(), Some("wayland-nested"));
    assert_eq!(session.kind, "wayland");
    assert_eq!(
        session.x11_display,
        live_x_displays().first().cloned(),
        "nested compositor: its socket and its Xwayland"
    );
    assert!(
        !crate::clipboard::x11::display_is_xwayland(""),
        "empty display name is rejected"
    );
}

/// A Wayland handler reading its own DISPLAY, in a process of its own.
#[tokio::test]
#[ignore = "run by a_rootful_xwayland_is_adopted_only_when_it_really_is_one, in a process of its own"]
async fn a_rootful_xwayland_is_read_in_its_process() {
    if !isolated() {
        return;
    }
    crate::settings::install(crate::settings::Settings::parse(&[], &|_| None));
    let display = std::env::var("DISPLAY").expect("a server");
    let want_xwayland = std::env::var("EXPECT_XWAYLAND").is_ok_and(|v| v == "1");
    let (h, rec) = wayland(Arc::new(FakeSeat::new(&keymap_text("us"))), false);
    let screens = SessionScreens::new(None, true, None);
    screens.attach(&h);
    assert!(crate::clipboard::x11::display_live(&display));
    assert_eq!(
        crate::clipboard::x11::display_is_xwayland(&display),
        want_xwayland
    );
    let session = h.app_session();
    let (offered, why) = screens.capability();
    if want_xwayland {
        assert_eq!(Session::x11_session_display(&*h), Some(display.clone()));
        assert_eq!(
            session,
            AppSession {
                x11_display: Some(display.clone()),
                wayland_display: None,
                kind: "x11"
            },
            "app_session adopts the rootful Xwayland as an X session"
        );
        assert!(
            !offered && why.contains("rootful Xwayland"),
            "and offers it no second display: {why}"
        );
        Session::x11_session_adopted(&*h);
        assert!(
            rec.calls().contains(&"screens".to_string()),
            "its monitor coming up tells the transport"
        );
    } else {
        assert_eq!(Session::x11_session_display(&*h), None);
        assert_eq!(
            session,
            AppSession {
                x11_display: None,
                wayland_display: Some("wayland-capture".into()),
                kind: "wayland"
            },
            "app_session ignores a non-Xwayland server, apps stay Wayland"
        );
        assert!(offered, "Wayland clients are offered a second display");
    }
    let (nested, _) = wayland(Arc::new(FakeSeat::new(&keymap_text("us"))), true);
    assert_eq!(
        Session::x11_session_display(&*nested),
        None,
        "nested session: its XWM bridges its Xwayland, so no unbridged X monitor"
    );
}

#[test]
#[ignore = "needs labwc, Xwayland, and Xvfb"]
fn a_rootful_xwayland_is_adopted_only_when_it_really_is_one() {
    let inner = "input::tests::a_rootful_xwayland_is_read_in_its_process";
    let (_xvfb, xvfb_display) = private_x_server(320, 240);
    run_isolated(
        inner,
        &[("DISPLAY", &xvfb_display), ("EXPECT_XWAYLAND", "0")],
    );

    let labwc = PrivateLabwc::start(1);
    let number = 3000 + std::process::id() % 5000;
    let display = format!(":{number}");
    let _xwayland = Owned::spawn(
        std::process::Command::new("Xwayland")
            .arg(&display)
            .env("XDG_RUNTIME_DIR", &labwc.runtime.0)
            .env("WAYLAND_DISPLAY", "wayland-0"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !crate::clipboard::x11::display_live(&display) {
        assert!(Instant::now() < deadline, "Xwayland never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    run_isolated(inner, &[("DISPLAY", &display), ("EXPECT_XWAYLAND", "1")]);
}
