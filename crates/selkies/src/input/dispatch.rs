/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The wire protocol of the input channel (`on_message`): every verb parsed and acted on, the
//! pointer messages with their numbering and the backend they land on, and the trackpad pages'
//! pointer echo.
//!
//! Motion may travel a channel that keeps no order, so pointer messages are numbered: an
//! absolute position older than the last message applied for its connection is stale, not a
//! move back; a delta still counts, in whatever order it lands. Every message carries the
//! buttons held when it was sent, so a late one's are older than those applied since and are
//! not applied again; only a wheel pulse's scroll bits, a click rather than a state, still land.
//!
//! A trackpad page draws the pointer where the server echoes it (`_pointer_echo`), moved on by
//! its own deltas, so an echo goes out where the pointer is not where the page draws it, at most
//! once per `POINTER_ECHO_MIN`, with a trailing echo for the end of the motion. A resting
//! pointer is read again every `POINTER_ECHO_POLL`, for a move no page sent (an application's
//! warp); X11 asks the server on a worker thread, Wayland reads the compositor's position.

use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};

use super::ConnId;
use super::authority::{Admission, Sender, admit};
use super::handler::{
    Dispatcher, KeyItem, MAX_PRESSED_KEYS, PASTE_CHORDS, run_xdotool, xdotool_type_args,
};
use super::keysyms::{
    KEYSYM_ALT_L, KEYSYM_LEFT_ARROW, KEYSYM_RIGHT_ARROW, is_modifier, is_printable,
    printable_codepoint,
};
use super::pointer::{
    MOUSE_MASK_BIT_ERASER, MOUSE_MASK_BIT_PRIMARY, MOUSE_MASK_SCROLL_BITS, PointerEcho, display_at,
    echo_message, g_rounded, one_decimal,
};
use super::x11_link::XPointerReader;
use super::x11_worker::XState;
use crate::core::Role;
use crate::core::{DisplayId, PRIMARY_DISPLAY};

/// The least time between two pointer echoes.
pub const POINTER_ECHO_MIN: Duration = Duration::from_millis(8);
/// How often a resting pointer is read again while a trackpad page watches it.
pub const POINTER_ECHO_POLL: Duration = Duration::from_millis(25);
/// The largest finger scroll one `sf` message moves, in pixels each way.
pub const FINGER_SCROLL_MAX_PX: f64 = 4096.0;
/// The rate control modes a client may name (`settings.RateControlMode`).
const RATE_CONTROL_MODES: &[&str] = &["cbr", "crf"];

/// X core pointer buttons for XTEST.
const XBUTTON_LEFT: u8 = 1;
const XBUTTON_MIDDLE: u8 = 2;
const XBUTTON_RIGHT: u8 = 3;
/// evdev codes for the published pointer and the uinput helper.
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
/// evdev button codes the Wayland seat takes.
const WL_BTN_LEFT: u32 = 272;
const WL_BTN_RIGHT: u32 = 273;
const WL_BTN_MIDDLE: u32 = 274;

/// One pointer action on the X11 backend, as `send_mouse` routes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MouseAction {
    Warp(i32, i32),
    Move(i32, i32),
    /// Vertical wheel: positive scrolls up (X button 4), negative down (button 5).
    ScrollY(i32),
    /// Horizontal wheel: positive scrolls right (button 7), negative left (button 6).
    ScrollX(i32),
    Button {
        x_button: u8,
        evdev: u16,
        down: bool,
    },
}

/// Where the pointer is: desktop position, the physical pixels a relative pixel moves it, and
/// the compositor's move number (None on X11).
type PointerAt = (f64, f64, f64, Option<u64>);

/// The X server's pointer, read off the runtime; a trait so the suites stand in a server.
pub trait PointerReader: Send + Sync {
    /// Blocking. The root position, or None when the server cannot be asked.
    fn read(&self) -> Option<(i16, i16)>;
    /// Drop the connection.
    fn close(&self);
}

impl PointerReader for XPointerReader {
    fn read(&self) -> Option<(i16, i16)> {
        XPointerReader::read(self)
    }

    fn close(&self) {
        XPointerReader::close(self)
    }
}

/// A coordinate a peer named, held to what the pointer logic computes in.
fn saturate_i32(value: i64) -> i32 {
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

impl Dispatcher {
    /// Admit or refuse a message by its sender, with the process's authority settings.
    pub fn admit(&self, msg: &str, sender: &Sender) -> Admission {
        admit(msg, sender, self.cfg.secure, self.cfg.enable_collab)
    }

    /// Both transports' entry point for one client message: the gates, then the handler. The
    /// sender is asked of the host per message, so a revoked or re-slotted token lands on the
    /// next one. A viewer's `SETTINGS` is admitted for the transport's own per-viewer reading but
    /// never reaches the display's settings, which stay controller-only.
    pub async fn receive(
        self: &Arc<Self>,
        msg: &str,
        display_id: &str,
        conn: Option<ConnId>,
    ) -> Admission {
        let sender = self.host.sender(conn);
        let decision = self.admit(msg, &sender);
        let head: String = msg.chars().take(32).collect();
        match decision {
            Admission::Allowed => {
                if sender.role.unwrap_or(Role::Viewer) == Role::Viewer
                    && msg.starts_with("SETTINGS,")
                {
                    debug!(target: "input", "Ignoring SETTINGS payload from a viewer (display '{display_id}')");
                } else {
                    self.on_message(msg, display_id, conn).await;
                }
            }
            Admission::DroppedSilently => {}
            Admission::UnauthorizedViewer => {
                warn!(target: "input", "Dropping unauthorized viewer input: {head}")
            }
            Admission::UnauthorizedSecure => {
                warn!(target: "input", "Dropping unauthorized secure-mode input: {head}")
            }
            Admission::GamepadSlot => {
                warn!(target: "input", "Dropping gamepad input for a slot this peer does not hold: {head}")
            }
        }
        decision
    }

    /// Transport entry point for one admitted client message. A malformed message never tears
    /// down the connection: parse errors are logged and swallowed here.
    ///
    /// Args:
    ///   msg: Raw comma-delimited message.
    ///   display_id: Transport-level id of the display whose channel delivered the message (not a
    ///     spoofable payload field).
    ///   conn: Transport connection identity, for per-connection state.
    pub async fn on_message(self: &Arc<Self>, msg: &str, display_id: &str, conn: Option<ConnId>) {
        if let Err(e) = self.dispatch_message(msg, display_id, conn).await {
            warn!(target: "input", "Malformed client message {:?}: {e}", msg.chars().take(64).collect::<String>());
        }
    }

    /// Parse and act on one client message (the whole wire protocol lives here).
    async fn dispatch_message(
        self: &Arc<Self>,
        msg: &str,
        display_id: &str,
        conn: Option<ConnId>,
    ) -> Result<(), String> {
        let toks: Vec<&str> = msg.split(',').collect();
        let display: DisplayId = if display_id.is_empty() {
            PRIMARY_DISPLAY.to_string()
        } else {
            display_id.to_string()
        };
        let int = |i: usize| -> Result<i64, String> {
            toks.get(i)
                .ok_or_else(|| format!("field {i} missing"))?
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("field {i}: {e}"))
        };
        match toks[0] {
            "pong" => {
                let start = self.state.lock().ping_start;
                match start {
                    // A straggler from the other mode's client after a transport flip.
                    None => debug!(target: "input", "received pong before ping; ignoring"),
                    Some(start) => {
                        let ms = start.elapsed().as_secs_f64() / 2.0 * 1000.0;
                        self.host.ping_response((ms * 1000.0).round() / 1000.0);
                    }
                }
            }
            "kd" => {
                let keysym = u32::try_from(int(1)?).map_err(|_| "keysym out of range")?;
                let chord_held = {
                    let st = self.state.lock();
                    PASTE_CHORDS.iter().any(|(k, mods)| {
                        *k == keysym && mods.iter().any(|m| st.pressed_keys.contains_key(m))
                    })
                };
                if chord_held {
                    self.clipboard().paste_hold(keysym).await;
                }
                let (atomic, inject_now) = {
                    let mut st = self.state.lock();
                    st.key_suspended.remove(&keysym);
                    let now = Instant::now();
                    if !st.pressed_keys.contains_key(&keysym)
                        && st.pressed_keys.len() >= MAX_PRESSED_KEYS
                    {
                        // At the cap the oldest entry goes, not the new one: the key is injected
                        // below regardless, and untracked it would never auto-release.
                        if let Some(oldest) = st
                            .pressed_keys
                            .iter()
                            .min_by_key(|(_, t)| **t)
                            .map(|(k, _)| *k)
                        {
                            st.pressed_keys.remove(&oldest);
                        }
                    }
                    st.pressed_keys.insert(keysym, now);
                    st.reaped_atomic_keys.remove(&keysym);
                    if self.cfg.is_wayland {
                        (false, false)
                    } else {
                        if is_modifier(keysym) {
                            st.active_modifiers.insert(keysym);
                        }
                        let atomic = is_printable(keysym)
                            && st.active_modifiers.is_empty()
                            && char::from_u32(printable_codepoint(keysym))
                                .is_some_and(|c| !c.is_alphabetic() && c != ' ');
                        (atomic, true)
                    }
                };
                if !inject_now {
                    self.enqueue(KeyItem::Kd(keysym));
                    return Ok(());
                }
                if atomic {
                    if let Some(ch) = char::from_u32(printable_codepoint(keysym)) {
                        Box::pin(self.on_message(&format!("co,end,{ch}"), &display, conn)).await;
                        self.state.lock().atomically_typed_keys.insert(keysym);
                    }
                } else {
                    self.send_keypress(keysym, true, None).await;
                }
                let mut st = self.state.lock();
                // Arm auto-repeat: the key moves to the end (only the newest repeats);
                // modifiers never; atomic keys are armed too.
                if st.key_repeat_enabled && !is_modifier(keysym) {
                    let at = Instant::now() + st.key_repeat_delay;
                    st.arm_repeat(keysym, at);
                } else {
                    st.disarm_repeat(keysym);
                }
            }
            "ku" => {
                let keysym = u32::try_from(int(1)?).map_err(|_| "keysym out of range")?;
                let release = {
                    let mut st = self.state.lock();
                    st.pressed_keys.remove(&keysym);
                    st.disarm_repeat(keysym);
                    if st.key_suspended.remove(&keysym).is_some() {
                        // Let go when its heartbeats stopped.
                        None
                    } else if self.cfg.is_wayland {
                        Some(false)
                    } else {
                        if is_modifier(keysym) {
                            st.active_modifiers.remove(&keysym);
                        }
                        if st.reaped_atomic_keys.remove(&keysym) {
                            // Already reaped by the sweep; a keyup now would be spurious.
                            None
                        } else if st.atomically_typed_keys.remove(&keysym) {
                            // Never physically held on X11; nothing to release.
                            None
                        } else {
                            Some(true)
                        }
                    }
                };
                match release {
                    Some(false) => self.enqueue(KeyItem::Ku(keysym)),
                    Some(true) => self.send_keypress(keysym, false, None).await,
                    None => {}
                }
            }
            "kr" => {
                if self.cfg.is_wayland {
                    self.enqueue(KeyItem::Kr(None));
                } else {
                    self.reset_keyboard().await;
                }
            }
            "kh" => {
                // Refresh only, no injection. Atomic keys are refreshed like any other: the
                // repeat loop pauses on a stale heartbeat, so skipping them would kill their
                // auto-repeat before the first repeat is due. Bounded: a client could otherwise
                // pack one frame with tens of thousands of tokens.
                let now = Instant::now();
                let mut st = self.state.lock();
                for tok in toks.iter().skip(1).take(MAX_PRESSED_KEYS) {
                    let Ok(keysym) = tok.trim().parse::<u32>() else {
                        continue;
                    };
                    if let Some(seen) = st.pressed_keys.get_mut(&keysym) {
                        *seen = now;
                        if let Some((beat, live)) = st.key_suspended.get_mut(&keysym) {
                            if beat.is_some_and(|b| {
                                now.duration_since(b) <= super::handler::KEY_REPEAT_HEARTBEAT_GRACE
                            }) && live.is_none()
                            {
                                *live = Some(now);
                            }
                            *beat = Some(now);
                        }
                    }
                }
            }
            "m" | "m2" => {
                let relative = toks[0] == "m2";
                // Dropped rather than defaulted: a default would warp to the origin.
                let (Ok(x), Ok(y), Ok(mask), Ok(magnitude)) = (int(1), int(2), int(3), int(4))
                else {
                    return Ok(());
                };
                let (x, y) = (saturate_i32(x), saturate_i32(y));
                let mut button_mask = u32::try_from(mask).unwrap_or(0);
                let mut seq = None;
                if toks.len() > 5 {
                    let Ok(number) = toks[5].trim().parse::<u64>() else {
                        return Ok(());
                    };
                    seq = Some(number);
                    let mut st = self.state.lock();
                    let last = st
                        .pointer_seq
                        .get(&conn.unwrap_or_default())
                        .copied()
                        .unwrap_or(0);
                    if number <= last && !relative {
                        return Ok(());
                    }
                    if number > last {
                        st.pointer_seq.insert(conn.unwrap_or_default(), number);
                    } else {
                        let pulse = if magnitude > 0 {
                            MOUSE_MASK_SCROLL_BITS
                        } else {
                            0
                        };
                        button_mask = (st.pointer.button_mask & !pulse) | (button_mask & pulse);
                    }
                }
                self.send_mouse(x, y, button_mask, magnitude, relative, &display)
                    .await;
                if !self.state.lock().pointer_echoes.is_empty() {
                    let (dx, dy) = if relative { (x, y) } else { (0, 0) };
                    self.pointer_moved(conn, seq, dx, dy);
                }
            }
            "sf" | "sfe" => {
                // A touchpad's scroll in stream pixels and the finger's lift, sent only where
                // `finger_scroll_available`.
                if !self.finger_scroll_available() {
                    return Ok(());
                }
                if toks[0] == "sfe" {
                    self.wl_finger_scroll(None);
                    return Ok(());
                }
                let (Some(dx), Some(dy)) = (
                    toks.get(1).and_then(|t| t.trim().parse::<f64>().ok()),
                    toks.get(2).and_then(|t| t.trim().parse::<f64>().ok()),
                ) else {
                    return Ok(());
                };
                if !(dx.is_finite() && dy.is_finite()) {
                    return Ok(());
                }
                self.wl_finger_scroll(Some((
                    dx.clamp(-FINGER_SCROLL_MAX_PX, FINGER_SCROLL_MAX_PX),
                    dy.clamp(-FINGER_SCROLL_MAX_PX, FINGER_SCROLL_MAX_PX),
                )));
            }
            "_pointer_echo" => {
                let on = toks.get(1).is_some_and(|t| t.trim() == "1");
                self.set_pointer_echo(conn, on).await;
            }
            "p" => self.host.pointer_visible(int(1)? != 0).await,
            "vp" => {
                let mut values = [0.0f64; 4];
                for (i, v) in values.iter_mut().enumerate() {
                    let Some(Ok(parsed)) = toks.get(i + 1).map(|t| t.trim().parse::<f64>()) else {
                        return Ok(());
                    };
                    *v = parsed;
                }
                self.host
                    .set_client_stream_box(&display, values[0], values[1], values[2], values[3])
                    .await;
            }
            "vb" => {
                let kbps = int(1)?;
                if kbps > 0 {
                    self.host.video_bitrate(kbps, &display).await;
                }
            }
            "ab" => {
                let kbps = int(1)?;
                if kbps > 0 {
                    self.host.audio_bitrate(kbps).await;
                }
            }
            // Enforced at the slots too, whatever the client's own UI state: the index is the
            // client's own claim, and nothing is driven while gamepads are off.
            "js" => {
                self.gamepads.handle(conn.unwrap_or_default(), msg);
            }
            "cws" | "cbs" | "cwd" | "cbd" | "cwe" | "cbe" | "cw" | "cb" | "cr"
            | "REQUEST_CLIPBOARD" | "_ebc" => {
                self.clipboard().handle(conn.unwrap_or_default(), msg).await;
            }
            "r" => {
                let res = toks.get(1).copied().unwrap_or("");
                let valid = res.split_once('x').is_some_and(|(w, h)| {
                    !w.is_empty()
                        && !h.is_empty()
                        && w.chars().all(|c| c.is_ascii_digit())
                        && h.chars().all(|c| c.is_ascii_digit())
                });
                if valid {
                    // Passed through verbatim: even-dimension normalization is the display's.
                    self.host.resize(res, &display).await;
                } else {
                    warn!(target: "input", "Rejecting resolution change, invalid: {res}");
                }
            }
            "s" => {
                let scale = toks.get(1).copied().unwrap_or("");
                let valid = scale
                    .split_once('.')
                    .map(|(a, b)| {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.chars().all(|c| c.is_ascii_digit())
                            && b.chars().all(|c| c.is_ascii_digit())
                    })
                    .unwrap_or(!scale.is_empty() && scale.chars().all(|c| c.is_ascii_digit()));
                if !valid {
                    warn!(target: "input", "Rejecting scaling change, invalid: {scale}");
                } else if display != PRIMARY_DISPLAY && !self.cfg.is_wayland {
                    // X11 has one DPI, the primary's; Wayland scales each screen.
                    info!(target: "input", "Ignoring DPI {scale} from '{display}': the desktop DPI follows the primary display.");
                } else if let Ok(value) = scale.parse::<f64>() {
                    self.host.scaling_ratio(value, &display).await;
                }
            }
            "cmd" => {
                if !self.cfg.command_enabled {
                    warn!(target: "input", "Received 'cmd' message, but command execution is disabled by server settings.");
                    return Ok(());
                }
                if toks.len() > 1 {
                    let command = toks[1..].join(",");
                    info!(target: "input", "Attempting to execute command: '{command}'");
                    self.launch_command(command, conn).await;
                } else {
                    warn!(target: "input", "Received 'cmd' message without a command string.");
                }
            }
            "_arg_fps" => {
                let fps: f64 = toks
                    .get(1)
                    .ok_or("fps missing")?
                    .trim()
                    .parse()
                    .map_err(|e| format!("fps: {e}"))?;
                if fps > 0.0 && fps.is_finite() {
                    self.host.set_fps(fps, &display).await;
                }
            }
            "_f" => self.host.client_fps(int(1)?),
            "_l" => self.host.client_latency(int(1)?),
            "_stats_video" | "_stats_audio" => {
                self.host
                    .client_webrtc_stats(toks[0], &toks[1..].join(","))
                    .await
            }
            "co" if toks.get(1) == Some(&"end") => {
                let text = msg.get(7..).unwrap_or("");
                if self.cfg.is_wayland {
                    self.enqueue(KeyItem::CoEnd(text.to_string()));
                } else {
                    let neutralize = !self.state.lock().action_held();
                    if !self.type_text_xtest(text, neutralize).await {
                        let args = xdotool_type_args(text);
                        let args: Vec<&str> = args.iter().map(String::as_str).collect();
                        run_xdotool(&args, Duration::from_millis(500)).await;
                    }
                }
            }
            "_rc" => {
                let mode = toks.get(1).ok_or("mode missing")?.trim().to_lowercase();
                if RATE_CONTROL_MODES.contains(&mode.as_str()) {
                    self.host.rate_control_mode(&mode, &display).await;
                } else {
                    error!(target: "input", "Error updating rate control mode: {mode:?} is not a mode");
                }
            }
            "_crf" => {
                let crf = int(1)?;
                if !(0..=51).contains(&crf) {
                    warn!(target: "input", "CRF value out of range (0-51): {crf}");
                    return Ok(());
                }
                self.host.crf(crf, &display).await;
            }
            "SETTINGS" => {
                let payload = if toks.len() > 1 { &msg[9..] } else { "" };
                debug!(target: "input", "Received SETTINGS message: {payload}");
                match serde_json::from_str::<serde_json::Value>(payload) {
                    // Applied to the delivering channel's display (not a spoofable payload
                    // field) and inline, so a resize behind it sees this policy.
                    Ok(json) => self.host.update_settings(json, &display).await,
                    Err(e) => error!(target: "input", "Failed to parse SETTINGS data: {e}"),
                }
            }
            "SET_NATIVE_CURSOR_RENDERING" => {
                // WS-protocol alias of `p,N`; both map to the capture_cursor tunable.
                let on = toks
                    .get(1)
                    .map(|t| t.trim().to_lowercase())
                    .is_some_and(|t| t == "1" || t == "true");
                self.host.pointer_visible(on).await;
            }
            "REQUEST_KEYFRAME" => self.host.request_keyframe(&display).await,
            _ => {
                warn!(target: "input", "Unknown data channel message: {}", msg.chars().take(100).collect::<String>())
            }
        }
        Ok(())
    }

    /// Disassociate every gamepad slot this transport connection associated, and forget its
    /// pointer message count and echo: the ungraceful path, since a tab that dies mid-press
    /// never sends `js,d`, and only the transport knows the connection is gone.
    pub fn release_conn(&self, conn: ConnId) {
        self.state.lock().pointer_seq.remove(&conn);
        self.drop_pointer_echo(conn);
        self.gamepads.release_conn(conn);
    }

    /// Disassociate the one-based `slots` this connection's pads drive: a token table that
    /// takes a slot from a connection leaves its page no way to.
    pub fn release_gamepad_slots_for_conn(&self, conn: ConnId, slots: &[u8]) {
        self.gamepads.release_slots(conn, slots);
    }

    /// Apply one client pointer message on whichever backend this session uses
    /// (`send_x11_mouse`): move the pointer (absolute coordinates offset into the named
    /// display's region of the combined layout), then diff `button_mask` against the held state
    /// bit by bit, emitting press and release, scroll clicks, or the Alt+Arrow back and forward
    /// chords. The eraser bit (button 5) folds into the primary button: neither the X core
    /// pointer nor `wl_pointer` has an eraser button, so the eraser clicks and drags like the pen
    /// tip, and OR-ing it keeps press and release balanced when the tip bit is set at the same
    /// time. `scroll_magnitude` is the wheel repeat count; 0 marks bits 3 and 4 as back and
    /// forward buttons instead of wheel ticks.
    pub(crate) async fn send_mouse(
        self: &Arc<Self>,
        x: i32,
        y: i32,
        button_mask: u32,
        scroll_magnitude: i64,
        relative: bool,
        display_id: &str,
    ) {
        // Client-controlled; unbounded, the X11 scroll loop would stall the X thread.
        let scroll_magnitude = scroll_magnitude.clamp(0, 64) as i32;
        let button_mask = if button_mask & MOUSE_MASK_BIT_ERASER != 0 {
            (button_mask | MOUSE_MASK_BIT_PRIMARY) & !MOUSE_MASK_BIT_ERASER
        } else {
            button_mask
        };
        let layouts = self.host.display_layouts();
        let (mv, previous_mask) = {
            let mut st = self.state.lock();
            let Some(mv) = st.pointer.resolve(x, y, relative, display_id, &layouts) else {
                return;
            };
            (mv, st.pointer.button_mask)
        };
        let changed_bits = (0..8u32)
            .map(|bit| 1u32 << bit)
            .filter(|bit| (previous_mask & bit) != (button_mask & bit));

        if let Some(seat) = self.seat.clone() {
            if !mv.static_relative {
                let motion = if mv.relative {
                    seat.inject_relative_mouse_move(mv.x as f64, mv.y as f64)
                } else {
                    seat.inject_mouse_move(mv.x as f64, mv.y as f64)
                };
                match motion {
                    Ok(Some(n)) => self.state.lock().wl_motion = Some(n),
                    Ok(None) => {}
                    Err(e) => debug!(target: "input", "pointer injection failed: {e}"),
                }
            }
            let mag = scroll_magnitude.max(1) as f64;
            for bit in changed_bits {
                let pressed = button_mask & bit != 0;
                let state = pressed as u32;
                let result = match bit {
                    1 => seat.inject_mouse_button(WL_BTN_LEFT, state),
                    2 => seat.inject_mouse_button(WL_BTN_MIDDLE, state),
                    4 => seat.inject_mouse_button(WL_BTN_RIGHT, state),
                    8 | 16 if !pressed => Ok(()),
                    8 if scroll_magnitude > 0 => {
                        self.wl_scroll(0.0, 10.0 * mag);
                        Ok(())
                    }
                    16 if scroll_magnitude > 0 => {
                        self.wl_scroll(0.0, -10.0 * mag);
                        Ok(())
                    }
                    // Queued behind pending keys like any key event: direct injection could
                    // land between a kd and its ku.
                    8 => {
                        self.enqueue_chord(vec![
                            (KEYSYM_ALT_L, true),
                            (KEYSYM_LEFT_ARROW, true),
                            (KEYSYM_LEFT_ARROW, false),
                            (KEYSYM_ALT_L, false),
                        ]);
                        Ok(())
                    }
                    16 => {
                        self.enqueue_chord(vec![
                            (KEYSYM_ALT_L, true),
                            (KEYSYM_RIGHT_ARROW, true),
                            (KEYSYM_RIGHT_ARROW, false),
                            (KEYSYM_ALT_L, false),
                        ]);
                        Ok(())
                    }
                    64 if scroll_magnitude > 0 && pressed => {
                        self.wl_scroll(-10.0 * mag, 0.0);
                        Ok(())
                    }
                    128 if scroll_magnitude > 0 && pressed => {
                        self.wl_scroll(10.0 * mag, 0.0);
                        Ok(())
                    }
                    _ => Ok(()),
                };
                if let Err(e) = result {
                    debug!(target: "input", "button injection failed: {e}");
                }
            }
            self.state.lock().pointer.button_mask = button_mask;
            return;
        }

        let mut actions: Vec<MouseAction> = Vec::new();
        if mv.relative {
            if !mv.static_relative {
                actions.push(MouseAction::Move(mv.x, mv.y));
            }
        } else if mv.position_changed || button_mask != previous_mask {
            // Button transitions warp unconditionally: an application may have moved the
            // pointer, and a press must land where the client aims.
            actions.push(MouseAction::Warp(mv.x, mv.y));
        }
        let mut chords: Vec<[(u32, bool); 4]> = Vec::new();
        for bit in changed_bits {
            let pressed = button_mask & bit != 0;
            match bit {
                1 => actions.push(MouseAction::Button {
                    x_button: XBUTTON_LEFT,
                    evdev: BTN_LEFT,
                    down: pressed,
                }),
                2 => actions.push(MouseAction::Button {
                    x_button: XBUTTON_MIDDLE,
                    evdev: BTN_MIDDLE,
                    down: pressed,
                }),
                4 => actions.push(MouseAction::Button {
                    x_button: XBUTTON_RIGHT,
                    evdev: BTN_RIGHT,
                    down: pressed,
                }),
                8 | 16 if !pressed => {}
                // MOUSE_SCROLL_UP is named for the client button, not the physical direction:
                // bit 3 is wheel-down (X button 5).
                8 if scroll_magnitude > 0 => actions.extend(std::iter::repeat_n(
                    MouseAction::ScrollY(-1),
                    scroll_magnitude.max(1) as usize,
                )),
                16 if scroll_magnitude > 0 => actions.extend(std::iter::repeat_n(
                    MouseAction::ScrollY(1),
                    scroll_magnitude.max(1) as usize,
                )),
                8 => chords.push([
                    (KEYSYM_ALT_L, true),
                    (KEYSYM_LEFT_ARROW, true),
                    (KEYSYM_LEFT_ARROW, false),
                    (KEYSYM_ALT_L, false),
                ]),
                16 => chords.push([
                    (KEYSYM_ALT_L, true),
                    (KEYSYM_RIGHT_ARROW, true),
                    (KEYSYM_RIGHT_ARROW, false),
                    (KEYSYM_ALT_L, false),
                ]),
                64 if scroll_magnitude > 0 && pressed => actions.extend(std::iter::repeat_n(
                    MouseAction::ScrollX(-1),
                    scroll_magnitude.max(1) as usize,
                )),
                128 if scroll_magnitude > 0 && pressed => actions.extend(std::iter::repeat_n(
                    MouseAction::ScrollX(1),
                    scroll_magnitude.max(1) as usize,
                )),
                _ => {}
            }
        }
        self.state.lock().pointer.button_mask = button_mask;
        self.run_mouse_actions(&actions, !mv.relative);
        for chord in chords {
            if self.x().is_some() {
                debug!(target: "input", "Sending Alt+Arrow for the mouse back/forward button");
                for (keysym, down) in chord {
                    self.send_keypress(keysym, down, None).await;
                }
            } else {
                warn!(target: "input", "Keyboard not available for Alt+Arrow.");
            }
        }
    }

    /// Route the X11 pointer actions to the published device, the uinput helper socket, or
    /// XTEST on the X thread; one job and, for an absolute message, one flush.
    fn run_mouse_actions(&self, actions: &[MouseAction], flush_after: bool) {
        for action in actions {
            if let Some(devices) = &self.devices {
                match *action {
                    MouseAction::Move(x, y) => {
                        if x != 0 {
                            devices.pointer(EV_REL, REL_X, x);
                        }
                        if y != 0 {
                            devices.pointer(EV_REL, REL_Y, y);
                        }
                    }
                    MouseAction::ScrollY(n) => devices.pointer(EV_REL, REL_WHEEL, n),
                    MouseAction::ScrollX(n) => devices.pointer(EV_REL, REL_HWHEEL, n),
                    MouseAction::Button { evdev, down, .. } => {
                        devices.pointer(EV_KEY, evdev, down as i32)
                    }
                    // Absolute positions are left out: the device declares relative axes.
                    MouseAction::Warp(..) => {}
                }
            }
        }
        // A warp stays on XTEST: the helper's device takes relative axes alone.
        let uinput = !self.cfg.uinput_mouse_socket.is_empty();
        let mut through_x = Vec::with_capacity(actions.len());
        for action in actions {
            if uinput && !matches!(action, MouseAction::Warp(..)) {
                self.uinput_mouse_emit(*action);
            } else {
                through_x.push(*action);
            }
        }
        let Some(x) = self.x() else { return };
        if through_x.is_empty() && !flush_after {
            return;
        }
        let actions = through_x;
        x.send(move |s: &mut XState| {
            let link = s.link();
            for action in &actions {
                match *action {
                    MouseAction::Warp(px, py) => link.fake_motion(
                        px.clamp(-32768, 32767) as i16,
                        py.clamp(-32768, 32767) as i16,
                        false,
                    ),
                    MouseAction::Move(dx, dy) => {
                        link.fake_motion(dx as i16, dy as i16, true);
                        // flush, not sync: XTEST needs no reply and a round trip per move would cost one.
                        link.flush();
                    }
                    MouseAction::ScrollY(n) => {
                        let button = if n > 0 { 4 } else { 5 };
                        for _ in 0..n.abs() {
                            link.fake_button(button, true);
                            link.fake_button(button, false);
                        }
                        link.flush();
                    }
                    MouseAction::ScrollX(n) => {
                        let button = if n > 0 { 7 } else { 6 };
                        for _ in 0..n.abs() {
                            link.fake_button(button, true);
                            link.fake_button(button, false);
                        }
                        link.flush();
                    }
                    MouseAction::Button { x_button, down, .. } => {
                        link.fake_button(x_button, down);
                        link.flush();
                    }
                }
            }
            if flush_after {
                link.flush();
            }
        });
    }

    /// Forward one pointer action to the uinput helper socket as the msgpack
    /// `{"args": [[type, code], value], "kwargs": {...}}` it reads.
    fn uinput_mouse_emit(&self, action: MouseAction) {
        #[derive(serde::Serialize)]
        struct Cmd {
            args: ((u16, u16), i32),
            kwargs: std::collections::HashMap<&'static str, bool>,
        }
        let mut events: Vec<((u16, u16), i32, bool)> = Vec::new();
        match action {
            MouseAction::Warp(..) => return,
            MouseAction::Move(x, y) => {
                events.push(((EV_REL, REL_X), x, false));
                events.push(((EV_REL, REL_Y), y, true));
            }
            MouseAction::ScrollY(n) => events.push(((EV_REL, REL_WHEEL), n, true)),
            MouseAction::ScrollX(n) => events.push(((EV_REL, REL_HWHEEL), n, true)),
            MouseAction::Button { evdev, down, .. } => {
                events.push(((EV_KEY, evdev), down as i32, true))
            }
        }
        let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() else {
            return;
        };
        for (code, value, syn) in events {
            let mut kwargs = std::collections::HashMap::new();
            if !syn {
                kwargs.insert("syn", false);
            }
            if let Ok(bytes) = rmp_serde::to_vec_named(&Cmd {
                args: (code, value),
                kwargs,
            }) && let Err(e) = socket.send_to(&bytes, &self.cfg.uinput_mouse_socket)
            {
                debug!(target: "input", "uinput mouse socket send failed: {e}");
            }
        }
    }

    /// Start or stop echoing the pointer to a trackpad page, which draws it from the echo:
    /// `pointer,<display>,<x>,<y>,<scale>,<seq>`, the position in that display's server pixels,
    /// the physical pixels a relative pixel moves it there, and the last of the page's pointer
    /// messages it includes. A session that cannot say where its pointer is (host capture, no
    /// `pointer_location`) answers `pointer,none`, and the page has the cursor composited instead.
    async fn set_pointer_echo(self: &Arc<Self>, conn: Option<ConnId>, on: bool) {
        let Some(conn) = conn else { return };
        self.drop_pointer_echo(conn);
        if !on {
            return;
        }
        let Some(at) = self.pointer_at_rest().await else {
            self.pointer_sink.pointer_echo(conn, "pointer,none");
            return;
        };
        let start_poll = {
            let mut st = self.state.lock();
            let seq = st.pointer_seq.get(&conn).copied().unwrap_or(0);
            st.pointer_echoes.insert(conn, PointerEcho::new(seq));
            let start = !st.echo_poll_running;
            st.echo_poll_running = true;
            start
        };
        self.echo_pointer_to(conn, at);
        if start_poll {
            let me = self.clone();
            tokio::spawn(async move { me.poll_pointer_echoes().await });
        }
    }

    pub(crate) fn drop_pointer_echo(&self, conn: ConnId) {
        let mut st = self.state.lock();
        st.pointer_echoes.remove(&conn);
        if st.pointer_echoes.is_empty() {
            st.echo_trailing_scheduled = false;
        }
    }

    /// Echo a pointer message's move to every trackpad page whose drawn pointer it leaves
    /// behind: at once unless an echo went out under `POINTER_ECHO_MIN` ago, else when that
    /// interval ends.
    fn pointer_moved(self: &Arc<Self>, conn: Option<ConnId>, seq: Option<u64>, dx: i32, dy: i32) {
        let now = Instant::now();
        let due = {
            let mut st = self.state.lock();
            let motion = if self.cfg.is_wayland {
                st.wl_motion
            } else {
                None
            };
            if let (Some(conn), Some(seq)) = (conn, seq)
                && let Some(echo) = st.pointer_echoes.get_mut(&conn)
            {
                echo.applied(motion, seq, dx as f64, dy as f64);
            }
            st.pointer_moved_at = Some(now);
            st.pointer_echo_at.map(|at| at + POINTER_ECHO_MIN)
        };
        match due {
            Some(due) if now < due => {
                let schedule = {
                    let mut st = self.state.lock();
                    let schedule = !st.echo_trailing_scheduled;
                    st.echo_trailing_scheduled = true;
                    schedule
                };
                if schedule {
                    let me = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(due.into()).await;
                        me.state.lock().echo_trailing_scheduled = false;
                        let at = me.pointer_at();
                        me.echo_pointer(at);
                    });
                }
            }
            _ => {
                let at = self.pointer_at();
                self.echo_pointer(at);
            }
        }
    }

    /// Read a resting pointer again while a trackpad page watches it, so a move no page sent
    /// reaches them too.
    async fn poll_pointer_echoes(self: &Arc<Self>) {
        loop {
            tokio::time::sleep(POINTER_ECHO_POLL).await;
            let (watched, resting) = {
                let st = self.state.lock();
                (
                    !st.pointer_echoes.is_empty(),
                    st.pointer_moved_at
                        .is_none_or(|t| t.elapsed() >= POINTER_ECHO_POLL),
                )
            };
            if !watched {
                break;
            }
            if resting {
                let at = self.pointer_at_rest().await;
                self.echo_pointer(at);
            }
        }
        let reader = {
            let mut st = self.state.lock();
            st.echo_poll_running = false;
            st.pointer_reader.take()
        };
        if let Some(reader) = reader {
            let _ = tokio::task::spawn_blocking(move || reader.close()).await;
        }
    }

    fn echo_pointer(&self, at: Option<PointerAt>) {
        self.state.lock().pointer_echo_at = Some(Instant::now());
        let Some(at) = at else { return };
        let conns: Vec<ConnId> = self.state.lock().pointer_echoes.keys().copied().collect();
        for conn in conns {
            self.echo_pointer_to(conn, at);
        }
    }

    fn echo_pointer_to(&self, conn: ConnId, at: PointerAt) {
        let (x, y, scale, motion) = at;
        let layouts = self.host.display_layouts();
        let point = display_at(&layouts, x, y);
        let mut st = self.state.lock();
        let Some(echo) = st.pointer_echoes.get_mut(&conn) else {
            return;
        };
        let seq = echo.through(motion);
        // As the page reads them back.
        let (lx, ly, scale) = (one_decimal(point.x), one_decimal(point.y), g_rounded(scale));
        if echo.drawn_at(&point.display, lx, ly) {
            return;
        }
        echo.echoed(&point.display, lx, ly, scale, point.w, point.h);
        let message = echo_message(&point.display, lx, ly, scale, seq);
        drop(st);
        self.pointer_sink.pointer_echo(conn, &message);
    }

    /// Where the pointer is on the desktop while it moves, waiting on nothing: Wayland's
    /// compositor position, X11's tracked one.
    fn pointer_at(&self) -> Option<PointerAt> {
        if self.cfg.is_wayland {
            let (x, y, scale, motion) = self.seat.as_ref()?.pointer_location()?;
            return Some((x, y, scale, Some(motion)));
        }
        let st = self.state.lock();
        Some((
            st.pointer.last_x as f64,
            st.pointer.last_y as f64,
            1.0,
            None,
        ))
    }

    /// `pointer_at` for a pointer at rest, which on X11 asks the server on a worker thread, so a
    /// warp of an application's own shows; the answer comes after every move sent before it, and
    /// the tracking goes on from it unless the pointer moved while it was asked. None where the
    /// session cannot say where its pointer is.
    async fn pointer_at_rest(&self) -> Option<PointerAt> {
        if self.cfg.is_wayland {
            return self.pointer_at();
        }
        self.x()?;
        let (reader, asked) = {
            let mut st = self.state.lock();
            let reader = st
                .pointer_reader
                .get_or_insert_with(|| {
                    Arc::new(XPointerReader::new(self.cfg.x_display.clone()))
                        as Arc<dyn PointerReader>
                })
                .clone();
            (reader, st.pointer_moved_at)
        };
        let xy = tokio::task::spawn_blocking(move || reader.read())
            .await
            .ok()
            .flatten()?;
        let mut st = self.state.lock();
        if st.pointer_moved_at != asked {
            drop(st);
            return self.pointer_at();
        }
        st.pointer.last_x = xy.0 as i32;
        st.pointer.last_y = xy.1 as i32;
        Some((xy.0 as f64, xy.1 as f64, 1.0, None))
    }
}
