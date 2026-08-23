# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""XKEYBOARD protocol link for the X11 XTEST keyboard.

The vendored python-xlib speaks no XKB, and the core keymap it exposes folds
every layout group into one column list: columns 0-1 are group 1, 2-3 group 2,
and from column 4 on the extra levels of groups 1 and 2 and whole further
groups interleave by per-key type widths the core protocol never reports, so
a keysym's (group, level) cannot be told from the core map alone. A handful of
raw XKEYBOARD requests on the injection connection give the XTEST keyboard
what it needs: XkbGetMap names the group and level every keysym sits at,
XkbGetState and XkbLatchLockState read and lock the group an injection must
run under, and XkbSelectEvents keeps keymap changes flowing — the server
withholds core MappingNotify from an XKB-aware client, and reports a
whole-keyboard replacement (setxkbmap) to it only as XkbNewKeyboardNotify.

Everything is best effort: a server without the extension simply leaves the
link unopened and the keyboard falls back to the core keymap.
"""

import logging
import struct
from typing import Any, Dict, Optional, Tuple

from .Xlib.protocol import rq

logger = logging.getLogger("x11_xkb")

XKB_USE_CORE_KBD = 0x0100
XKB_KEY_SYMS_MASK = 1 << 1
XKB_ALL_MAP_PARTS = 0x00FF
XKB_NEW_KEYBOARD_NOTIFY_MASK = 1 << 0
XKB_MAP_NOTIFY_MASK = 1 << 1
XKB_NEW_KEYBOARD_NOTIFY = 0
# Levels 0-3 are what Shift and AltGr reach; deeper levels are not injectable.
XKB_INJECTABLE_LEVELS = 4


class _XkbUseExtension(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'), rq.Opcode(0), rq.RequestLength(),
        rq.Card16('wanted_major'), rq.Card16('wanted_minor'))
    _reply = rq.Struct(
        rq.ReplyCode(), rq.Bool('supported'), rq.Card16('sequence_number'),
        rq.ReplyLength(), rq.Card16('server_major'), rq.Card16('server_minor'),
        rq.Pad(20))


class _XkbSelectEvents(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'), rq.Opcode(1), rq.RequestLength(),
        rq.Card16('device_spec'), rq.Card16('affect_which'), rq.Card16('clear'),
        rq.Card16('select_all'), rq.Card16('affect_map'), rq.Card16('map'))


class _XkbGetState(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'), rq.Opcode(4), rq.RequestLength(),
        rq.Card16('device_spec'), rq.Pad(2))
    _reply = rq.Struct(
        rq.ReplyCode(), rq.Card8('device_id'), rq.Card16('sequence_number'),
        rq.ReplyLength(), rq.Card8('mods'), rq.Card8('base_mods'),
        rq.Card8('latched_mods'), rq.Card8('locked_mods'), rq.Card8('group'),
        rq.Card8('locked_group'), rq.Int16('base_group'), rq.Int16('latched_group'),
        rq.Card8('compat_state'), rq.Card8('grab_mods'), rq.Card8('compat_grab_mods'),
        rq.Card8('lookup_mods'), rq.Card8('compat_lookup_mods'), rq.Pad(1),
        rq.Card16('ptr_btn_state'), rq.Pad(6))


class _XkbLatchLockState(rq.Request):
    _request = rq.Struct(
        rq.Card8('opcode'), rq.Opcode(5), rq.RequestLength(),
        rq.Card16('device_spec'), rq.Card8('affect_mod_locks'), rq.Card8('mod_locks'),
        rq.Bool('lock_group'), rq.Card8('group_lock'), rq.Card8('affect_mod_latches'),
        rq.Card8('mod_latches'), rq.Pad(1), rq.Bool('latch_group'),
        rq.Int16('group_latch'))


class _XkbGetMap(rq.ReplyRequest):
    _request = rq.Struct(
        rq.Card8('opcode'), rq.Opcode(8), rq.RequestLength(),
        rq.Card16('device_spec'), rq.Card16('full'), rq.Card16('partial'),
        rq.Card8('first_type'), rq.Card8('n_types'),
        rq.Card8('first_key_sym'), rq.Card8('n_key_syms'),
        rq.Card8('first_key_action'), rq.Card8('n_key_actions'),
        rq.Card8('first_key_behavior'), rq.Card8('n_key_behaviors'),
        rq.Card16('virtual_mods'),
        rq.Card8('first_key_explicit'), rq.Card8('n_key_explicit'),
        rq.Card8('first_mod_map_key'), rq.Card8('n_mod_map_keys'),
        rq.Card8('first_vmod_map_key'), rq.Card8('n_vmod_map_keys'), rq.Pad(2))
    _reply = rq.Struct(
        rq.ReplyCode(), rq.Card8('device_id'), rq.Card16('sequence_number'),
        rq.ReplyLength(), rq.Pad(2), rq.Card8('min_key_code'), rq.Card8('max_key_code'),
        rq.Card16('present'), rq.Card8('first_type'), rq.Card8('n_types'),
        rq.Card8('total_types'), rq.Card8('first_key_sym'), rq.Card16('total_syms'),
        rq.Card8('n_key_syms'), rq.Card8('first_key_action'), rq.Card16('total_actions'),
        rq.Card8('n_key_actions'), rq.Card8('first_key_behavior'),
        rq.Card8('n_key_behaviors'), rq.Card8('total_key_behaviors'),
        rq.Card8('first_key_explicit'), rq.Card8('n_key_explicit'),
        rq.Card8('total_key_explicit'), rq.Card8('first_mod_map_key'),
        rq.Card8('n_mod_map_keys'), rq.Card8('total_mod_map_keys'),
        rq.Card8('first_vmod_map_key'), rq.Card8('n_vmod_map_keys'),
        rq.Card8('total_vmod_map_keys'), rq.Pad(1), rq.Card16('virtual_mods'),
        rq.Binary('map'))


def _log_x_error(error: Any, request: Any) -> int:
    logger.debug("XKB request failed: %s", error)
    return 0


class XkbLink:
    """One connection's XKEYBOARD session: keysym placement and group control.

    Opened through `open_xkb_link`; every method talks to the server on the
    connection the link was opened on, so a group lock and the XTEST key that
    follows it are processed in order without a round trip between them.
    """

    def __init__(self, xdisplay: Any, major_opcode: int, event_base: int,
                 core_device: int) -> None:
        self._d = xdisplay
        self._opcode = major_opcode
        self.event_base = event_base
        self.core_device = core_device
        # keysym -> (keycode, group, level) at the lowest group, then level,
        # then keycode carrying it; None until a lookup needs it.
        self._placement = None

    def invalidate(self) -> None:
        """Forget the map; the next lookup refetches it from the server."""
        self._placement = None

    def locate(self, keysym: int) -> Optional[Tuple[int, int, int]]:
        """Where the server's keymap puts a keysym.

        Returns:
            `(keycode, group, level)` for the lowest group, then level, then
            keycode carrying the keysym at an injectable level, or None when
            the keymap does not carry it.
        """
        if self._placement is None:
            self._placement = self._fetch_placement()
        return self._placement.get(keysym)

    def _fetch_placement(self) -> Dict[int, Tuple[int, int, int]]:
        """Fetch the XKB symbol map and index it by keysym.

        Only the key symbol maps are requested: each is the key's group count
        and width followed by `width` keysyms per group, which is all the
        placement needs; the levels Shift and AltGr cannot reach are skipped.
        """
        info = self._d.display.info
        lo, hi = info.min_keycode, info.max_keycode
        reply = _XkbGetMap(
            display=self._d.display, opcode=self._opcode, device_spec=XKB_USE_CORE_KBD,
            full=0, partial=XKB_KEY_SYMS_MASK, first_type=0, n_types=0,
            first_key_sym=lo, n_key_syms=hi - lo + 1, first_key_action=0,
            n_key_actions=0, first_key_behavior=0, n_key_behaviors=0, virtual_mods=0,
            first_key_explicit=0, n_key_explicit=0, first_mod_map_key=0,
            n_mod_map_keys=0, first_vmod_map_key=0, n_vmod_map_keys=0)
        blob = bytes(reply.map)
        placement = {}
        offset = 0
        for keycode in range(reply.first_key_sym, reply.first_key_sym + reply.n_key_syms):
            group_info, width, nsyms = struct.unpack_from('=xxxxBBH', blob, offset)
            offset += 8
            syms = struct.unpack_from('=%dI' % nsyms, blob, offset)
            offset += 4 * nsyms
            ngroups = group_info & 0x0F
            for group in range(ngroups):
                for level in range(min(width, XKB_INJECTABLE_LEVELS)):
                    index = group * width + level
                    sym = syms[index] if index < nsyms else 0
                    if not sym:
                        continue
                    have = placement.get(sym)
                    if have is None or (group, level, keycode) < (have[1], have[2], have[0]):
                        placement[sym] = (keycode, group, level)
        return placement

    def locked_group(self) -> int:
        """The group the server currently has locked for the core keyboard."""
        reply = _XkbGetState(display=self._d.display, opcode=self._opcode,
                             device_spec=XKB_USE_CORE_KBD)
        return int(reply.locked_group)

    def lock_group(self, group: int) -> None:
        """Lock the core keyboard's group; queued, not flushed, so the caller's
        next XTEST key follows it in the same write."""
        _XkbLatchLockState(
            display=self._d.display, onerror=_log_x_error, opcode=self._opcode,
            device_spec=XKB_USE_CORE_KBD, affect_mod_locks=0, mod_locks=0,
            lock_group=1, group_lock=group, affect_mod_latches=0, mod_latches=0,
            latch_group=0, group_latch=0)

    def replaced_keyboard(self, event: Any) -> Optional[Tuple[int, int]]:
        """Recognise the XkbNewKeyboardNotify a whole-keyboard replacement sends.

        The server emits one per device; only the core keyboard's counts, so a
        layout switch is handled once.

        Returns:
            `(min_keycode, max_keycode)` of the new keyboard, or None when the
            event is something else.
        """
        if event.type != self.event_base or getattr(event, 'detail', None) != XKB_NEW_KEYBOARD_NOTIFY:
            return None
        data = bytes(event.data)
        if len(data) < 8:
            return None
        _time, device, _old_device, lo, hi = struct.unpack_from('=IBBBB', data, 0)
        if device != self.core_device:
            return None
        return lo, hi


def open_xkb_link(xdisplay: Any) -> Optional[XkbLink]:
    """Open the XKEYBOARD link on a python-xlib display, or None without XKB.

    Registers the connection as XKB-aware and selects the keymap events that
    registration would otherwise silence: XkbNewKeyboardNotify for keyboard
    replacements, and XkbMapNotify, whose selection is what keeps the server
    sending core MappingNotify for partial remaps to this connection.
    """
    try:
        info = xdisplay.query_extension('XKEYBOARD')
        if info is None:
            return None
        opcode = info.major_opcode
        hello = _XkbUseExtension(display=xdisplay.display, opcode=opcode,
                                 wanted_major=1, wanted_minor=0)
        if not hello.supported:
            return None
        # The core keyboard's device id, which the replacement notify (sent
        # once per device) is filtered on.
        state = _XkbGetState(display=xdisplay.display, opcode=opcode,
                             device_spec=XKB_USE_CORE_KBD)
        _XkbSelectEvents(
            display=xdisplay.display, onerror=_log_x_error, opcode=opcode,
            device_spec=XKB_USE_CORE_KBD,
            affect_which=XKB_NEW_KEYBOARD_NOTIFY_MASK | XKB_MAP_NOTIFY_MASK, clear=0,
            select_all=XKB_NEW_KEYBOARD_NOTIFY_MASK,
            affect_map=XKB_ALL_MAP_PARTS, map=XKB_ALL_MAP_PARTS)
        xdisplay.flush()
    except Exception as e:
        logger.debug("XKB link unavailable (%s); core keymap only", e)
        return None
    return XkbLink(xdisplay, opcode, info.first_event, int(state.device_id))
