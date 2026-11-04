/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Server-side keyboard, mouse, touch, and IME injection with the input authority both
//! transports enforce, and the fallback ladders for X11 (XTEST) and Wayland (the compositor's
//! seat, the virtual keyboard, a clipboard paste) (`input_handler.py`, `server_keysym_map.py`,
//! `x11_xkb.py`).
//!
//! Both transports feed every input-channel text message into one [`Dispatcher`], so input
//! authority ([`authority`]) and fallback behavior stay identical across modes. Injection
//! follows ladders that resolve the newest mechanism first and degrade rung by rung, with
//! cooldowns that re-probe the top rung rather than latching into a fallback:
//!
//! - Wayland keyboard: the compositor seat keymap ([`wayland_keymap::WaylandKeymapOwner`]),
//!   then the in-process `zwp_virtual_keyboard` client pixelflux runs, then a data-control
//!   clipboard paste. The Wayland path is subprocess-free by design; never reintroduce wtype,
//!   wl-copy, or similar forks where the in-process pixelflux harness exists.
//! - X11 keyboard: in-process XTEST ([`x11_keyboard::XTestKeyboard`], with a dynamic
//!   spare-keycode overlay for unmapped keysyms and an XKB group lock for keysyms a later
//!   layout group carries), then an xdotool fork for a keysym with no keycode at all.
//!
//! Under a nested app compositor the seat rung only carries keysyms the base layout resolves,
//! so the keyboard worker routes character-bearing keysyms the base lacks onto the
//! virtual-keyboard text batch by classification, not by failure. That batch and the selection
//! are aimed at the auto-detected app compositor: the live `wayland-<N>` socket beside the
//! capture compositor's, never a differently named relay; aiming at the capture compositor
//! instead silently kills every non-ASCII key.
//!
//! A modifier's role comes from the keysym the client resolved for it, never from the engine's
//! flags: the client sends `kd,<keysym>` for Shift, AltGr, or Control as it named them, and the
//! injectors read what is held from those keysyms alone (`LEVEL_MODIFIER_KEYSYMS`,
//! `ACTION_MODIFIER_KEYSYMS`), so no server-side keymap query decides whether a chord picked a
//! character or named a shortcut.
//!
//! Blocking X work runs on a dedicated thread ([`x11_worker`]); compositor round trips and the
//! virtual-keyboard typer run on blocking tasks; the runtime only parses, decides, and awaits,
//! so input never stalls behind a slow display server. The handler's state sits behind one
//! mutex held for the length of a decision and never across an await.
//!
//! The verbs the dispatcher parses: `kd`/`ku`/`kr`/`kh`, `m`/`m2`, `p`, `sf`/`sfe`, `co,end`,
//! `vp`, `_pointer_echo`, `r`, `s`, `vb`, `ab`, `_arg_fps`, `_f`, `_l`,
//! `_stats_video`/`_stats_audio`, `_rc`, `_crf`, `SETTINGS`, `SET_NATIVE_CURSOR_RENDERING`,
//! `REQUEST_KEYFRAME`, `cmd`, `pong`; `js` goes to the [`GamepadSink`] and the clipboard verbs
//! (`cws`, `cbs`, `cwd`, `cbd`, `cwe`, `cbe`, `cw`, `cb`, `cr`, `REQUEST_CLIPBOARD`, `_ebc`) to
//! the [`ClipboardSink`]. A transport answers what is its own before calling
//! [`Dispatcher::receive`]: `_gz`, `START_VIDEO`, `STOP_VIDEO`, a viewer's `SETTINGS`, and the
//! other verbs of its data plane.
//!
//! Off Linux the X server connection (`x11_link`) and the compositor keymap owner
//! (`wayland_keymap`) gate with the display servers they speak to, and twins answer the
//! surface the handler and the dispatcher reach: a connection that does not come up, a
//! keymap with no keys, so both ladders fall through to their last rung exactly as a Linux
//! session without those display servers does. The keyboard shim and the X thread
//! (`x11_keyboard`, `x11_worker`) are written against the `XLink` contract and stay
//! compiled, so their overlays and ordering hold their shape against the twin. The
//! Windows injection milestone (SendInput) lands in the places the twins hold.

pub mod authority;
pub mod command;
pub mod cursor;
pub mod dispatch;
pub mod handler;
pub mod keysym_names;
pub mod keysyms;
pub mod pointer;
pub mod seat;
/// The compositor keymap owner, gated with the compositor it compiles a keymap for.
#[cfg(target_os = "linux")]
pub mod wayland_keymap;
/// The keymap owner's names where no compositor runs, as [`wayland_keymap`] off Linux.
#[cfg(not(target_os = "linux"))]
pub mod wayland_keymap_stub;
#[cfg(not(target_os = "linux"))]
pub use wayland_keymap_stub as wayland_keymap;
pub mod x11_keyboard;
/// The X server connection, gated with the X server it speaks to.
#[cfg(target_os = "linux")]
pub mod x11_link;
/// The X server's names where no X server runs, as [`x11_link`] off Linux.
#[cfg(not(target_os = "linux"))]
pub mod x11_stub;
#[cfg(not(target_os = "linux"))]
pub use x11_stub as x11_link;
pub mod x11_worker;

#[cfg(target_os = "linux")]
#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;

use crate::clipboard::{Clipboard, PasteKeys};
use crate::core::DisplayId;
pub use crate::gamepad::ConnId;
use crate::gamepad::GamepadSlots;
pub use authority::{Admission, Sender, admit, page_put_to_use};
pub use cursor::CursorMessage;
pub use handler::{Dispatcher, InputConfig};
pub use pointer::{Layouts, Rect};
pub use seat::WaylandSeat;

/// What the handler needs from its transport: delivery to clients, the sender of a message,
/// the callbacks the streaming pipeline registers, and the display geometry.
#[async_trait]
pub trait InputHost: Send + Sync {
    /// What the transport knows about a connection, read per message from its live token table
    /// (role, mk token, slot); None for a message the server made itself.
    fn sender(&self, conn: Option<ConnId>) -> Sender;
    /// Send a system action (`command_error,...`, `command_done,...`) to one connection, addressed
    /// as the transport knows it and to nobody once it is gone, or with None to every client.
    fn send_system_action(&self, conn: Option<ConnId>, action: &str);
    /// Deliver a cursor to the clients.
    fn send_cursor(&self, cursor: &CursorMessage);
    /// The displays' rectangles in the combined desktop layout.
    fn display_layouts(&self) -> Layouts;
    /// `r,<w>x<h>`: a resize request for a display, passed through verbatim.
    async fn resize(&self, resolution: &str, display_id: &DisplayId);
    /// `s,<scale>`: a DPI scale for a display.
    async fn scaling_ratio(&self, scale: f64, display_id: &DisplayId);
    /// `SETTINGS,<json>`: the delivering channel's display applies the client's settings.
    async fn update_settings(&self, settings: serde_json::Value, display_id: &DisplayId);
    /// `vb,<kbps>` for a display.
    async fn video_bitrate(&self, kbps: i64, display_id: &DisplayId);
    /// `ab,<kbps>`.
    async fn audio_bitrate(&self, kbps: i64);
    /// `_arg_fps,<fps>` for a display.
    async fn set_fps(&self, fps: f64, display_id: &DisplayId);
    /// `REQUEST_KEYFRAME`, routed by delivering channel like an RTCP PLI.
    async fn request_keyframe(&self, display_id: &DisplayId);
    /// `p,<0|1>` and `SET_NATIVE_CURSOR_RENDERING`: whether the capture composites the cursor.
    async fn pointer_visible(&self, visible: bool);
    /// `_rc,<mode>` for a display, `cbr` or `crf`.
    async fn rate_control_mode(&self, mode: &str, display_id: &DisplayId);
    /// `_crf,<value>` for a display.
    async fn crf(&self, value: i64, display_id: &DisplayId);
    /// `_f,<fps>`: the client's measured frame rate.
    fn client_fps(&self, fps: i64);
    /// `_l,<ms>`: the client's measured latency.
    fn client_latency(&self, ms: i64);
    /// `_stats_video,...` and `_stats_audio,...`: the client's WebRTC statistics.
    async fn client_webrtc_stats(&self, kind: &str, csv: &str);
    /// `pong`: half the ping round trip, in milliseconds.
    fn ping_response(&self, latency_ms: f64);
    /// `vp,<ox>,<oy>,<sx>,<sy>`: where a display's page shows its stream on the user's desktop
    /// and the remote pixels per desktop pixel over it.
    async fn set_client_stream_box(
        &self,
        display_id: &DisplayId,
        origin_x: f64,
        origin_y: f64,
        scale_x: f64,
        scale_y: f64,
    );
    /// A nested app compositor was adopted for input and clipboard: the transport hands it to
    /// `session_screens::SessionScreens::compositor_adopted`, which runs its scale ladder, holds
    /// the spare screens, and tells its pages what a second display can do.
    fn app_compositor_adopted(&self, display: &str);
    /// The session behind the displays changed after they connected (a rootful Xwayland came
    /// up): the pages are told again what a second display can do.
    fn session_screens_changed(&self);
}

/// Where a trackpad page's pointer echoes go.
pub trait PointerSink: Send + Sync {
    /// `pointer,<display>,<x>,<y>,<scale>,<seq>` or `pointer,none`, for one connection.
    fn pointer_echo(&self, conn: ConnId, action: &str);
}

/// The gamepad slots, as the `js,` verb and a departing connection reach them.
pub trait GamepadSink: Send + Sync {
    /// One `js,` message; the slots parse it and hold it to their own gate.
    fn handle(&self, conn: ConnId, msg: &str);
    /// Disassociate every slot `conn` associated (the ungraceful path).
    fn release_conn(&self, conn: ConnId);
    /// Disassociate the one-based `slots` the connection's pads drive.
    fn release_slots(&self, conn: ConnId, slots: &[u8]);
}

impl GamepadSink for GamepadSlots {
    fn handle(&self, conn: ConnId, msg: &str) {
        self.handle_message(conn, msg);
    }

    fn release_conn(&self, conn: ConnId) {
        self.release_for_conn(conn);
    }

    fn release_slots(&self, conn: ConnId, slots: &[u8]) {
        self.release_slots_for_conn(conn, slots);
    }
}

/// The clipboard, as its verbs and the keyboard paths reach it.
#[async_trait]
pub trait ClipboardSink: Send + Sync {
    /// One clipboard verb, already admitted.
    async fn handle(&self, conn: ConnId, msg: &str);
    /// Hold a paste chord (Ctrl+V, Shift+Insert, its modifier held) until the PNG of a client's
    /// image is on the clipboard, bounded.
    async fn paste_hold(&self, keysym: u32);
    /// Type `text` by pasting it through the session clipboard, the chord riding `keys`; true
    /// once the chord is sent.
    async fn paste_text(&self, text: &str, keys: &dyn PasteKeys) -> bool;
}

#[async_trait]
impl ClipboardSink for Arc<Clipboard> {
    async fn handle(&self, conn: ConnId, msg: &str) {
        self.on_message(conn, msg).await;
    }

    async fn paste_hold(&self, keysym: u32) {
        // The dispatcher has already found the chord's modifier held.
        self.paste_waits_for_png(keysym, |_| true).await;
    }

    async fn paste_text(&self, text: &str, keys: &dyn PasteKeys) -> bool {
        self.inject_text_via_clipboard(text, keys).await
    }
}

/// The published input devices (`publish_input_devices`): a copy of each key and pointer event
/// for applications that enumerate evdev instead of the display server.
pub trait VirtualDeviceSink: Send + Sync {
    /// A key by its evdev code (XKB keycode minus eight).
    fn key(&self, evdev_code: u32, down: bool);
    /// A pointer event by evdev type, code, and value; absolute positions are never carried.
    fn pointer(&self, ev_type: u16, code: u16, value: i32);
}

/// A sink that drops everything, for a transport that wires no gamepads or clipboard yet.
#[derive(Debug, Default)]
pub struct NullSink;

impl GamepadSink for NullSink {
    fn handle(&self, _conn: ConnId, _msg: &str) {}
    fn release_conn(&self, _conn: ConnId) {}
    fn release_slots(&self, _conn: ConnId, _slots: &[u8]) {}
}

#[async_trait]
impl ClipboardSink for NullSink {
    async fn handle(&self, _conn: ConnId, _msg: &str) {}
    async fn paste_hold(&self, _keysym: u32) {}
    async fn paste_text(&self, _text: &str, _keys: &dyn PasteKeys) -> bool {
        false
    }
}

impl PointerSink for NullSink {
    fn pointer_echo(&self, _conn: ConnId, _action: &str) {}
}
