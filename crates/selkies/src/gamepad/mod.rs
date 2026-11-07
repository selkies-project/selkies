/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Virtual gamepads: four slots applications see as a 'Microsoft X-Box 360 pad', driven by
//! the clients' `js,` messages, with the rumble their applications play relayed back.
//!
//! Each slot ([`pad::Pad`]) serves the Input Interposer's joydev and evdev sockets under the
//! socket directory and, where `/dev/uinput` is writable and the interposer is not
//! preloaded ([`uinput::uinput_gamepads_enabled`]), a kernel uinput device as well; the
//! session's keyboard and pointer can be published the same two ways
//! ([`virtual_device::VirtualInputDevice`]). The slot table ([`GamepadSlots`]) is created
//! once for the process and outlives every transport: applications hold the sockets open
//! across a WebSockets-to-WebRTC switch, so only the client associations and the host are
//! exchanged.
//!
//! A `js,` message names its own gamepad index, so the connection decides which ones it may
//! name ([`slot_denied`]): a slot holder drives the index of each slot it holds
//! (`slot - 1`), a viewer without one drives none, a token without one drives none whatever
//! its role, and a legacy controller, which already holds keyboard and mouse, stays
//! unrestricted. The verbs are `js,c,<idx>,<base64 name>,<axes>,<buttons>` (associate a
//! client pad with the slot), `js,d,<idx>` (release it, neutralizing what it held),
//! `js,b,<idx>,<button>,<value>` and `js,a,<idx>,<axis>,<value>` (one control change), and
//! `js,h,<idx>` (a held-state heartbeat). A slot whose heartbeat stops for
//! [`HEARTBEAT_WINDOW`] while controls are held is neutralized, so a tab that dies
//! mid-press leaves no stuck button; the transport's own disconnect releases the slots a
//! connection held ([`GamepadSlots::release_for_conn`]). Rumble goes to the one client
//! driving the slot as the system action `rumble,<slot>,<strong>,<weak>,<ms>`, through the
//! [`GamepadHost`] the transport wires.

pub mod abi;
pub mod ff;
pub mod mapper;
pub mod pad;
#[cfg(test)]
mod tests;
pub mod uinput;
pub mod virtual_device;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use base64::Engine as _;
use log::{debug, error, info, warn};
use parking_lot::{Mutex, RwLock};
use serde_json::Value as Json;

use crate::core::Role;
pub use pad::Pad;

/// The number of gamepad slots a session has.
pub const GAMEPAD_SLOTS: usize = 4;
/// How long a slot's heartbeat may lapse before what it holds is released.
pub const HEARTBEAT_WINDOW: Duration = Duration::from_secs(2);
/// How often lapsed heartbeats are swept.
pub const HEARTBEAT_SWEEP: Duration = Duration::from_millis(100);

/// A transport's identity for one client connection.
pub type ConnId = u64;

/// What a connection may do with gamepad slots, as the transport knows it.
#[derive(Clone, Debug)]
pub struct SlotAuthority {
    pub role: Role,
    /// The `slot` entry of the connection's token, or the slot it claimed: a number, a list
    /// of numbers, or `None` when it holds none (`token_slots` reads it).
    pub slot: Option<Json>,
    /// Whether a master token is set.
    pub secure: bool,
}

/// What the gamepads need from the transport that carries their clients.
pub trait GamepadHost: Send + Sync {
    /// Send a system action (`rumble,...`) to one connection alone, framed as the transport
    /// frames every system action; nobody once it is gone.
    fn send_system_action(&self, conn: ConnId, action: &str);
    /// The connection's authority over gamepad slots.
    fn slot_authority(&self, conn: ConnId) -> SlotAuthority;
}

/// The one-based gamepad slots a token entry's `slot` names, in its order.
///
/// An entry names one slot (1 to [`GAMEPAD_SLOTS`], or its decimal string), a list of them,
/// or none (null, 0, or an empty list). The order is the page's: its first pad drives the
/// first slot, its second the next. A value that is no such slot names none, and a slot
/// named twice counts once.
pub fn token_slots(value: &Json) -> Vec<u8> {
    let items: Vec<&Json> = match value {
        Json::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    let mut slots = Vec::new();
    for item in items {
        let slot = match item {
            Json::Number(n) => n.as_f64().map(|f| f.trunc() as i64),
            Json::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        if let Some(slot) = slot
            && (1..=GAMEPAD_SLOTS as i64).contains(&slot)
            && !slots.contains(&(slot as u8))
        {
            slots.push(slot as u8);
        }
    }
    slots
}

/// A token entry's `slot` as the table keeps and pages are told it: null for none, the
/// number for one, and the list for several.
pub fn stored_slot(value: &Json) -> Json {
    let slots = token_slots(value);
    match slots.as_slice() {
        [] => Json::Null,
        [one] => Json::from(*one),
        many => Json::from(many.to_vec()),
    }
}

/// Whether a `js,` message drives a gamepad slot its sender does not hold.
///
/// Anything but `js,` is not this gate's business. With no slot, a viewer is refused, and so
/// is anyone while a master token is set; a legacy controller is left unrestricted, since it
/// already holds keyboard and mouse. With a slot, the index the message names must be one
/// of the slots it holds; a malformed or absent index is refused, not read as slot 0, and
/// an index spelled oddly is compared by value.
pub fn slot_denied(msg: &str, role: Role, slot: Option<&Json>, secure: bool) -> bool {
    if !msg.starts_with("js,") {
        return false;
    }
    let Some(slot) = slot.filter(|s| !s.is_null()) else {
        return secure || role != Role::Controller;
    };
    let Some(index) = msg
        .split(',')
        .nth(2)
        .and_then(|t| t.trim().parse::<i64>().ok())
    else {
        return true;
    };
    !token_slots(slot).iter().any(|s| *s as i64 == index + 1)
}

/// What a client told a slot about the pad it drives.
#[derive(Clone, Debug)]
pub struct Association {
    pub client_name: String,
    pub client_num_btns: u32,
    pub client_num_axes: u32,
    pub since: Instant,
    pub conn: ConnId,
}

/// The session's gamepad slots and the clients driving them.
pub struct GamepadSlots {
    socket_dir: PathBuf,
    uinput_enabled: bool,
    pads: Vec<Arc<Pad>>,
    associations: Mutex<HashMap<usize, Association>>,
    heartbeats: Mutex<HashMap<usize, Instant>>,
    host: RwLock<Option<Arc<dyn GamepadHost>>>,
}

impl GamepadSlots {
    /// The slot table, its pads' sockets under `socket_dir`, mirroring onto kernel devices
    /// when `uinput_enabled`; `start` brings the servers up.
    pub fn new(socket_dir: &Path, uinput_enabled: bool) -> Arc<GamepadSlots> {
        Arc::new_cyclic(|this: &Weak<GamepadSlots>| {
            let pads: Vec<Arc<Pad>> = (0..GAMEPAD_SLOTS)
                .map(|i| Pad::new(i, socket_dir, uinput_enabled))
                .collect();
            for pad in &pads {
                let weak = this.clone();
                let index = pad.index();
                pad.set_on_rumble(Some(Arc::new(move |strong, weak_level, ms| {
                    if let Some(slots) = weak.upgrade() {
                        slots.relay_rumble(index, strong, weak_level, ms);
                    }
                })));
            }
            GamepadSlots {
                socket_dir: socket_dir.to_path_buf(),
                uinput_enabled,
                pads,
                associations: Mutex::new(HashMap::new()),
                heartbeats: Mutex::new(HashMap::new()),
                host: RwLock::new(None),
            }
        })
    }

    /// The slot table as the settings configure it: sockets under `js_socket_path`'s
    /// directory, kernel devices as `uinput_gamepad` resolves against this host.
    pub fn from_settings() -> Arc<GamepadSlots> {
        let (dir, mode) = {
            let s = crate::settings::settings();
            (
                crate::settings::socket_dir(s.js_socket_path()),
                s.uinput_gamepad().to_string(),
            )
        };
        GamepadSlots::new(Path::new(&dir), uinput::uinput_gamepads_enabled(&mode))
    }

    /// The transport the clients ride; exchanged on a transport switch.
    pub fn set_host(&self, host: Option<Arc<dyn GamepadHost>>) {
        *self.host.write() = host;
    }

    pub fn pads(&self) -> &[Arc<Pad>] {
        &self.pads
    }

    pub fn pad(&self, index: usize) -> Option<&Arc<Pad>> {
        self.pads.get(index)
    }

    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
    }

    /// Bring every slot's servers up. Pads already running (a transport switch) are kept.
    pub async fn start(&self) {
        if let Err(e) = std::fs::create_dir_all(&self.socket_dir) {
            error!(target: "gamepad", "Failed to create directory {} for gamepad sockets: {e}", self.socket_dir.display());
            return;
        }
        let mut ready = 0;
        for pad in &self.pads {
            if pad.running() || pad.start().await {
                ready += 1;
            }
        }
        info!(target: "gamepad", "{ready} virtual gamepads ready ('{}', sockets under {}); kernel uinput {}.",
            mapper::PAD_NAME, self.socket_dir.display(), if self.uinput_enabled { "on" } else { "off" });
    }

    /// Sweep lapsed heartbeats every [`HEARTBEAT_SWEEP`] until the table is dropped.
    pub fn run_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_SWEEP);
            loop {
                tick.tick().await;
                let Some(slots) = weak.upgrade() else {
                    return;
                };
                slots.sweep_stale(Instant::now());
            }
        })
    }

    /// Neutralize every slot whose heartbeat lapsed while it held controls.
    pub fn sweep_stale(&self, now: Instant) {
        let stale: Vec<usize> = {
            let mut beats = self.heartbeats.lock();
            let stale: Vec<usize> = beats
                .iter()
                .filter(|(_, seen)| now.duration_since(**seen) > HEARTBEAT_WINDOW)
                .map(|(i, _)| *i)
                .collect();
            for idx in &stale {
                beats.remove(idx);
            }
            stale
        };
        for idx in stale {
            if let Some(pad) = self.pads.get(idx)
                && pad.held_count() > 0
            {
                warn!(target: "gamepad", "Neutralizing gamepad slot {idx} (heartbeat lost).");
                pad.reset_state();
            }
        }
    }

    /// Handle one `js,` message from `conn`; false when the message is not one.
    ///
    /// The slot gate runs first, since the index is the client's own claim; a refused
    /// message is dropped with a warning. Nothing is driven while `gamepad_enabled` is off,
    /// whatever the client's own UI state.
    pub fn handle_message(&self, conn: ConnId, msg: &str) -> bool {
        if !msg.starts_with("js,") {
            return false;
        }
        let host = self.host.read().clone();
        if let Some(host) = host.as_ref() {
            let auth = host.slot_authority(conn);
            if slot_denied(msg, auth.role, auth.slot.as_ref(), auth.secure) {
                warn!(target: "gamepad", "DENIED gamepad input from connection {conn}: {} does not match slot {}.",
                    msg.chars().take(32).collect::<String>(), auth.slot.map(|s| s.to_string()).unwrap_or_else(|| "null".into()));
                return true;
            }
        }
        if !crate::settings::settings().gamepad_enabled().on {
            return true;
        }
        let toks: Vec<&str> = msg.split(',').collect();
        let (Some(cmd), Some(index)) = (
            toks.get(1),
            toks.get(2).and_then(|t| t.trim().parse::<i64>().ok()),
        ) else {
            warn!(target: "gamepad", "Malformed joystick message: {}", msg.chars().take(32).collect::<String>());
            return true;
        };
        if !(0..GAMEPAD_SLOTS as i64).contains(&index) {
            error!(target: "gamepad", "Client message for gamepad index {index} is out of range (0-{}).", GAMEPAD_SLOTS - 1);
            return true;
        }
        let index = index as usize;
        let pad = &self.pads[index];
        let num = |at: usize| toks.get(at).and_then(|t| t.trim().parse::<f64>().ok());
        match *cmd {
            "c" => {
                let name = toks
                    .get(3)
                    .and_then(|t| base64::engine::general_purpose::STANDARD.decode(t).ok())
                    .map(|bytes| bytes.iter().map(|b| *b as char).take(255).collect::<String>())
                    .unwrap_or_else(|| {
                        warn!(target: "gamepad", "Error decoding client gamepad name for slot {index}.");
                        format!("ClientGamepad{index}")
                    });
                let axes = num(4).unwrap_or(0.0) as u32;
                let btns = num(5).unwrap_or(0.0) as u32;
                self.connect(index, &name, btns, axes, conn);
            }
            "d" => self.disconnect(Some(index)),
            "b" | "a" => {
                if let (Some(control), Some(value)) = (num(3), num(4)) {
                    pad.send_event(control as u32, value, *cmd == "b");
                }
            }
            "h" => {
                self.heartbeats.lock().insert(index, Instant::now());
            }
            other => {
                warn!(target: "gamepad", "Unhandled joystick command for slot {index}: js {other}")
            }
        }
        true
    }

    /// Associate a client controller with a slot.
    ///
    /// A fresh association starts with no heartbeat, so the previous client's last beat
    /// cannot date-stamp it into an immediate sweep, and the kernel device is brought up
    /// before the first input so applications see a plug event rather than a controller
    /// appearing mid-press. A rumble playing on the slot goes to the new client at once.
    pub fn connect(
        &self,
        index: usize,
        client_name: &str,
        client_num_btns: u32,
        client_num_axes: u32,
        conn: ConnId,
    ) {
        let Some(pad) = self.pads.get(index) else {
            error!(target: "gamepad", "Client association: Gamepad index {index} out of range (0-{}).", GAMEPAD_SLOTS - 1);
            return;
        };
        info!(target: "gamepad",
            "Client controller '{client_name}' ({client_num_btns}b, {client_num_axes}a) is now associated with persistent \
             virtual gamepad slot {index}.");
        self.associations.lock().insert(
            index,
            Association {
                client_name: client_name.to_string(),
                client_num_btns,
                client_num_axes,
                since: Instant::now(),
                conn,
            },
        );
        self.heartbeats.lock().remove(&index);
        let pad = pad.clone();
        if pad.uinput_enabled() && !pad.has_uinput() {
            tokio::task::spawn_blocking(move || {
                pad.ensure_uinput();
                pad.ff_replay();
            });
        } else {
            pad.ff_replay();
        }
    }

    /// Release one slot's association (or every one, with `None`), neutralizing what it
    /// held: a tab closed mid-press would otherwise leave the in-desktop application with a
    /// stuck button or deflected stick until a new client re-sends state.
    pub fn disconnect(&self, index: Option<usize>) {
        let indices: Vec<usize> = match index {
            None => {
                debug!(target: "gamepad", "Disassociating all client gamepads from persistent slots.");
                self.associations.lock().keys().copied().collect()
            }
            Some(i) if i >= GAMEPAD_SLOTS => {
                error!(target: "gamepad", "Client disassociation: Gamepad index {i} out of range.");
                return;
            }
            Some(i) => vec![i],
        };
        for idx in indices {
            self.heartbeats.lock().remove(&idx);
            let removed = self.associations.lock().remove(&idx);
            match removed {
                Some(assoc) => {
                    if let Some(pad) = self.pads.get(idx) {
                        pad.reset_state();
                    }
                    info!(target: "gamepad", "Client controller '{}' disassociated from persistent virtual gamepad slot {idx}.",
                        assoc.client_name);
                }
                None if index.is_some() => warn!(target: "gamepad",
                    "Client disassociation: No active client association found for gamepad slot {idx} to disassociate."),
                None => {}
            }
        }
    }

    /// Release every slot `conn` associated: the ungraceful path, since a tab that dies
    /// mid-press never sends `js,d`, and only the transport knows the connection is gone.
    pub fn release_for_conn(&self, conn: ConnId) {
        let held: Vec<usize> = self
            .associations
            .lock()
            .iter()
            .filter(|(_, a)| a.conn == conn)
            .map(|(i, _)| *i)
            .collect();
        for idx in held {
            self.disconnect(Some(idx));
        }
    }

    /// Release the one-based `slots` this connection's pads drive. A token table that takes
    /// a slot from a connection leaves its page no way to: the `js,d` it sends for the slot
    /// is refused, as the slot is no longer its, and anything it held would stay held until
    /// its heartbeat lapsed.
    pub fn release_slots_for_conn(&self, conn: ConnId, slots: &[u8]) {
        for slot in slots {
            let idx = *slot as usize;
            if idx == 0 {
                continue;
            }
            let held = self
                .associations
                .lock()
                .get(&(idx - 1))
                .is_some_and(|a| a.conn == conn);
            if held {
                self.disconnect(Some(idx - 1));
            }
        }
    }

    /// The client driving each slot.
    pub fn associations(&self) -> HashMap<usize, Association> {
        self.associations.lock().clone()
    }

    /// Whether `index` has a live heartbeat.
    pub fn heartbeat_live(&self, index: usize) -> bool {
        self.heartbeats.lock().contains_key(&index)
    }

    /// Send a pad's rumble to the one client driving its slot; with no client driving it, to
    /// nobody.
    fn relay_rumble(&self, index: usize, strong: f64, weak: f64, duration_ms: u32) {
        let conn = self.associations.lock().get(&index).map(|a| a.conn);
        let host = self.host.read().clone();
        if let (Some(conn), Some(host)) = (conn, host) {
            host.send_system_action(
                conn,
                &format!("rumble,{index},{strong:.3},{weak:.3},{duration_ms}"),
            );
        }
    }

    /// Stop every pad: the servers, their sockets, and the kernel devices. Only process
    /// shutdown calls this; a transport switch keeps the table.
    pub async fn close(&self) {
        self.disconnect(None);
        for pad in &self.pads {
            pad.close().await;
        }
    }
}
