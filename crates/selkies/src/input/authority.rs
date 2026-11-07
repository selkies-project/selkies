/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Input authority shared by both transports, so a modified client cannot inject input a
//! controller did not grant.
//!
//! A read-only viewer peer (shared or player co-op) may send only `VIEWER_ALLOWED_PREFIXES`; a
//! viewer holding the active mk token while `enable_collab` is on (a read-write collaborator)
//! may additionally send `VIEWER_COLLAB_EXTRA_PREFIXES`, the keyboard, mouse, and clipboard
//! set, including `co,` because IME commits and atomic typing arrive that way. `cmd` and every
//! settings-mutating message stay controller-only. Blur and visibility lifecycle noise and a
//! trackpad page's pointer echo request (`VIEWER_SILENT_DROP_PREFIXES`) from a read-only viewer
//! are normal operation and are dropped without a warning. In secure mode (a master token is
//! set) every peer is additionally held to the token check for input: the active mk-token
//! holder, or a controller-role token while no mk token is provisioned. A `js,` message names
//! its own gamepad index, so `gamepad::slot_denied` decides separately whether its sender holds
//! that slot.

use serde_json::Value as Json;

use crate::core::Role;
use crate::gamepad::slot_denied;

/// What a read-only viewer may send.
pub const VIEWER_ALLOWED_PREFIXES: &[&str] = &[
    "SETTINGS,",
    "START_VIDEO",
    // A viewer must pause its own feed on tab hide, or a broadcast pause can never trigger.
    "STOP_VIDEO",
    "REQUEST_KEYFRAME",
    "js,",
];

/// What a collaborator may send beyond `VIEWER_ALLOWED_PREFIXES`.
pub const VIEWER_COLLAB_EXTRA_PREFIXES: &[&str] = &[
    "kd",
    "ku",
    "kh",
    "kr",
    "m",
    "m2",
    "co,",
    "cws",
    "cbs",
    "cwd",
    "cbd",
    "cwe",
    "cbe",
    "cw",
    "cb",
    "cr",
    "REQUEST_CLIPBOARD",
    "_pointer_echo,",
    "sf",
];

/// A read-only viewer's messages refused without a warning.
pub const VIEWER_SILENT_DROP_PREFIXES: &[&str] = &["kr", "cr", "_pointer_echo"];

/// A person at work through a page: a key pressed, the pointer moved or pressed, a gamepad's
/// button, an IME's text. Key releases, a blur's resets, a gamepad's axes, which a drifting
/// stick or trigger moves on its own, and what a page sends by itself (acks, stats, sizes) are
/// none of it.
pub const PAGE_USE_PREFIXES: &[&str] = &["kd,", "m,", "m2,", "js,b,", "co,"];

/// Whether `msg` starts with any of `prefixes`.
pub fn starts_with_any(msg: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| msg.starts_with(p))
}

/// Whether a page's message is its user acting through it (`PAGE_USE_PREFIXES`), which hands
/// a display to the page in use.
pub fn page_put_to_use(msg: &str) -> bool {
    starts_with_any(msg, PAGE_USE_PREFIXES)
}

/// The verbs secure mode holds to the token check on either transport: what drives the desktop
/// or runs on it, whatever role the connection has.
pub const SECURE_INPUT_VERBS: &[&str] = &["cmd", "co"];

/// What a transport knows about a message's sender, as the gates read it; the host answers it
/// per message (`InputHost::sender`) from its live token table, so a revocation or a re-slot
/// lands on the next message.
#[derive(Clone, Debug, Default)]
pub struct Sender {
    /// The connection's role; unknown reads as a viewer, the fail-safe.
    pub role: Option<Role>,
    /// Whether the sender's token is the active mk token (the collaborator test, before
    /// `enable_collab` is consulted).
    pub holds_mk_token: bool,
    /// Secure mode's token-level input authority: the active mk-token holder, or a
    /// controller-role token while no mk token is provisioned.
    pub input_authorized: bool,
    /// The `slot` entry of the connection's token or the slot it claimed, as
    /// `gamepad::token_slots` reads it; None when it holds none.
    pub slot: Option<Json>,
    /// Whether the connection shares a display it does not own: the display's owner sizes it
    /// and sets its density, so its `r,` and `s,` are dropped.
    pub co_controller: bool,
}

/// What the gates decided about a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The handler may act on it.
    Allowed,
    /// Refused as normal operation; nothing is logged.
    DroppedSilently,
    /// A read-only viewer sent what it may not.
    UnauthorizedViewer,
    /// Secure-mode input from a token without input authority.
    UnauthorizedSecure,
    /// A `js,` message for a slot the sender does not hold.
    GamepadSlot,
}

/// Admit or refuse one client message by the sender's role and tokens.
///
/// Args:
///   msg: The raw message.
///   sender: What the transport knows about the connection.
///   is_secure: Whether a master token is set.
///   collab_enabled: The `enable_collab` setting.
pub fn admit(msg: &str, sender: &Sender, is_secure: bool, collab_enabled: bool) -> Admission {
    let role = sender.role.unwrap_or(Role::Viewer);
    if role == Role::Viewer {
        let collaborator = collab_enabled && sender.holds_mk_token;
        let allowed = starts_with_any(msg, VIEWER_ALLOWED_PREFIXES)
            || (collaborator && starts_with_any(msg, VIEWER_COLLAB_EXTRA_PREFIXES));
        if !allowed {
            return if starts_with_any(msg, VIEWER_SILENT_DROP_PREFIXES) {
                Admission::DroppedSilently
            } else {
                Admission::UnauthorizedViewer
            };
        }
    }
    if is_secure && !sender.input_authorized {
        let verb = msg.split(',').next().unwrap_or("");
        if SECURE_INPUT_VERBS.contains(&verb) || starts_with_any(msg, VIEWER_COLLAB_EXTRA_PREFIXES)
        {
            return Admission::UnauthorizedSecure;
        }
    }
    if slot_denied(msg, role, sender.slot.as_ref(), is_secure) {
        return Admission::GamepadSlot;
    }
    if sender.co_controller && (msg.starts_with("r,") || msg.starts_with("s,")) {
        return Admission::DroppedSilently;
    }
    Admission::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_gates() {
        let viewer = Sender {
            role: Some(Role::Viewer),
            ..Default::default()
        };
        assert_eq!(
            admit("kd,97", &viewer, false, true),
            Admission::UnauthorizedViewer
        );
        assert_eq!(
            admit("kr", &viewer, false, true),
            Admission::DroppedSilently
        );
        assert_eq!(
            admit("cr", &viewer, false, true),
            Admission::DroppedSilently
        );
        assert_eq!(
            admit("_pointer_echo,1", &viewer, false, true),
            Admission::DroppedSilently,
            "a read-only viewer's request is dropped without a warning"
        );
        assert_eq!(
            admit("sf,1,1", &viewer, false, true),
            Admission::UnauthorizedViewer,
            "a read-only viewer may not send a finger's scroll"
        );
        assert_eq!(
            admit("STOP_VIDEO", &viewer, false, true),
            Admission::Allowed
        );
        assert_eq!(
            admit("cmd,ls", &Sender::default(), false, true),
            Admission::UnauthorizedViewer,
            "an unknown role reads as a viewer"
        );
        let collab = Sender {
            role: Some(Role::Viewer),
            holds_mk_token: true,
            input_authorized: true,
            ..Default::default()
        };
        assert_eq!(
            admit("_pointer_echo,1", &collab, false, true),
            Admission::Allowed,
            "a collaborator may ask for the echo"
        );
        assert_eq!(
            admit("sf,1,1", &collab, false, true),
            Admission::Allowed,
            "a collaborator may send a finger's scroll"
        );
        assert_eq!(admit("sfe", &collab, false, true), Admission::Allowed);
        assert_eq!(admit("co,end,x", &collab, false, true), Admission::Allowed);
        assert_eq!(
            admit("sf,1,1", &collab, false, false),
            Admission::UnauthorizedViewer,
            "the extras need enable_collab on"
        );
        assert_eq!(
            admit("cmd,ls", &collab, false, true),
            Admission::UnauthorizedViewer,
            "cmd stays controller-only"
        );
    }

    #[test]
    fn secure_gamepad_and_owner_gates() {
        let controller = Sender {
            role: Some(Role::Controller),
            ..Default::default()
        };
        assert_eq!(
            admit("kd,97", &controller, true, true),
            Admission::UnauthorizedSecure
        );
        assert_eq!(
            admit("cmd,ls", &controller, true, true),
            Admission::UnauthorizedSecure
        );
        assert_eq!(admit("kd,97", &controller, false, true), Admission::Allowed);
        assert_eq!(
            admit("js,b,0,1,1", &controller, false, true),
            Admission::Allowed,
            "a legacy controller is left unrestricted"
        );
        assert_eq!(
            admit("js,b,0,1,1", &controller, true, true),
            Admission::GamepadSlot
        );
        let player = Sender {
            role: Some(Role::Viewer),
            slot: Some(serde_json::json!(2)),
            ..Default::default()
        };
        assert_eq!(admit("js,b,1,1,1", &player, true, true), Admission::Allowed);
        assert_eq!(
            admit("js,b,0,1,1", &player, true, true),
            Admission::GamepadSlot
        );
        let shared = Sender {
            role: Some(Role::Controller),
            co_controller: true,
            ..Default::default()
        };
        assert_eq!(
            admit("r,1920x1080", &shared, false, true),
            Admission::DroppedSilently
        );
        assert_eq!(
            admit("s,1.5", &shared, false, true),
            Admission::DroppedSilently
        );
        assert_eq!(admit("kd,97", &shared, false, true), Admission::Allowed);
        assert!(page_put_to_use("kd,97") && !page_put_to_use("ku,97"));
    }
}
